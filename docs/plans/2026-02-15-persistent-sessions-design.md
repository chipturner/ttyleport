# Persistent Sessions Design

## Goal

Server keeps the PTY/shell alive across client disconnects. Client can reconnect to the same session.

## Architecture

Restructure `server::run` from linear (bind → accept → spawn → relay → exit) to nested loops:

```
bind socket
spawn PTY + shell (once)
loop {                          // outer: accept loop
    accept client connection
    relay loop {                // inner: existing select! loop
        socket ↔ PTY
    }
    // client disconnected — loop back to accept
    // PTY still alive, shell still running
}
// shell exited — clean up socket, done
```

## Key Decisions

- **PTY output while disconnected:** Shell blocks on full kernel PTY buffer (~4KB). Resumes when client reconnects. No application-level buffering. Matches screen/tmux behavior.
- **Client side:** No changes. Reconnecting = running `ttyleport connect` again.
- **Exit semantics:** Client disconnect breaks inner relay loop only. Shell exit (EIO / child.wait) breaks both loops.

## What Changes

- `server::run` — outer accept loop wrapping inner relay loop. PTY spawn before accept loop.
- Socket stays bound between clients. Just re-accept.
- Client Exit frame = disconnect (break inner loop), not server shutdown.

## Testing

1. Connect → disconnect → reconnect: shell still alive, can send commands
2. Shell exits while no client: server cleans up
3. Shell exits during session: Exit frame sent (regression)
