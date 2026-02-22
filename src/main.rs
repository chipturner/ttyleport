use clap::{Parser, Subcommand};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "gritty", about = "Persistent TTY sessions over Unix domain sockets")]
struct Cli {
    /// Path to the daemon control socket (overrides default)
    #[arg(long, global = true)]
    ctl_socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the daemon (backgrounds by default, use --foreground to stay in foreground)
    #[command(alias = "d")]
    Daemon {
        /// Run in the foreground instead of daemonizing
        #[arg(long, short = 'f')]
        foreground: bool,
    },
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
    /// SSH tunnel to a remote host (prints socket path, stays running)
    #[command(alias = "c")]
    Connect {
        /// Remote destination ([user@]host[:port])
        destination: String,

        /// Don't auto-start remote daemon
        #[arg(long)]
        no_daemon_start: bool,

        /// Extra SSH options (can be repeated)
        #[arg(long = "ssh-option", short = 'o')]
        ssh_options: Vec<String>,
    },
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
}

/// Fork into background, returning the write end of the readiness pipe.
///
/// Parent: blocks reading the pipe. Gets a byte → child is ready (prints PID, exits 0).
/// Gets EOF → child died (exits 1).
/// Child: returns Ok(OwnedFd) for the write end of the pipe.
fn daemonize() -> anyhow::Result<OwnedFd> {
    use nix::unistd::{ForkResult, dup2, fork, pipe, setsid};
    let (read_fd, write_fd) = pipe()?;

    // Safety: fork before any threads (tokio runtime not yet created)
    match unsafe { fork() }? {
        ForkResult::Parent { child } => {
            // Close write end
            drop(write_fd);

            // Read from pipe: one byte = child ready, EOF = child died
            let mut buf = [0u8; 1];
            let mut read_file = std::fs::File::from(read_fd);
            use std::io::Read;
            match read_file.read(&mut buf) {
                Ok(1) => {
                    eprintln!("daemon started (pid {child})");
                    std::process::exit(0);
                }
                _ => {
                    eprintln!("error: daemon failed to start");
                    std::process::exit(1);
                }
            }
        }
        ForkResult::Child => {
            // Close read end
            drop(read_fd);

            // New session, detach from terminal
            setsid()?;

            // Redirect stdin/stdout/stderr to /dev/null
            let devnull = nix::fcntl::open(
                "/dev/null",
                nix::fcntl::OFlag::O_RDWR,
                nix::sys::stat::Mode::empty(),
            )?;
            dup2(devnull, 0)?;
            dup2(devnull, 1)?;
            dup2(devnull, 2)?;
            if devnull > 2 {
                nix::unistd::close(devnull)?;
            }

            Ok(write_fd)
        }
    }
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Daemon { foreground } => {
            let ctl_path = cli
                .ctl_socket
                .unwrap_or_else(gritty::daemon::control_socket_path);

            let ready_fd = if foreground {
                None
            } else {
                match daemonize() {
                    Ok(fd) => Some(fd),
                    Err(e) => {
                        eprintln!("error: failed to daemonize: {e}");
                        std::process::exit(1);
                    }
                }
            };

            // Init tracing AFTER fork (stderr may be /dev/null in daemon mode)
            init_tracing();

            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = rt.block_on(gritty::daemon::run(&ctl_path, ready_fd)) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        _ => {
            init_tracing();
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = rt.block_on(run(cli)) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let ctl_path = cli
        .ctl_socket
        .unwrap_or_else(gritty::daemon::control_socket_path);
    match cli.command {
        Command::Daemon { .. } => unreachable!(),
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
            no_daemon_start,
            ssh_options,
        } => {
            let code = gritty::connect::run(gritty::connect::ConnectOpts {
                destination,
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
    use gritty::protocol::{Frame, FrameCodec};

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

    match Frame::expect_from(framed.next().await)? {
        Frame::SessionCreated { id } => {
            match &name {
                Some(n) => eprintln!("session created: {n} (id {id})"),
                None => eprintln!("session created: id {id}"),
            }
            let env_vars = gritty::collect_env_vars();
            let code = gritty::client::run(&id, framed, false, &ctl_path, env_vars, no_escape).await?;
            std::process::exit(code);
        }
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from daemon: {other:?}"),
    }
}

async fn attach(target: String, redraw: bool, no_escape: bool, ctl_path: PathBuf) -> anyhow::Result<i32> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;
    use gritty::protocol::{Frame, FrameCodec};

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

    match Frame::expect_from(framed.next().await)? {
        Frame::Ok => {
            eprintln!("[attached]");
            let code = gritty::client::run(&target, framed, redraw, &ctl_path, vec![], no_escape).await?;
            Ok(code)
        }
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from daemon: {other:?}"),
    }
}

/// Send a control frame to the daemon and return the response.
async fn daemon_request(
    ctl_path: &PathBuf,
    frame: gritty::protocol::Frame,
) -> anyhow::Result<gritty::protocol::Frame> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;
    use gritty::protocol::{Frame, FrameCodec};

    let stream = UnixStream::connect(ctl_path)
        .await
        .map_err(|_| anyhow::anyhow!("no daemon running (could not connect to {})", ctl_path.display()))?;
    let mut framed = Framed::new(stream, FrameCodec);
    framed.send(frame).await?;
    Frame::expect_from(framed.next().await)
}

async fn list_sessions(ctl_path: PathBuf) -> anyhow::Result<()> {
    use gritty::protocol::Frame;

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
    use gritty::protocol::Frame;

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
    use gritty::protocol::Frame;

    match daemon_request(&ctl_path, Frame::KillServer).await? {
        Frame::Ok => {
            eprintln!("server killed");
            Ok(())
        }
        Frame::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response from daemon: {other:?}"),
    }
}
