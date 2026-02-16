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
    /// Create a persistent session at the given socket path
    Serve {
        /// Path to the session's Unix domain socket
        socket: PathBuf,

        /// Run the daemon in the foreground (don't fork)
        #[arg(long)]
        foreground: bool,
    },
    /// Connect to a session socket and attach the local terminal
    Connect {
        /// Path to the Unix domain socket
        socket: PathBuf,
    },
    /// List active sessions managed by the daemon
    List,
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
        Command::Serve { socket, foreground } => serve(socket, foreground, ctl_path).await,
        Command::Connect { socket } => {
            let code = ttyleport::client::run(&socket).await?;
            std::process::exit(code);
        }
        Command::List => list(ctl_path).await,
    }
}

async fn serve(session_socket: PathBuf, foreground: bool, ctl_path: PathBuf) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;
    use ttyleport::protocol::{Frame, FrameCodec};

    if foreground {
        // Run daemon in foreground — create the session, then run daemon loop
        // Spawn the session creation as a task after daemon starts
        let session = session_socket.clone();
        let ctl_for_spawn = ctl_path.clone();
        tokio::spawn(async move {
            let ctl_path = ctl_for_spawn;
            // Wait for daemon to be ready
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
        // Check if daemon is already running
        let daemon_running = UnixStream::connect(&ctl_path).await.is_ok();

        if !daemon_running {
            // Fork daemon into background
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

            // Wait for daemon to be ready
            for _ in 0..50 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if UnixStream::connect(&ctl_path).await.is_ok() {
                    break;
                }
            }
        }

        // Send CreateSession to daemon
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

async fn list(ctl_path: PathBuf) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;
    use ttyleport::protocol::{Frame, FrameCodec};
    let stream = UnixStream::connect(&ctl_path)
        .await
        .map_err(|_| anyhow::anyhow!("no daemon running (could not connect to {ctl_path:?})"))?;

    let mut framed = Framed::new(stream, FrameCodec);
    framed.send(Frame::ListSessions).await?;

    match framed.next().await {
        Some(Ok(Frame::SessionInfo { sessions })) => {
            if sessions.is_empty() {
                println!("no active sessions");
            } else {
                for s in &sessions {
                    println!("{s}");
                }
            }
            Ok(())
        }
        other => {
            anyhow::bail!("unexpected response from daemon: {other:?}");
        }
    }
}
