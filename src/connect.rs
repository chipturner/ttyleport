use crate::protocol::{Frame, FrameCodec};
use anyhow::{bail, Context};
use futures_util::{SinkExt, StreamExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio_util::codec::Framed;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Destination parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Destination {
    user: Option<String>,
    host: String,
    port: Option<u16>,
}

impl Destination {
    fn parse(s: &str) -> anyhow::Result<Self> {
        if s.is_empty() {
            bail!("empty destination");
        }

        let (user, remainder) = if let Some(at) = s.find('@') {
            let u = &s[..at];
            if u.is_empty() {
                bail!("empty user in destination: {s}");
            }
            (Some(u.to_string()), &s[at + 1..])
        } else {
            (None, s)
        };

        let (host, port) = if let Some(colon) = remainder.rfind(':') {
            let h = &remainder[..colon];
            let p = remainder[colon + 1..]
                .parse::<u16>()
                .with_context(|| format!("invalid port in destination: {s}"))?;
            (h.to_string(), Some(p))
        } else {
            (remainder.to_string(), None)
        };

        if host.is_empty() {
            bail!("empty host in destination: {s}");
        }

        Ok(Self { user, host, port })
    }

    /// Build the SSH destination string (`user@host` or just `host`).
    fn ssh_dest(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }

    /// Common SSH args for port, if set.
    fn port_args(&self) -> Vec<String> {
        match self.port {
            Some(p) => vec!["-p".to_string(), p.to_string()],
            None => vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// SSH helpers
// ---------------------------------------------------------------------------

/// Hardened SSH options embedded in every tunnel.
const SSH_TUNNEL_OPTS: &[&str] = &[
    "-o", "ServerAliveInterval=3",
    "-o", "ServerAliveCountMax=2",
    "-o", "StreamLocalBindUnlink=yes",
    "-o", "ExitOnForwardFailure=yes",
    "-o", "ConnectTimeout=5",
    "-N", "-T",
];

/// Run a command on the remote host via SSH, returning stdout.
async fn remote_exec(
    dest: &Destination,
    remote_cmd: &str,
    extra_ssh_opts: &[String],
) -> anyhow::Result<String> {
    let mut cmd = Command::new("ssh");
    cmd.args(dest.port_args());
    for opt in extra_ssh_opts {
        cmd.arg("-o").arg(opt);
    }
    cmd.arg("-o").arg("ConnectTimeout=5");
    cmd.arg(dest.ssh_dest());
    cmd.arg(remote_cmd);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::null());

    let output = cmd.output().await.context("failed to run ssh")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.contains("command not found") || stderr.contains("No such file") {
            bail!("gritty not found on remote host (is it in PATH?)");
        }
        bail!("ssh command failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Build the SSH tunnel command with hardened options.
fn tunnel_command(
    dest: &Destination,
    local_sock: &Path,
    remote_sock: &str,
    extra_ssh_opts: &[String],
) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args(dest.port_args());
    cmd.args(SSH_TUNNEL_OPTS);
    for opt in extra_ssh_opts {
        cmd.arg("-o").arg(opt);
    }
    let forward = format!(
        "{}:{}",
        local_sock.display(),
        remote_sock
    );
    cmd.arg("-L").arg(forward);
    cmd.arg(dest.ssh_dest());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::null());
    cmd
}

/// Spawn the SSH tunnel, returning the child process.
async fn spawn_tunnel(
    dest: &Destination,
    local_sock: &Path,
    remote_sock: &str,
    extra_ssh_opts: &[String],
) -> anyhow::Result<Child> {
    let mut cmd = tunnel_command(dest, local_sock, remote_sock, extra_ssh_opts);
    let child = cmd.spawn().context("failed to spawn ssh tunnel")?;
    debug!("ssh tunnel spawned (pid {:?})", child.id());
    Ok(child)
}

/// Poll until the local socket is connectable (200ms interval, 15s timeout).
async fn wait_for_socket(path: &Path) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timeout waiting for SSH tunnel socket at {}", path.display());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Background task: monitor SSH child, respawn on transient failure.
async fn tunnel_monitor(
    mut child: Child,
    dest: Destination,
    local_sock: PathBuf,
    remote_sock: String,
    extra_ssh_opts: Vec<String>,
    stop: tokio_util::sync::CancellationToken,
) {
    let mut exit_times: Vec<Instant> = Vec::new();

    loop {
        tokio::select! {
            _ = stop.cancelled() => {
                let _ = child.kill().await;
                return;
            }
            status = child.wait() => {
                let status = match status {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("failed to wait on ssh tunnel: {e}");
                        return;
                    }
                };

                if stop.is_cancelled() {
                    return;
                }

                let code = status.code();
                debug!("ssh tunnel exited: {:?}", code);

                // Non-transient failure: don't retry
                // SSH exit 255 = connection error (transient). Signal-killed = no code.
                // Everything else (auth failure, config error) = bail.
                if let Some(c) = code
                    && c != 255
                {
                    warn!("ssh tunnel exited with code {c} (not retrying)");
                    return;
                }

                // Rate limit: 5 exits in 10s = give up
                let now = Instant::now();
                exit_times.push(now);
                exit_times.retain(|t| now.duration_since(*t) < Duration::from_secs(10));
                if exit_times.len() >= 5 {
                    warn!("ssh tunnel failing too fast (5 exits in 10s), giving up");
                    return;
                }

                tokio::time::sleep(Duration::from_secs(1)).await;

                if stop.is_cancelled() {
                    return;
                }

                match spawn_tunnel(&dest, &local_sock, &remote_sock, &extra_ssh_opts).await {
                    Ok(new_child) => {
                        info!("ssh tunnel respawned");
                        child = new_child;
                    }
                    Err(e) => {
                        warn!("failed to respawn ssh tunnel: {e}");
                        return;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Remote daemon management
// ---------------------------------------------------------------------------

const REMOTE_ENSURE_CMD: &str = "\
    SOCK=$(gritty socket-path) && \
    (gritty ls >/dev/null 2>&1 || \
     { nohup gritty daemon </dev/null >/dev/null 2>&1 & sleep 0.5; }) && \
    echo \"$SOCK\"";

/// Get the remote socket path and optionally auto-start the daemon.
async fn ensure_remote_ready(
    dest: &Destination,
    no_daemon_start: bool,
    extra_ssh_opts: &[String],
) -> anyhow::Result<String> {
    let remote_cmd = if no_daemon_start {
        "gritty socket-path"
    } else {
        REMOTE_ENSURE_CMD
    };

    let sock_path = remote_exec(dest, remote_cmd, extra_ssh_opts).await?;

    if sock_path.is_empty() {
        bail!("remote host returned empty socket path");
    }

    Ok(sock_path)
}

// ---------------------------------------------------------------------------
// Local socket path
// ---------------------------------------------------------------------------

/// Compute a PID-based local socket path for the tunnel endpoint.
fn local_socket_path() -> PathBuf {
    let pid = std::process::id();
    crate::daemon::socket_dir().join(format!("connect-{pid}.sock"))
}

// ---------------------------------------------------------------------------
// Session negotiation
// ---------------------------------------------------------------------------

/// Connect through the local socket, try Attach then NewSession.
/// Returns (session_label, framed_connection, env_vars).
async fn negotiate_session(
    local_sock: &Path,
    target: Option<&str>,
    force_new: bool,
) -> anyhow::Result<(String, Framed<UnixStream, FrameCodec>, Vec<(String, String)>)> {
    let env_vars = crate::collect_env_vars();

    let session_name = target.unwrap_or_default().to_string();

    // If we have a target and aren't forcing new, try attach first
    if target.is_some() && !force_new {
        let stream = UnixStream::connect(local_sock)
            .await
            .context("failed to connect to tunnel socket")?;
        let mut framed = Framed::new(stream, FrameCodec);

        framed
            .send(Frame::Attach {
                session: session_name.clone(),
            })
            .await?;

        match Frame::expect_from(framed.next().await)? {
            Frame::Ok => {
                eprintln!("[attached]");
                return Ok((session_name, framed, vec![]));
            }
            Frame::Error { message } if message.contains("no such session") => {
                debug!("session not found, will create");
            }
            Frame::Error { message } => bail!("{message}"),
            other => bail!("unexpected response: {other:?}"),
        }
    }

    // Create new session
    let stream = UnixStream::connect(local_sock)
        .await
        .context("failed to connect to tunnel socket")?;
    let mut framed = Framed::new(stream, FrameCodec);

    framed
        .send(Frame::NewSession {
            name: session_name.clone(),
        })
        .await?;

    match Frame::expect_from(framed.next().await)? {
        Frame::SessionCreated { id } => {
            if session_name.is_empty() {
                eprintln!("session created: id {id}");
            } else {
                eprintln!("session created: {session_name} (id {id})");
            }
            Ok((id, framed, env_vars))
        }
        Frame::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Cleanup guard
// ---------------------------------------------------------------------------

struct ConnectGuard {
    child: Option<Child>,
    local_sock: PathBuf,
    stop: tokio_util::sync::CancellationToken,
}

impl Drop for ConnectGuard {
    fn drop(&mut self) {
        self.stop.cancel();

        if let Some(ref mut child) = self.child
            && let Some(pid) = child.id()
        {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }

        let _ = std::fs::remove_file(&self.local_sock);
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct ConnectOpts {
    pub destination: String,
    pub target: Option<String>,
    pub force_new: bool,
    pub list: bool,
    pub no_redraw: bool,
    pub no_escape: bool,
    pub no_daemon_start: bool,
    pub ssh_options: Vec<String>,
}

pub async fn run(opts: ConnectOpts) -> anyhow::Result<i32> {
    let dest = Destination::parse(&opts.destination)?;

    // --ls: list remote sessions and exit
    if opts.list {
        let output = remote_exec(&dest, "gritty ls", &opts.ssh_options).await?;
        println!("{output}");
        return Ok(0);
    }

    // 1. Ensure remote daemon is running and get socket path
    eprintln!("starting remote daemon...");
    let remote_sock = ensure_remote_ready(&dest, opts.no_daemon_start, &opts.ssh_options).await?;
    debug!(remote_sock, "remote socket path");

    // 2. Compute local socket path
    let local_sock = local_socket_path();
    if let Some(parent) = local_sock.parent() {
        crate::security::secure_create_dir_all(parent)?;
    }
    // Remove stale socket if it exists
    let _ = std::fs::remove_file(&local_sock);

    // 3. Spawn SSH tunnel
    let child = spawn_tunnel(&dest, &local_sock, &remote_sock, &opts.ssh_options).await?;
    let stop = tokio_util::sync::CancellationToken::new();

    let mut guard = ConnectGuard {
        child: Some(child),
        local_sock: local_sock.clone(),
        stop: stop.clone(),
    };

    // 4. Wait for local socket to become connectable
    wait_for_socket(&local_sock).await?;

    // 5. Negotiate session (attach-or-create)
    let (session_label, framed, env_vars) =
        negotiate_session(&local_sock, opts.target.as_deref(), opts.force_new).await?;

    // 6. Hand off the child to the tunnel monitor background task
    let original_child = guard.child.take().unwrap();
    tokio::spawn(tunnel_monitor(
        original_child,
        dest,
        local_sock.clone(),
        remote_sock,
        opts.ssh_options,
        stop.clone(),
    ));

    // 7. Run the client relay (reuses existing client::run with auto-reconnect)
    let code = crate::client::run(
        &session_label,
        framed,
        !opts.no_redraw,
        &local_sock,
        env_vars,
        opts.no_escape,
    )
    .await?;

    // 8. Cleanup (guard Drop handles ssh kill + socket removal)
    drop(guard);

    Ok(code)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_destination_user_host() {
        let d = Destination::parse("user@host").unwrap();
        assert_eq!(d.user.as_deref(), Some("user"));
        assert_eq!(d.host, "host");
        assert_eq!(d.port, None);
    }

    #[test]
    fn parse_destination_host_only() {
        let d = Destination::parse("myhost").unwrap();
        assert_eq!(d.user, None);
        assert_eq!(d.host, "myhost");
        assert_eq!(d.port, None);
    }

    #[test]
    fn parse_destination_host_port() {
        let d = Destination::parse("host:2222").unwrap();
        assert_eq!(d.user, None);
        assert_eq!(d.host, "host");
        assert_eq!(d.port, Some(2222));
    }

    #[test]
    fn parse_destination_user_host_port() {
        let d = Destination::parse("user@host:2222").unwrap();
        assert_eq!(d.user.as_deref(), Some("user"));
        assert_eq!(d.host, "host");
        assert_eq!(d.port, Some(2222));
    }

    #[test]
    fn parse_destination_invalid_empty() {
        assert!(Destination::parse("").is_err());
    }

    #[test]
    fn parse_destination_invalid_at_only() {
        assert!(Destination::parse("@host").is_err());
    }

    #[test]
    fn parse_destination_invalid_colon_only() {
        assert!(Destination::parse(":2222").is_err());
    }

    #[test]
    fn tunnel_command_default_opts() {
        let dest = Destination::parse("user@host").unwrap();
        let cmd = tunnel_command(
            &dest,
            Path::new("/tmp/local.sock"),
            "/run/user/1000/gritty/ctl.sock",
            &[],
        );
        let args: Vec<_> = cmd.as_std().get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert!(args.contains(&"ServerAliveInterval=3".to_string()));
        assert!(args.contains(&"StreamLocalBindUnlink=yes".to_string()));
        assert!(args.contains(&"ExitOnForwardFailure=yes".to_string()));
        assert!(args.contains(&"ConnectTimeout=5".to_string()));
        assert!(args.contains(&"-N".to_string()));
        assert!(args.contains(&"-T".to_string()));
        assert!(args.contains(&"/tmp/local.sock:/run/user/1000/gritty/ctl.sock".to_string()));
        assert!(args.contains(&"user@host".to_string()));
    }

    #[test]
    fn tunnel_command_extra_opts() {
        let dest = Destination::parse("host:2222").unwrap();
        let cmd = tunnel_command(
            &dest,
            Path::new("/tmp/local.sock"),
            "/tmp/remote.sock",
            &["ProxyJump=bastion".to_string()],
        );
        let args: Vec<_> = cmd.as_std().get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert!(args.contains(&"ProxyJump=bastion".to_string()));
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"2222".to_string()));
    }

    #[test]
    fn local_socket_path_format() {
        let path = local_socket_path();
        let filename = path.file_name().unwrap().to_string_lossy();
        assert!(filename.starts_with("connect-"));
        assert!(filename.ends_with(".sock"));
        // Parent should be a gritty directory
        let parent = path.parent().unwrap().file_name().unwrap().to_string_lossy();
        assert!(parent.contains("gritty"));
    }

    #[test]
    fn ssh_dest_with_user() {
        let d = Destination::parse("alice@example.com").unwrap();
        assert_eq!(d.ssh_dest(), "alice@example.com");
    }

    #[test]
    fn ssh_dest_without_user() {
        let d = Destination::parse("example.com").unwrap();
        assert_eq!(d.ssh_dest(), "example.com");
    }

    #[test]
    fn port_args_with_port() {
        let d = Destination::parse("host:9999").unwrap();
        assert_eq!(d.port_args(), vec!["-p", "9999"]);
    }

    #[test]
    fn port_args_without_port() {
        let d = Destination::parse("host").unwrap();
        assert!(d.port_args().is_empty());
    }
}
