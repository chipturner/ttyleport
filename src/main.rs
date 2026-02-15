use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
    let cli = Cli::parse();
    match cli.command {
        Command::Serve { socket } => todo!("serve on {}", socket.display()),
        Command::Connect { socket } => todo!("connect to {}", socket.display()),
    }
}
