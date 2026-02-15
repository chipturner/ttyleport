use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "ttyleport", about = "Teleport a TTY over a socket")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Listen on a socket, spawn a shell, and relay the PTY
    Serve {
        /// Path to the Unix domain socket
        socket: PathBuf,
    },
    /// Connect to a socket and attach the local terminal
    Connect {
        /// Path to the Unix domain socket
        socket: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve { socket } => ttyleport::server::run(&socket).await,
        Command::Connect { socket } => {
            let code = ttyleport::client::run(&socket).await?;
            std::process::exit(code);
        }
    }
}
