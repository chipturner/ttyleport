# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is ttyleport

ttyleport teleports a TTY over a Unix domain socket. Single binary, three subcommands:
- `ttyleport serve <socket-path>` — creates a persistent session (auto-starts daemon if needed)
- `ttyleport serve <socket-path> --foreground` — runs daemon in foreground (for testing)
- `ttyleport connect <socket-path>` — connects to a session socket, enters raw mode, relays stdin/stdout
- `ttyleport list` — lists active sessions managed by the daemon

Global option: `--ctl-socket <path>` overrides the default control socket path.

Similar to Eternal Terminal but socket-based. Sessions are persistent (shell survives client disconnect). A background daemon manages multiple sessions.

## Build & Test

```bash
cargo build
cargo test                           # all tests (21 total)
cargo test --test protocol_test      # codec unit tests only
cargo test --test daemon_test        # daemon integration tests
cargo run -- serve /tmp/test.sock    # create session (auto-starts daemon)
cargo run -- connect /tmp/test.sock  # attach to session
cargo run -- list                    # list active sessions
RUST_LOG=debug cargo run -- serve --foreground /tmp/test.sock  # debug mode
```

## Architecture

Four modules behind a lib crate (`src/lib.rs`) with a thin binary entry point (`src/main.rs`):

- **`protocol`** — `Frame` enum with session types (Data/Resize/Exit) and control types (CreateSession/ListSessions/SessionInfo/Ok/Error). Custom tokio-util `Encoder`/`Decoder`. Wire format: `[type: u8][length: u32 BE][payload]`. Session frame types: `0x01` Data, `0x02` Resize, `0x03` Exit. Control frame types: `0x10` CreateSession, `0x11` ListSessions, `0x12` SessionInfo, `0x13` Ok, `0x14` Error.

- **`daemon`** — Listens on a control socket (`$XDG_RUNTIME_DIR/ttyleport/ctl.sock` or `/tmp/ttyleport-$UID/ctl.sock`). Manages sessions in a `HashMap<PathBuf, JoinHandle>`. Handles `CreateSession` (spawns `server::run` as a tokio task), `ListSessions`, and rejects duplicates. Reaps finished sessions before each operation.

- **`server`** — Binds UDS, accepts one client, allocates PTY via `nix::pty::openpty`, spawns `$SHELL` with `setsid`+`TIOCSCTTY` in `pre_exec`. Wraps PTY master fd in `tokio::io::unix::AsyncFd` (set non-blocking via fcntl). Two nested loops: outer accepts clients (PTY persists), inner is `tokio::select!` over socket frames and PTY reads. `RelayExit` enum distinguishes client disconnect (re-accept) from shell exit (done). Outer loop also selects on `child.wait()` so shell exit during disconnect is detected. Resize frames apply via `ioctl(TIOCSWINSZ)`. PTY EOF produces `EIO` which is treated as clean shutdown.

- **`client`** — Three-function structure: `connect()` retries on ConnectionRefused/NotFound, `relay()` handles stdin/socket I/O returning `Some(code)` on clean exit or `None` on disconnect, `run()` is the outer reconnect loop. Saves/restores terminal via `RawModeGuard` drop guard (`cfmakeraw`/`tcsetattr`), handles `SIGWINCH` as Resize frames via `tokio::signal::unix`. Auto-reconnects on server disconnect.

## Patterns

- **AsyncFd + try_io**: PTY master and stdin are raw fds wrapped in `AsyncFd`. Reads/writes use `guard.try_io()` with would-block continuation (`Err(_) => continue`).
- **Framed codec**: Socket I/O uses `tokio_util::codec::Framed<UnixStream, FrameCodec>` with `SinkExt`/`StreamExt` from futures-util.
- **PTY lifecycle**: `openpty` then fork with `pre_exec(setsid + TIOCSCTTY)` then drop slave in parent then relay on master. EIO means shell exited.
- **Persistent sessions**: PTY spawns once before the accept loop. Client disconnect breaks the inner relay loop only; outer loop re-accepts. While disconnected, shell blocks on full kernel PTY buffer (~4KB) and resumes on reconnect.
- **Daemon auto-start**: `serve` without `--foreground` checks for running daemon, spawns one in a background thread with a separate tokio runtime if needed, then sends `CreateSession` via the control socket.

## Current Status

Daemon mode complete. All modules implemented and tested (21 tests: 14 protocol codec + 5 e2e session + 2 daemon integration).
