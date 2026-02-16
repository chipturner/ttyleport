# Daemon Mode Design

## Goal

`ttyleport serve <session.sock>` auto-starts a background daemon that manages session lifecycle. Multiple sessions supported, each at its own socket path.

## CLI

```
ttyleport serve <session.sock>               # auto-start daemon, create session
ttyleport serve <session.sock> --foreground  # run daemon in foreground (no fork)
ttyleport connect <session.sock>             # unchanged, direct to session socket
ttyleport list                               # list sessions via control socket
```

## Architecture

```
                          control socket
  ttyleport serve ------+ $XDG_RUNTIME_DIR/ttyleport/ctl.sock
  ttyleport list  ------+          |
                                   v
                              +---------+
                              |  daemon  | (manages sessions)
                              +----+----+
                                   | spawns
                          +--------+--------+
                          v        v        v
                      session1  session2  session3
                      (PTY+UDS) (PTY+UDS) (PTY+UDS)
                          ^
                          |
  ttyleport connect ------+ (direct to session socket)
```

## Control Protocol

Extend Frame with new types:
- `0x10` CreateSession — payload: session socket path (UTF-8)
- `0x11` ListSessions — no payload
- `0x12` SessionInfo — payload: newline-separated session paths
- `0x13` Ok — acknowledgement
- `0x14` Error — payload: error message (UTF-8)

## Daemon Module (src/daemon.rs)

- Listens on control socket
- Accepts control connections (multiple concurrent)
- On CreateSession: spawns tokio task running session logic (current server::run)
- Tracks sessions in HashMap<PathBuf, JoinHandle>
- On ListSessions: responds with active session paths
- Exits when last session ends

## Session Refactor

Current server::run becomes a session runner. Daemon calls it per-session as a tokio task.

## Daemon Lifecycle

- Control socket: `$XDG_RUNTIME_DIR/ttyleport/ctl.sock` (fallback: `/tmp/ttyleport-$UID/ctl.sock`)
- `ttyleport serve` checks for existing daemon, forks one if missing
- `--foreground` runs daemon in current process (for testing/debugging)
- Daemon exits when no sessions remain

## Connect Path

`ttyleport connect` stays unchanged — connects directly to the per-session socket. Does not go through daemon.
