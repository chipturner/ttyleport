# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is ttyleport

ttyleport teleports a TTY over a Unix domain socket. Single binary, two subcommands:
- `ttyleport serve <socket-path>` — listens on UDS, spawns a shell on a PTY, relays frames
- `ttyleport connect <socket-path>` — connects to UDS, enters raw mode, relays stdin/stdout

Similar to Eternal Terminal but socket-based. Currently single-client only.

## Build & Test

```bash
cargo build
cargo test                           # all tests
cargo test --test protocol_test      # codec unit tests only
cargo run -- serve /tmp/test.sock    # run server (terminal 1)
cargo run -- connect /tmp/test.sock  # run client (terminal 2)
```

## Architecture

Three modules behind a lib crate (`src/lib.rs`) with a thin binary entry point (`src/main.rs`):

- **`protocol`** — `Frame` enum (Data/Resize/Exit) with a custom tokio-util `Encoder`/`Decoder`. Wire format: `[type: u8][length: u32 BE][payload]`. Frame types: `0x01` Data, `0x02` Resize (4 bytes: cols u16 + rows u16), `0x03` Exit (4 bytes: code i32).

- **`server`** — Binds UDS, accepts one client, allocates PTY via `nix::pty::openpty`, spawns `$SHELL` with `setsid`+`TIOCSCTTY` in `pre_exec`. Wraps PTY master fd in `tokio::io::unix::AsyncFd` (set non-blocking via fcntl). Main loop is `tokio::select!` over socket frames and PTY reads. Resize frames apply via `ioctl(TIOCSWINSZ)`. PTY EOF produces `EIO` which is treated as clean shutdown.

- **`client`** — Connects to UDS, saves/restores terminal via `RawModeGuard` drop guard (`cfmakeraw`/`tcsetattr`), relays stdin to socket, handles `SIGWINCH` as Resize frames via `tokio::signal::unix`.

## Patterns

- **AsyncFd + try_io**: PTY master and stdin are raw fds wrapped in `AsyncFd`. Reads/writes use `guard.try_io()` with would-block continuation (`Err(_) => continue`).
- **Framed codec**: Socket I/O uses `tokio_util::codec::Framed<UnixStream, FrameCodec>` with `SinkExt`/`StreamExt` from futures-util.
- **PTY lifecycle**: `openpty` then fork with `pre_exec(setsid + TIOCSCTTY)` then drop slave in parent then relay on master. EIO means shell exited.

## Current Status

MVP complete. All modules implemented and tested (10 tests: 9 protocol codec + 1 e2e integration).
