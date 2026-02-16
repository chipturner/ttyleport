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
    /// Create a new persistent session
    #[command(alias = "new", alias = "serve")]
    NewSession {
        /// Session name or socket path
        #[arg(short = 't', long = "target")]
        target: String,

        /// Run the daemon in the foreground (don't fork)
        #[arg(long)]
        foreground: bool,
    },
    /// Attach to an existing session (detaches other clients)
    #[command(alias = "a", alias = "connect")]
    Attach {
        /// Session name or socket path
        #[arg(short = 't', long = "target")]
        target: String,
    },
    /// List active sessions
    #[command(alias = "ls", alias = "list")]
    ListSessions,
    /// Kill a specific session
    KillSession {
        /// Session name or socket path
        #[arg(short = 't', long = "target")]
        target: String,
    },
    /// Kill the daemon and all sessions
    KillServer,
}

/// Resolve a session target to a socket path.
/// If the target contains a `/`, treat it as a literal path.
/// Otherwise, resolve to `$XDG_RUNTIME_DIR/ttyleport/sessions/<name>.sock`
/// (or `/tmp/ttyleport-$UID/sessions/<name>.sock`).
fn resolve_target(target: &str) -> PathBuf {
    if target.contains('/') {
        PathBuf::from(target)
    } else {
        let base = if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            PathBuf::from(xdg).join("ttyleport")
        } else {
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/ttyleport-{uid}"))
        };
        base.join("sessions").join(format!("{target}.sock"))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let ctl_path = cli
        .ctl_socket
        .unwrap_or_else(ttyleport::daemon::control_socket_path);
    match cli.command {
        Command::NewSession { target, foreground } => {
            let socket = resolve_target(&target);
            new_session(socket, foreground, ctl_path).await
        }
        Command::Attach { target } => {
            let socket = resolve_target(&target);
            let code = ttyleport::client::run(&socket).await?;
            std::process::exit(code);
        }
        Command::ListSessions => list_sessions(ctl_path).await,
        Command::KillSession { target } => {
            let socket = resolve_target(&target);
            kill_session(socket, ctl_path).await
        }
        Command::KillServer => kill_server(ctl_path).await,
    }
}

async fn new_session(
    session_socket: PathBuf,
    foreground: bool,
    ctl_path: PathBuf,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;
    use ttyleport::protocol::{Frame, FrameCodec};

    // Ensure the sessions directory exists
    if let Some(parent) = session_socket.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if foreground {
        let session = session_socket.clone();
        let ctl_for_spawn = ctl_path.clone();
        tokio::spawn(async move {
            let ctl_path = ctl_for_spawn;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            let Ok(stream) = UnixStream::connect(&ctl_path).await else {
                eprintln!("error: failed to connect to daemon control socket");
                return;
            };
            let mut framed = Framed::new(stream, FrameCodec);
            let _ = framed
                .send(Frame::CreateSession {
                    path: session.display().to_string(),
                })
                .await;
            match framed.next().await {
                Some(Ok(Frame::Ok)) => {
                    eprintln!("session created: {}", session.display());
                }
                Some(Ok(Frame::Error { message })) => {
                    eprintln!("error: {message}");
                }
                other => {
                    eprintln!("unexpected response: {other:?}");
                }
            }
        });

        ttyleport::daemon::run(&ctl_path).await
    } else {
        let daemon_running = UnixStream::connect(&ctl_path).await.is_ok();

        if !daemon_running {
            if let Some(parent) = ctl_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let ctl = ctl_path.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    if let Err(e) = ttyleport::daemon::run(&ctl).await {
                        eprintln!("daemon error: {e}");
                    }
                });
            });

            for _ in 0..50 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if UnixStream::connect(&ctl_path).await.is_ok() {
                    break;
                }
            }
        }

        let stream = UnixStream::connect(&ctl_path).await?;
        let mut framed = Framed::new(stream, FrameCodec);
        framed
            .send(Frame::CreateSession {
                path: session_socket.display().to_string(),
            })
            .await?;

        match framed.next().await {
            Some(Ok(Frame::Ok)) => {
                eprintln!("session created: {}", session_socket.display());
                Ok(())
            }
            Some(Ok(Frame::Error { message })) => {
                anyhow::bail!("{message}");
            }
            other => {
                anyhow::bail!("unexpected response from daemon: {other:?}");
            }
        }
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
        .map_err(|_| anyhow::anyhow!("no daemon running (could not connect to {ctl_path:?})"))?;
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

    match daemon_request(&ctl_path, Frame::ListSessions).await? {
        Frame::SessionInfo { sessions } => {
            if sessions.is_empty() {
                println!("no active sessions");
            } else {
                for s in &sessions {
                    let status = if s.attached { "attached" } else { "detached" };
                    if s.shell_pid > 0 {
                        println!(
                            "{}: {} (pid {}) ({})",
                            s.path, s.pty_path, s.shell_pid, status
                        );
                    } else {
                        println!("{}: (starting...)", s.path);
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

async fn kill_session(socket: PathBuf, ctl_path: PathBuf) -> anyhow::Result<()> {
    use ttyleport::protocol::Frame;

    match daemon_request(
        &ctl_path,
        Frame::KillSession {
            path: socket.display().to_string(),
        },
    )
    .await?
    {
        Frame::Ok => {
            eprintln!("session killed: {}", socket.display());
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
