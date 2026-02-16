# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is ttyleport

ttyleport teleports a TTY over a Unix domain socket. Single binary, tmux-like CLI:
- `ttyleport new-session -t <name|path>` — creates a persistent session (auto-starts daemon). Aliases: `new`, `serve`
- `ttyleport new-session -t <name|path> --foreground` — runs daemon in foreground (for testing)
- `ttyleport attach -t <name|path>` — attaches to a session, detaches other clients. Aliases: `a`, `connect`
- `ttyleport list-sessions` — lists active sessions with PTY/PID/status. Aliases: `ls`, `list`
- `ttyleport kill-session -t <name|path>` — kills a specific session
- `ttyleport kill-server` — kills daemon and all sessions

Session name resolution: bare names (no `/`) resolve to `$XDG_RUNTIME_DIR/ttyleport/sessions/<name>.sock`, paths used as-is.

Global option: `--ctl-socket <path>` overrides the default control socket path.

Similar to Eternal Terminal but socket-based. Sessions are persistent (shell survives client disconnect). A background daemon manages multiple sessions.

## Build & Test

```bash
cargo build
cargo test                           # all tests (27 total)
cargo test --test protocol_test      # codec unit tests only (18)
cargo test --test daemon_test        # daemon integration tests (4)
cargo test --test e2e_test           # e2e session tests (5)
cargo run -- new -t myproject        # create session (auto-starts daemon)
cargo run -- attach -t myproject     # attach to session
cargo run -- ls                      # list active sessions
cargo run -- kill-session -t myproject  # kill session
cargo run -- kill-server             # kill daemon
RUST_LOG=debug cargo run -- new-session -t test --foreground  # debug mode
tmux start-server\; source-file quicktest.tmux  # manual 3-pane test (server + socat bridge + client)
```

## Architecture

Four modules behind a lib crate (`src/lib.rs`) with a thin binary entry point (`src/main.rs`):

- **`protocol`** — `Frame` enum with session types (Data/Resize/Exit/Detached) and control types (CreateSession/ListSessions/SessionInfo/Ok/Error/KillSession/KillServer). `SessionEntry` struct carries per-session metadata (path, pty_path, shell_pid, created_at, attached). Custom tokio-util `Encoder`/`Decoder`. Wire format: `[type: u8][length: u32 BE][payload]`. Session frame types: `0x01` Data, `0x02` Resize, `0x03` Exit, `0x04` Detached. Control frame types: `0x10` CreateSession, `0x11` ListSessions, `0x12` SessionInfo, `0x13` Ok, `0x14` Error, `0x15` KillSession, `0x16` KillServer.

- **`daemon`** — Listens on a control socket (`$XDG_RUNTIME_DIR/ttyleport/ctl.sock` or `/tmp/ttyleport-$UID/ctl.sock`). Manages sessions in a `HashMap<PathBuf, SessionState>` where `SessionState` holds `JoinHandle` + `Arc<OnceLock<SessionMetadata>>`. Handles `CreateSession`, `ListSessions` (with rich metadata), `KillSession` (aborts task, removes socket), `KillServer` (aborts all, breaks loop). Reaps finished sessions before each operation.

- **`server`** — `SessionMetadata` struct with pty_path, shell_pid, created_at, `AtomicBool` attached flag. `ManagedChild` wraps `tokio::process::Child` with process-group cleanup (`killpg(SIGHUP)` on drop). `run()` takes `Arc<OnceLock<SessionMetadata>>`, fills it after PTY+shell setup. Inner relay select includes `listener.accept()` for client takeover — new client gets the session, old client receives `Detached` frame.

- **`client`** — `NonBlockGuard` saves/restores stdin's `O_NONBLOCK` flag on drop (prevents breaking parent shell). `RawModeGuard` saves/restores terminal mode. Handles `Detached` frame as clean exit (no reconnect). Auto-reconnects on server disconnect.

## Patterns

- **AsyncFd + try_io**: PTY master and stdin are raw fds wrapped in `AsyncFd`. Reads/writes use `guard.try_io()` with would-block continuation (`Err(_) => continue`).
- **Framed codec**: Socket I/O uses `tokio_util::codec::Framed<UnixStream, FrameCodec>` with `SinkExt`/`StreamExt` from futures-util.
- **PTY lifecycle**: `openpty` then fork with `pre_exec(setsid + TIOCSCTTY)` then drop slave in parent then relay on master. EIO means shell exited.
- **Persistent sessions**: PTY spawns once before the accept loop. Client disconnect breaks the inner relay loop only; outer loop re-accepts. While disconnected, shell blocks on full kernel PTY buffer (~4KB) and resumes on reconnect.
- **Client takeover**: Inner relay loop also selects on `listener.accept()`. New client causes `Detached` to be sent to old client, then relay switches to new connection.
- **Process group cleanup**: `ManagedChild` drop sends `SIGHUP` to shell's process group via `killpg`.
- **Terminal state guards**: `RawModeGuard` restores terminal attrs, `NonBlockGuard` restores stdin flags. Drop order ensures `NonBlockGuard` outlives `AsyncFd`.
- **Daemon auto-start**: `new-session` without `--foreground` checks for running daemon, spawns one in a background thread with a separate tokio runtime if needed, then sends `CreateSession` via the control socket.
- **Session name resolution**: Bare names resolve to `$XDG_RUNTIME_DIR/ttyleport/sessions/<name>.sock`.

## Current Status

Full CLI with tmux-like ergonomics. All modules implemented and tested (28 tests: 18 protocol codec + 6 e2e session + 4 daemon integration).

## Development Notes

- **`server::run()` signature** — called directly by e2e tests (`tests/e2e_test.rs`) and spawned by daemon. Changing its signature requires updating both.
- **`Frame` enum changes** — adding variants requires updating: encoder, decoder, protocol tests, and all `match frame` sites in server.rs, client.rs, daemon.rs, main.rs.
- **`SessionInfo` wire format** — tab-separated fields per line. Changing `SessionEntry` fields requires updating both encoder and decoder in protocol.rs.
- **Test socket cleanup** — each test uses unique temp socket paths. Tests clean up sockets manually; leaked sockets cause subsequent test failures.
- **Daemon tests are timing-sensitive** — use `tokio::time::sleep` to wait for daemon/session binding. If tests flake, increase sleep durations.
