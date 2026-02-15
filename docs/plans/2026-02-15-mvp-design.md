# ttyleport MVP Design

## Overview

ttyleport teleports a TTY over a Unix domain socket. Single binary, two modes:

- `ttyleport serve <socket-path>` — listen on UDS, accept one client, spawn a PTY with the user's shell, relay frames between socket and PTY
- `ttyleport connect <socket-path>` — connect to UDS, put local terminal in raw mode, relay frames between local stdin/stdout and socket

## Wire Protocol

Length-prefixed binary frames:

```
[type: u8][length: u32 big-endian][payload: length bytes]
```

Message types:
- `0x01` **Data** — terminal data (stdin→server or stdout→client)
- `0x02` **Resize** — payload: `[cols: u16][rows: u16]` (4 bytes)
- `0x03` **Exit** — payload: `[exit_code: i32]` (4 bytes), server→client when shell exits

## Architecture

### Data Flow

```
Client                          Server
┌──────────┐    UDS socket    ┌──────────┐    PTY    ┌───────┐
│ stdin    ├──► Data frame ──►│ pty_write ├─────────►│       │
│          │                  │          │           │ shell │
│ stdout  ◄──  Data frame ◄──┤ pty_read ◄───────── │       │
│          │                  │          │           │       │
│ SIGWINCH ├──► Resize frame►│ pty_resize│──ioctl──►│       │
└──────────┘                  └──────────┘           └───────┘
```

### Server

1. Bind UDS listener at `<socket-path>`
2. Accept one connection
3. `openpty()` to allocate PTY pair
4. Fork/spawn shell process attached to slave PTY
5. Relay loop: read from socket → write to PTY master, read from PTY master → write to socket
6. On Resize frame: `ioctl(TIOCSWINSZ)` on PTY master
7. On shell exit: send Exit frame, close socket, clean up socket file

### Client

1. Save current terminal state
2. Set terminal to raw mode (drop guard restores on any exit, including panic)
3. Connect to UDS at `<socket-path>`
4. Send initial Resize frame with current window size
5. Relay loop: read stdin → Data frame to socket, read socket → write stdout
6. Handle SIGWINCH: send Resize frame
7. On Exit frame or disconnect: restore terminal, exit with received code

## Concurrency

- Single client at a time
- tokio async runtime for socket and PTY I/O

## Dependencies

- `tokio` — async runtime, UDS
- `clap` — CLI parsing
- `nix` — PTY allocation, raw mode, signals, ioctl
- `bytes` + `tokio-util` — frame codec

## Terminal Handling

- Client: drop guard saves/restores termios state, ensures restoration on panic
- Server: `openpty` + spawn, initial window size set from client's first Resize frame
- Clean shutdown on socket disconnect from either side
