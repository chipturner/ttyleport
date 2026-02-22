use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "ttyleport", about = "Teleport a TTY over a socket")]
struct Cli {
    /// Path to the daemon control socket (overrides default)
    #[arg(long, global = true)]
    ctl_socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the daemon (runs in foreground)
    #[command(alias = "d")]
    Daemon,
    /// Create a new persistent session (auto-attaches)
    #[command(alias = "new")]
    NewSession {
        /// Session name (optional; sessions always get an auto-incrementing id)
        #[arg(short = 't', long = "target")]
        target: Option<String>,

        /// Disable escape sequences (~. detach, ~? help, etc.)
        #[arg(long)]
        no_escape: bool,
    },
    /// Attach to an existing session (detaches other clients)
    #[command(alias = "a")]
    Attach {
        /// Session id or name
        #[arg(short = 't', long = "target")]
        target: String,

        /// Don't send Ctrl-L to redraw after attaching
        #[arg(long)]
        no_redraw: bool,

        /// Disable escape sequences (~. detach, ~? help, etc.)
        #[arg(long)]
        no_escape: bool,
    },
    /// List active sessions
    #[command(alias = "ls", alias = "list")]
    ListSessions,
    /// Kill a specific session
    KillSession {
        /// Session id or name
        #[arg(short = 't', long = "target")]
        target: String,
    },
    /// Kill the daemon and all sessions
    KillServer,
    /// Print the default socket path
    #[command(alias = "socket")]
    SocketPath,
    /// Connect to a remote host via SSH tunnel
    #[command(alias = "c")]
    Connect {
        /// Remote destination ([user@]host[:port])
        destination: String,

        /// Session name (attaches if exists, creates if not)
        #[arg(short = 't', long = "target")]
        target: Option<String>,

        /// Force create a new session (error if name already taken)
        #[arg(long)]
        new: bool,

        /// List remote sessions and exit
        #[arg(long)]
        ls: bool,

        /// Don't auto-start remote daemon
        #[arg(long)]
        no_daemon_start: bool,

        /// Don't send Ctrl-L to redraw after attaching
        #[arg(long)]
        no_redraw: bool,

        /// Disable escape sequences (~. detach, ~? help, etc.)
        #[arg(long)]
        no_escape: bool,

        /// Extra SSH options (can be repeated)
        #[arg(long = "ssh-option", short = 'o')]
        ssh_options: Vec<String>,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    if let Err(e) = run().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let ctl_path = cli
        .ctl_socket
        .unwrap_or_else(ttyleport::daemon::control_socket_path);
    match cli.command {
        Command::Daemon => ttyleport::daemon::run(&ctl_path).await,
        Command::NewSession { target, no_escape } => new_session(target, no_escape, ctl_path).await,
        Command::Attach { target, no_redraw, no_escape } => {
            let code = attach(target, !no_redraw, no_escape, ctl_path).await?;
            std::process::exit(code);
        }
        Command::ListSessions => list_sessions(ctl_path).await,
        Command::KillSession { target } => kill_session(target, ctl_path).await,
        Command::KillServer => kill_server(ctl_path).await,
        Command::SocketPath => {
            println!("{}", ctl_path.display());
            Ok(())
        }
        Command::Connect {
            destination,
            target,
            new,
            ls,
            no_daemon_start,
            no_redraw,
            no_escape,
            ssh_options,
        } => {
            let code = ttyleport::connect::run(ttyleport::connect::ConnectOpts {
                destination,
                target,
                force_new: new,
                list: ls,
                no_redraw,
                no_escape,
                no_daemon_start,
                ssh_options,
            })
            .await?;
            std::process::exit(code);
        }
    }
}

async fn new_session(name: Option<String>, no_escape: bool, ctl_path: PathBuf) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;
    use ttyleport::protocol::{Frame, FrameCodec};

    let session_name = name.clone().unwrap_or_default();

    let stream = UnixStream::connect(&ctl_path)
        .await
        .map_err(|_| anyhow::anyhow!("no daemon running (could not connect to {})", ctl_path.display()))?;
    let mut framed = Framed::new(stream, FrameCodec);
    framed
        .send(Frame::NewSession {
            name: session_name,
        })
        .await?;

    match framed.next().await {
        Some(Ok(Frame::SessionCreated { id })) => {
            match &name {
                Some(n) => eprintln!("session created: {n} (id {id})"),
                None => eprintln!("session created: id {id}"),
            }
            let env_vars: Vec<(String, String)> = ["TERM", "LANG", "COLORTERM"]
                .iter()
                .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
                .collect();
            let code = ttyleport::client::run(&id, framed, false, &ctl_path, env_vars, no_escape).await?;
            std::process::exit(code);
        }
        Some(Ok(Frame::Error { message })) => anyhow::bail!("{message}"),
        Some(Err(e)) => anyhow::bail!("daemon protocol error: {e}"),
        None => anyhow::bail!("daemon closed connection (is it still running?)"),
        Some(Ok(other)) => anyhow::bail!("unexpected response from daemon: {other:?}"),
    }
}

async fn attach(target: String, redraw: bool, no_escape: bool, ctl_path: PathBuf) -> anyhow::Result<i32> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;
    use ttyleport::protocol::{Frame, FrameCodec};

    let stream = loop {
        match UnixStream::connect(&ctl_path).await {
            Ok(s) => break s,
            Err(_) => {
                eprintln!("waiting for daemon ({})... ctrl-c to abort", ctl_path.display());
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    };
    let mut framed = Framed::new(stream, FrameCodec);
    framed
        .send(Frame::Attach {
            session: target.clone(),
        })
        .await?;

    match framed.next().await {
        Some(Ok(Frame::Ok)) => {
            eprintln!("[attached]");
            let code = ttyleport::client::run(&target, framed, redraw, &ctl_path, vec![], no_escape).await?;
            Ok(code)
        }
        Some(Ok(Frame::Error { message })) => anyhow::bail!("{message}"),
        Some(Err(e)) => anyhow::bail!("daemon protocol error: {e}"),
        None => anyhow::bail!("daemon closed connection (is it still running?)"),
        Some(Ok(other)) => anyhow::bail!("unexpected response from daemon: {other:?}"),
    }
}

/// Send a control frame to the daemon and return the response.
async fn daemon_request(
    ctl_path: &PathBuf,
    frame: ttyleport::protocol::Frame,
) -> anyhow::Result<ttyleport::protocol::Frame> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;
    use ttyleport::protocol::FrameCodec;

    let stream = UnixStream::connect(ctl_path)
        .await
        .map_err(|_| anyhow::anyhow!("no daemon running (could not connect to {})", ctl_path.display()))?;
    let mut framed = Framed::new(stream, FrameCodec);
    framed.send(frame).await?;
    match framed.next().await {
        Some(Ok(resp)) => Ok(resp),
        Some(Err(e)) => Err(e.into()),
        None => anyhow::bail!("daemon closed connection without response"),
    }
}

async fn list_sessions(ctl_path: PathBuf) -> anyhow::Result<()> {
    use ttyleport::protocol::Frame;

    let resp = daemon_request(&ctl_path, Frame::ListSessions).await?;
    match resp {
        Frame::SessionInfo { sessions } => {
            if sessions.is_empty() {
                println!("no active sessions");
            } else {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                for s in &sessions {
                    let status = if s.attached {
                        if s.last_heartbeat > 0 {
                            let ago = now.saturating_sub(s.last_heartbeat);
                            format!("attached, heartbeat {ago}s ago")
                        } else {
                            "attached".to_string()
                        }
                    } else {
                        "detached".to_string()
                    };
                    let label = if s.name.is_empty() {
                        s.id.clone()
                    } else {
                        format!("{}: {}", s.id, s.name)
                    };
                    if s.shell_pid > 0 {
                        println!("{label} {} (pid {}) ({status})", s.pty_path, s.shell_pid);
                    } else {
                        println!("{label} (starting...)");
                    }
                }
            }
            Ok(())
        }
        other => {
            anyhow::bail!("unexpected response from daemon: {other:?}");
        }
    }
}

async fn kill_session(target: String, ctl_path: PathBuf) -> anyhow::Result<()> {
    use ttyleport::protocol::Frame;

    match daemon_request(
        &ctl_path,
        Frame::KillSession {
            session: target.clone(),
        },
    )
    .await?
    {
        Frame::Ok => {
            eprintln!("session killed: {target}");
            Ok(())
        }
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from daemon: {other:?}"),
    }
}

async fn kill_server(ctl_path: PathBuf) -> anyhow::Result<()> {
    use ttyleport::protocol::Frame;

    match daemon_request(&ctl_path, Frame::KillServer).await? {
        Frame::Ok => {
            eprintln!("server killed");
            Ok(())
        }
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from daemon: {other:?}"),
    }
}
