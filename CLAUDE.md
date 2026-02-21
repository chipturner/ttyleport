# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is ttyleport

ttyleport teleports a TTY over a Unix domain socket. Single binary, tmux-like CLI:
- `ttyleport daemon` — starts the daemon in the foreground. Alias: `d`
- `ttyleport new-session` — creates a persistent session and auto-attaches (requires running daemon). Alias: `new`
- `ttyleport new-session -t <name>` — creates a named session and auto-attaches
- `ttyleport attach -t <id|name>` — attaches to a session, detaches other clients. Aliases: `a`, `connect`
- `ttyleport list-sessions` — lists active sessions with id/name/PTY/PID/status. Aliases: `ls`, `list`
- `ttyleport kill-session -t <id|name>` — kills a specific session
- `ttyleport kill-server` — kills daemon and all sessions

Sessions get auto-incrementing integer IDs (0, 1, 2...) with optional human-friendly names via `-t`.

Global option: `--ctl-socket <path>` overrides the default daemon socket path.

Similar to Eternal Terminal but socket-based. Sessions are persistent (shell survives client disconnect). A background daemon manages multiple sessions over a single socket.

## Build & Test

```bash
cargo build
cargo test                           # all tests (70 total)
cargo test --test protocol_test      # codec unit tests only (35)
cargo test --test daemon_test        # daemon integration tests (20)
cargo test --test e2e_test           # e2e session tests (15)
cargo run -- daemon &                 # start daemon in background
cargo run -- new -t myproject        # create named session (requires daemon)
cargo run -- new                     # create unnamed session (id-only)
cargo run -- attach -t myproject     # attach to session by name
cargo run -- attach -t 0             # attach to session by id
cargo run -- ls                      # list active sessions
cargo run -- kill-session -t myproject  # kill session by name
cargo run -- kill-server             # kill daemon
RUST_LOG=debug cargo run -- daemon   # debug mode
tmux start-server\; source-file quicktest.tmux  # manual 2-pane test (server + client)
```

## Architecture

Single-socket architecture: all communication (control AND session relay) goes through one daemon socket. Clients connect, send a control frame declaring intent, daemon routes accordingly.

Five modules behind a lib crate (`src/lib.rs`) with a thin binary entry point (`src/main.rs`):

- **`security`** — Shared security utilities. `secure_create_dir_all` (0700 dirs, ownership validation, symlink rejection). `bind_unix_listener` (TOCTOU-safe stale socket handling, 0600 permissions). `verify_peer_uid` (SO_PEERCRED check). `checked_dup` (returns `OwnedFd`). `clamp_winsize`. All socket/directory creation MUST go through this module.

- **`protocol`** — `Frame` enum with session relay types (Data/Resize/Exit/Detached), control request types (NewSession/Attach/ListSessions/KillSession/KillServer), and control response types (SessionCreated/SessionInfo/Ok/Error). `SessionEntry` struct carries per-session metadata (id, name, pty_path, shell_pid, created_at, attached). Custom tokio-util `Encoder`/`Decoder`. Wire format: `[type: u8][length: u32 BE][payload]`. Session relay: `0x01` Data, `0x02` Resize, `0x03` Exit, `0x04` Detached. Control requests: `0x10` NewSession, `0x11` Attach, `0x12` ListSessions, `0x13` KillSession, `0x14` KillServer. Control responses: `0x20` SessionCreated, `0x21` SessionInfo, `0x22` Ok, `0x23` Error.

- **`daemon`** — Listens on a single socket (`$XDG_RUNTIME_DIR/ttyleport/ctl.sock` or `/tmp/ttyleport-$UID/ctl.sock`). Manages sessions in a `HashMap<u32, SessionState>` where `SessionState` holds `JoinHandle` + `Arc<OnceLock<SessionMetadata>>` + `mpsc::UnboundedSender` for client handoff + optional name. Auto-incrementing `next_id` counter. Session resolution: name match first, then numeric id parse. Handles `NewSession` (allocate id, create channel, spawn server, send SessionCreated, hand off framed connection), `Attach` (resolve session, send Ok, hand off), `ListSessions`, `KillSession`, `KillServer`. Reaps finished sessions before each operation.

- **`server`** — `SessionMetadata` struct with pty_path, shell_pid, created_at, `AtomicBool` attached flag. `ManagedChild` wraps `tokio::process::Child` with process-group cleanup (`killpg(SIGHUP)` on drop). `run()` takes `mpsc::UnboundedReceiver<Framed<UnixStream, FrameCodec>>` + `Arc<OnceLock<SessionMetadata>>`. Receives clients via channel (no per-session socket). Inner relay select includes `client_rx.recv()` for client takeover — new client gets the session, old client receives `Detached` frame.

- **`client`** — `NonBlockGuard` saves/restores stdin's `O_NONBLOCK` flag on drop (prevents breaking parent shell). `RawModeGuard` saves/restores terminal mode. `run()` takes session id/name and initial framed connection. Handles `Detached` frame with `[detached]` message. On unexpected disconnect, prints `[disconnected]` with reconnect command and exits (no auto-reconnect).

## Patterns

- **Single-socket connection handoff**: Daemon reads first frame, routes. For NewSession/Attach, daemon transfers the `Framed<UnixStream>` to the session task via `mpsc` channel. Daemon no longer touches that socket.
- **AsyncFd + try_io**: PTY master and stdin are raw fds wrapped in `AsyncFd`. Reads/writes use `guard.try_io()` with would-block continuation (`Err(_) => continue`).
- **Framed codec**: Socket I/O uses `tokio_util::codec::Framed<UnixStream, FrameCodec>` with `SinkExt`/`StreamExt` from futures-util.
- **PTY lifecycle**: `openpty` then fork with `pre_exec(setsid + TIOCSCTTY)` then drop slave in parent then relay on master. EIO means shell exited.
- **Persistent sessions**: PTY spawns once before the accept loop. Client disconnect breaks the inner relay loop only; outer loop re-accepts via channel. While disconnected, shell blocks on full kernel PTY buffer (~4KB) and resumes on reconnect.
- **Client takeover**: Inner relay loop also selects on `client_rx.recv()`. New client causes `Detached` to be sent to old client, then relay switches to new connection.
- **Process group cleanup**: `ManagedChild` drop sends `SIGHUP` to shell's process group via `killpg`.
- **Terminal state guards**: `RawModeGuard` restores terminal attrs, `NonBlockGuard` restores stdin flags. Drop order ensures `NonBlockGuard` outlives `AsyncFd`.
- **Explicit daemon**: Daemon must be started explicitly via `ttyleport daemon`. `new-session` connects to the running daemon and fails clearly if none is running.
- **Auto-attach**: After `NewSession` succeeds, the same connection transitions to session relay mode (client calls `client::run` with the existing framed connection).
- **SIGWINCH on resize**: Server sends `killpg(SIGWINCH)` via `tcgetpgrp()` (foreground process group, not shell pgid) after every `TIOCSWINSZ`, ensuring foreground apps redraw on attach even when terminal size hasn't changed.
- **Ctrl-L redraw on attach**: Client sends `\x0c` after initial resize to force shell/app redraw. Controlled by `redraw: bool` param to `client::run()`, disabled with `--no-redraw` CLI flag on attach.
- **Session ID resolution**: Given a string, try name match first, then parse as u32 for id match.
- **Security invariants**: Daemon sets `umask(0o077)` at startup. Sockets are 0600, directories 0700. All `accept()` sites verify `SO_PEERCRED` UID. Frame decoder rejects payloads > 1 MB. Resize values clamped to 1..=10000. `/tmp` fallback directories validated for ownership (not symlinks, owned by current uid).

## Current Status

Full CLI with tmux-like ergonomics. Single-socket architecture. All modules implemented and tested (70 tests: 35 protocol codec + 15 e2e session + 20 daemon integration).

## Development Notes

- **`client::run()` signature** — takes `session: &str` + `Framed<UnixStream, FrameCodec>` + `redraw: bool`. Called from `new_session()` (redraw=false) and `attach()` (redraw=!no_redraw) in main.rs.
- **`server::run()` signature** — takes `mpsc::UnboundedReceiver<Framed<UnixStream, FrameCodec>>` + `Arc<OnceLock<SessionMetadata>>`. Called directly by e2e tests (via `UnixStream::pair()` + channel) and spawned by daemon. Changing its signature requires updating both.
- **`Frame` enum changes** — adding variants requires updating: encoder, decoder, protocol tests, and all `match frame` sites in server.rs, client.rs, daemon.rs, main.rs.
- **`SessionInfo` wire format** — 6 tab-separated fields per line: `id\tname\tpty_path\tshell_pid\tcreated_at\tattached`. Changing `SessionEntry` fields requires updating both encoder and decoder in protocol.rs.
- **E2e tests use socketpair + channel** — no socket files needed, no cleanup issues. `UnixStream::pair()` creates both ends, server side sent via `mpsc` channel to `server::run()`.
- **Daemon tests still use a real daemon socket** — each test gets a unique control socket path. Tests clean up sockets manually.
- **Daemon tests are timing-sensitive** — use `tokio::time::sleep` to wait for daemon/session binding. If tests flake, increase sleep durations.
- **`security` module is load-bearing** — all socket binding and directory creation goes through it. Never use `UnixListener::bind` or `create_dir_all` directly.
- **`Stdio::from(OwnedFd)`** — server uses safe `Stdio::from()` instead of `Stdio::from_raw_fd()`. Don't reintroduce `FromRawFd` in server.rs.
- **Reap before lookup** — `reap_sessions()` MUST be called before any operation that resolves a session (Attach, KillSession, ListSessions). Stale sessions in the HashMap cause silent failures (Ok sent, then connection drops because the mpsc channel is closed).
- **Channel closed check** — Before sending `Frame::Ok` for Attach, check `client_tx.is_closed()`. If true, the session died between reap and lookup; send `Frame::Error` instead.
- **Error handling in main.rs** — `main()` returns `()`, delegates to `run() -> anyhow::Result`. Errors print as `error: <message>` via `eprintln!`, no backtraces. Never use `-> anyhow::Result` on `main()` in a CLI tool.
- **Test-first for bug fixes** — When fixing bugs, write a failing test first that reproduces the bug, then implement the fix, then confirm the test passes. Regression tests go in daemon_test.rs (for daemon races) or e2e_test.rs (for session/relay bugs).
