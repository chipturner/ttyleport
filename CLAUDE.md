# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is ttyleport

ttyleport teleports a TTY over a Unix domain socket. Single binary, tmux-like CLI:
- `ttyleport daemon` — starts the daemon in the foreground. Alias: `d`
- `ttyleport new-session` — creates a persistent session and auto-attaches (requires running daemon). Alias: `new`
- `ttyleport new-session -t <name>` — creates a named session and auto-attaches
- `ttyleport attach -t <id|name>` — attaches to a session, detaches other clients. Alias: `a`
- `ttyleport connect user@host` — SSH tunnel wrapper: auto-starts remote daemon, sets up tunnel, negotiates session. Alias: `c`
- `ttyleport list-sessions` — lists active sessions with id/name/PTY/PID/status. Aliases: `ls`, `list`
- `ttyleport kill-session -t <id|name>` — kills a specific session
- `ttyleport kill-server` — kills daemon and all sessions

Sessions get auto-incrementing integer IDs (0, 1, 2...) with optional human-friendly names via `-t`.

Global option: `--ctl-socket <path>` overrides the default daemon socket path.

Similar to Eternal Terminal but socket-based. Sessions are persistent (shell survives client disconnect). A background daemon manages multiple sessions over a single socket.

## Build & Test

```bash
cargo build
cargo test                           # all tests (109 total)
cargo test --test protocol_test      # codec unit tests only (39)
cargo test --test daemon_test        # daemon integration tests (21)
cargo test --test e2e_test           # e2e session tests (18)
                                     # + 14 connect unit tests + 17 escape processor tests in lib
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

Six modules behind a lib crate (`src/lib.rs`) with a thin binary entry point (`src/main.rs`):

- **`security`** — Shared security utilities. `secure_create_dir_all` (0700 dirs, ownership validation, symlink rejection). `bind_unix_listener` (TOCTOU-safe stale socket handling, 0600 permissions). `verify_peer_uid` (SO_PEERCRED check). `checked_dup` (returns `OwnedFd`). `clamp_winsize`. All socket/directory creation MUST go through this module.

- **`protocol`** — `Frame` enum with session relay types (Data/Resize/Exit/Detached/Ping/Pong/Env), control request types (NewSession/Attach/ListSessions/KillSession/KillServer), and control response types (SessionCreated/SessionInfo/Ok/Error). `SessionEntry` struct carries per-session metadata (id, name, pty_path, shell_pid, created_at, attached, last_heartbeat). Custom tokio-util `Encoder`/`Decoder`. Wire format: `[type: u8][length: u32 BE][payload]`. Session relay: `0x01` Data, `0x02` Resize, `0x03` Exit, `0x04` Detached, `0x05` Ping, `0x06` Pong, `0x07` Env. Control requests: `0x10` NewSession, `0x11` Attach, `0x12` ListSessions, `0x13` KillSession, `0x14` KillServer. Control responses: `0x20` SessionCreated, `0x21` SessionInfo, `0x22` Ok, `0x23` Error.

- **`daemon`** — Listens on a single socket (`$XDG_RUNTIME_DIR/ttyleport/ctl.sock` or `/tmp/ttyleport-$UID/ctl.sock`). Manages sessions in a `HashMap<u32, SessionState>` where `SessionState` holds `JoinHandle` + `Arc<OnceLock<SessionMetadata>>` + `mpsc::UnboundedSender` for client handoff + optional name. Auto-incrementing `next_id` counter. Session resolution: name match first, then numeric id parse. Handles `NewSession` (allocate id, create channel, spawn server, send SessionCreated, hand off framed connection), `Attach` (resolve session, send Ok, hand off), `ListSessions`, `KillSession`, `KillServer`. Reaps finished sessions before each operation.

- **`server`** — `SessionMetadata` struct with pty_path, shell_pid, created_at, `AtomicBool` attached flag, `AtomicU64` last_heartbeat. `ManagedChild` wraps `tokio::process::Child` with process-group cleanup (`killpg(SIGHUP)` on drop). `run()` takes `mpsc::UnboundedReceiver<Framed<UnixStream, FrameCodec>>` + `Arc<OnceLock<SessionMetadata>>`. Deferred shell spawn: allocates PTY early, waits for first client, reads optional `Env` frame (100ms timeout), then spawns login shell (`-l`) with `CWD=$HOME` and forwarded env vars. Receives clients via channel (no per-session socket). Inner relay select includes `client_rx.recv()` for client takeover — new client gets the session, old client receives `Detached` frame. Replies `Pong` to `Ping` and updates `last_heartbeat` timestamp.

- **`connect`** — SSH tunnel wrapper for remote access. `Destination` struct parses `[user@]host[:port]`. `remote_exec` runs commands on the remote host via SSH. `tunnel_command` builds hardened SSH tunnel commands (ServerAliveInterval, StreamLocalBindUnlink, ExitOnForwardFailure, ConnectTimeout, -N -T). `spawn_tunnel` + `wait_for_socket` (200ms poll, 15s timeout). `tunnel_monitor` background task watches the SSH child and respawns on transient failure (exit 255 = retry; rate limit 5 exits in 10s). `ensure_remote_ready` gets remote socket path and conditionally starts daemon. `negotiate_session` implements attach-or-create: try Attach first, fall back to NewSession on "no such session". `ConnectGuard` (Drop) kills SSH child and removes local socket. `run()` orchestrates: parse destination → ensure remote ready → spawn tunnel → wait for socket → negotiate session → spawn monitor → `client::run()` → cleanup. Local socket at `$XDG_RUNTIME_DIR/ttyleport/connect-{pid}.sock` (or `/tmp` fallback). 14 unit tests.

- **`client`** — `NonBlockGuard` saves/restores stdin's `O_NONBLOCK` flag on drop (prevents breaking parent shell). `RawModeGuard` saves/restores terminal mode. `EscapeProcessor` implements SSH-style `~` escape sequences (detach/suspend/help). `run()` takes session id/name, initial framed connection, redraw flag, `ctl_path` for reconnect, `env_vars: Vec<(String, String)>`, and `no_escape: bool`. On first relay, sends `Env` frame (if non-empty) then `Resize`; on reconnect sends only `Resize` (env_vars cleared). Sends `Ping` every 5s; if no `Pong` within 15s, treats connection as dead. On disconnect/timeout, auto-reconnects via `ctl_path` (connect → Attach → resume relay). Prints `[reconnecting...]` / `[reconnected]` during the loop. Ctrl-C (0x03 in raw mode) exits during reconnect. Handles `Detached` frame with `[detached]` message (no reconnect on detach).

## Patterns

- **Single-socket connection handoff**: Daemon reads first frame, routes. For NewSession/Attach, daemon transfers the `Framed<UnixStream>` to the session task via `mpsc` channel. Daemon no longer touches that socket.
- **AsyncFd + try_io**: PTY master and stdin are raw fds wrapped in `AsyncFd`. Reads/writes use `guard.try_io()` with would-block continuation (`Err(_) => continue`).
- **Framed codec**: Socket I/O uses `tokio_util::codec::Framed<UnixStream, FrameCodec>` with `SinkExt`/`StreamExt` from futures-util.
- **PTY lifecycle**: `openpty` then defer shell spawn until first client connects. Read optional `Env` frame, then fork with `pre_exec(setsid + TIOCSCTTY)` using `-l` (login shell) and `CWD=$HOME`. Drop slave in parent then relay on master. EIO means shell exited.
- **Deferred shell spawn**: PTY is allocated early but shell spawn waits for first client so the server can read the `Env` frame and apply env vars. First client feeds directly into the relay loop (no re-wait in the outer loop).
- **Client environment forwarding**: Client sends `Env` frame with TERM/LANG/COLORTERM before the first `Resize` on new sessions. Server applies these as env vars when spawning the shell. On reconnect/attach, no `Env` frame is sent (shell already running).
- **Persistent sessions**: PTY spawns once after the first client connects. Client disconnect breaks the inner relay loop only; outer loop re-accepts via channel. While disconnected, shell blocks on full kernel PTY buffer (~4KB) and resumes on reconnect.
- **Client takeover**: Inner relay loop also selects on `client_rx.recv()`. New client causes `Detached` to be sent to old client, then relay switches to new connection.
- **Process group cleanup**: `ManagedChild` drop sends `SIGHUP` to shell's process group via `killpg`.
- **Terminal state guards**: `RawModeGuard` restores terminal attrs, `NonBlockGuard` restores stdin flags. Drop order ensures `NonBlockGuard` outlives `AsyncFd`.
- **Explicit daemon**: Daemon must be started explicitly via `ttyleport daemon`. `new-session` connects to the running daemon and fails clearly if none is running.
- **Auto-attach**: After `NewSession` succeeds, the same connection transitions to session relay mode (client calls `client::run` with the existing framed connection).
- **SIGWINCH on resize**: Server sends `killpg(SIGWINCH)` via `tcgetpgrp()` (foreground process group, not shell pgid) after every `TIOCSWINSZ`, ensuring foreground apps redraw on attach even when terminal size hasn't changed.
- **Ctrl-L redraw on attach**: Client sends `\x0c` after initial resize to force shell/app redraw. Controlled by `redraw: bool` param to `client::run()`, disabled with `--no-redraw` CLI flag on attach.
- **Ping/Pong heartbeat**: Client sends `Ping` every 5s, server replies `Pong` immediately and updates `last_heartbeat` in `SessionMetadata`. If client gets no `Pong` within 15s, connection is considered dead and client enters auto-reconnect loop. Zero-payload frames — 5 bytes on wire each.
- **Auto-reconnect**: On heartbeat timeout or disconnect, client loops: sleep 1s → connect to `ctl_path` → send `Attach` → read response. Terminal stays in raw mode throughout. Ctrl-C (0x03) during reconnect exits. On success, relay resumes with `redraw: true`. On `Frame::Error` (session gone), exits cleanly.
- **Session ID resolution**: Given a string, try name match first, then parse as u32 for id match.
- **SSH-style escape sequences**: `~` after newline (or at session start) enters escape mode. `~.` detaches (clean exit, no reconnect), `~^Z` suspends client (SIGTSTP), `~?` prints help, `~~` sends literal `~`. Unrecognized command flushes tilde + byte to server. `EscapeProcessor` state machine with 3 states (Normal/AfterNewline/AfterTilde). Disabled with `--no-escape` CLI flag. 17 unit tests in `client::tests`.
- **SSH tunnel wrapper**: `connect` command handles full remote workflow: SSH to get socket path + ensure daemon, spawn `ssh -N -T -L` tunnel with hardened options, poll until local socket connectable, negotiate session (attach-or-create), hand off to `client::run()` for relay. Tunnel monitor respawns SSH on transient failure (exit 255); non-transient (auth, config) bails. `ConnectGuard` Drop ensures cleanup on all exit paths.
- **Attach-or-create negotiation**: When `-t name` given without `--new`, try Attach first (one round trip for common case). On "no such session" error, open new connection and send NewSession. Uses existing protocol frames — no wire format changes.
- **Security invariants**: Daemon sets `umask(0o077)` at startup. Sockets are 0600, directories 0700. All `accept()` sites verify `SO_PEERCRED` UID. Frame decoder rejects payloads > 1 MB. Resize values clamped to 1..=10000. `/tmp` fallback directories validated for ownership (not symlinks, owned by current uid).

## Current Status

Full CLI with tmux-like ergonomics. Single-socket architecture. Ping/Pong heartbeat with auto-reconnect. Login shell with client environment forwarding. SSH-style escape sequences (`~.` detach, `~^Z` suspend, `~?` help). SSH tunnel wrapper (`ttyleport connect user@host`) for one-command remote access. All modules implemented and tested (109 tests: 17 escape processor + 14 connect + 39 protocol codec + 18 e2e session + 21 daemon integration).

## Development Notes

- **`client::run()` signature** — takes `session: &str` + `Framed<UnixStream, FrameCodec>` + `redraw: bool` + `ctl_path: &Path` + `env_vars: Vec<(String, String)>` + `no_escape: bool`. Called from `new_session()` (redraw=false, env_vars=[TERM,LANG,COLORTERM], no_escape from CLI) and `attach()` (redraw=!no_redraw, env_vars=[], no_escape from CLI) in main.rs. The `ctl_path` enables auto-reconnect.
- **`server::run()` signature** — takes `mpsc::UnboundedReceiver<Framed<UnixStream, FrameCodec>>` + `Arc<OnceLock<SessionMetadata>>`. Called directly by e2e tests (via `UnixStream::pair()` + channel) and spawned by daemon. Changing its signature requires updating both.
- **`Frame` enum changes** — adding variants requires updating: encoder, decoder, protocol tests, and all `match frame` sites in server.rs, client.rs, daemon.rs, main.rs.
- **`SessionInfo` wire format** — 7 tab-separated fields per line: `id\tname\tpty_path\tshell_pid\tcreated_at\tattached\tlast_heartbeat`. Changing `SessionEntry` fields requires updating both encoder and decoder in protocol.rs.
- **E2e tests use socketpair + channel** — no socket files needed, no cleanup issues. `UnixStream::pair()` creates both ends, server side sent via `mpsc` channel to `server::run()`.
- **Daemon tests still use a real daemon socket** — each test gets a unique control socket path. Tests clean up sockets manually.
- **Daemon tests are timing-sensitive** — use `tokio::time::sleep` to wait for daemon/session binding. If tests flake, increase sleep durations.
- **`security` module is load-bearing** — all socket binding and directory creation goes through it. Never use `UnixListener::bind` or `create_dir_all` directly.
- **`Stdio::from(OwnedFd)`** — server uses safe `Stdio::from()` instead of `Stdio::from_raw_fd()`. Don't reintroduce `FromRawFd` in server.rs.
- **Reap before lookup** — `reap_sessions()` MUST be called before any operation that resolves a session (Attach, KillSession, ListSessions). Stale sessions in the HashMap cause silent failures (Ok sent, then connection drops because the mpsc channel is closed).
- **Channel closed check** — Before sending `Frame::Ok` for Attach, check `client_tx.is_closed()`. If true, the session died between reap and lookup; send `Frame::Error` instead.
- **Error handling in main.rs** — `main()` returns `()`, delegates to `run() -> anyhow::Result`. Errors print as `error: <message>` via `eprintln!`, no backtraces. Never use `-> anyhow::Result` on `main()` in a CLI tool.
- **`connect::run()` signature** — takes `ConnectOpts` struct with destination, target, force_new, list, no_redraw, no_escape, no_daemon_start, ssh_options. Returns `i32` exit code. Composes `client::run()` for the relay — does not duplicate relay logic.
- **Connect integration tests need SSH** — unit tests cover parsing and command building. Full integration requires a remote host, so deferred to manual testing.
- **Test-first for bug fixes** — When fixing bugs, write a failing test first that reproduces the bug, then implement the fix, then confirm the test passes. Regression tests go in daemon_test.rs (for daemon races) or e2e_test.rs (for session/relay bugs).
