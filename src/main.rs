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
    /// Create a new persistent session (auto-attaches)
    #[command(alias = "new", alias = "serve")]
    NewSession {
        /// Session name (optional; sessions always get an auto-incrementing id)
        #[arg(short = 't', long = "target")]
        target: Option<String>,

        /// Run the daemon in the foreground (don't fork)
        #[arg(long)]
        foreground: bool,
    },
    /// Attach to an existing session (detaches other clients)
    #[command(alias = "a", alias = "connect")]
    Attach {
        /// Session id or name
        #[arg(short = 't', long = "target")]
        target: String,
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
            new_session(target, foreground, ctl_path).await
        }
        Command::Attach { target } => {
            let code = attach(target, ctl_path).await?;
            std::process::exit(code);
        }
        Command::ListSessions => list_sessions(ctl_path).await,
        Command::KillSession { target } => kill_session(target, ctl_path).await,
        Command::KillServer => kill_server(ctl_path).await,
    }
}

async fn new_session(
    name: Option<String>,
    foreground: bool,
    ctl_path: PathBuf,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;
    use ttyleport::protocol::{Frame, FrameCodec};

    let session_name = name.clone().unwrap_or_default();

    if foreground {
        let ctl_for_spawn = ctl_path.clone();
        let name_for_spawn = session_name.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            let Ok(stream) = UnixStream::connect(&ctl_for_spawn).await else {
                eprintln!("error: failed to connect to daemon control socket");
                return;
            };
            let mut framed = Framed::new(stream, FrameCodec);
            let _ = framed
                .send(Frame::NewSession {
                    name: name_for_spawn,
                })
                .await;
            match framed.next().await {
                Some(Ok(Frame::SessionCreated { id })) => {
                    eprintln!("session created: id {id}");
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
                ttyleport::security::secure_create_dir_all(parent)?;
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
            .send(Frame::NewSession {
                name: session_name,
            })
            .await?;

        match framed.next().await {
            Some(Ok(Frame::SessionCreated { id })) => {
                eprintln!("session created: id {id}");
                // Auto-attach: the daemon already handed off our connection to the session
                let code = ttyleport::client::run(&ctl_path, &id, framed).await?;
                std::process::exit(code);
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

async fn attach(target: String, ctl_path: PathBuf) -> anyhow::Result<i32> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;
    use ttyleport::protocol::{Frame, FrameCodec};

    let stream = UnixStream::connect(&ctl_path)
        .await
        .map_err(|_| anyhow::anyhow!("no daemon running (could not connect to {ctl_path:?})"))?;
    let mut framed = Framed::new(stream, FrameCodec);
    framed
        .send(Frame::Attach {
            session: target.clone(),
        })
        .await?;

    match framed.next().await {
        Some(Ok(Frame::Ok)) => {
            let code = ttyleport::client::run(&ctl_path, &target, framed).await?;
            Ok(code)
        }
        Some(Ok(Frame::Error { message })) => {
            anyhow::bail!("{message}");
        }
        other => {
            anyhow::bail!("unexpected response from daemon: {other:?}");
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
                    let label = if s.name.is_empty() {
                        s.id.clone()
                    } else {
                        format!("{}: {}", s.id, s.name)
                    };
                    if s.shell_pid > 0 {
                        println!("{label}: {} (pid {}) ({status})", s.pty_path, s.shell_pid);
                    } else {
                        println!("{label}: (starting...)");
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
