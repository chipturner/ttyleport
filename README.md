# gritty

A tool for persistent, automatically reconnecting terminal sessions.

SSH sessions die when your connection drops. gritty keeps your shell running on the server and reconnects to it automatically — close your laptop, change networks, lose your VPN, and pick up where you left off.

## Features

- **Persistent sessions** — shells survive client disconnect, network failure, laptop sleep
- **Auto-reconnect** — heartbeat detection with transparent reconnection, no manual intervention
- **SSH tunneling** — one command sets up the tunnel, starts the remote daemon, and forwards the socket
- **Single binary, zero config** — no server config files, no port allocation, no root required
- **No network protocol** — sessions live on Unix domain sockets; SSH handles encryption and auth
- **SSH-style escape sequences** — `~.` detach, `~^Z` suspend, `~?` help
- **Client environment forwarding** — TERM, LANG, COLORTERM propagated to remote shell
- **Multiple named sessions** — create, list, attach, kill by name or auto-assigned ID

## Table of Contents

- [Quick Start](#quick-start)
- [Remote Usage via SSH](#remote-usage-via-ssh)
- [Commands](#commands)
- [Escape Sequences](#escape-sequences)
- [Design](#design)
- [How It Works](#how-it-works)
- [Tips](#tips)
- [Prior Art](#prior-art)
- [Status & Roadmap](#status--roadmap)
- [License](#license)

## Quick Start

```bash
# Build
cargo build --release
cp target/release/gritty ~/.local/bin/  # or somewhere in your PATH

# Start the daemon (self-backgrounds, prints PID)
gritty daemon

# Create a named session (auto-attaches)
gritty new -t work

# Detach with ~. or just close the terminal

# Reattach later
gritty attach -t work

# List sessions
gritty ls

# Clean up
gritty kill-session -t work
gritty kill-server
```

`gritty ls` output:

```
ID  Name   PTY         PID    Created              Attached  Last Heartbeat
0   work   /dev/pts/4  48291  2026-02-21 14:32:07  yes       2026-02-21 14:35:12
1   logs   /dev/pts/5  48305  2026-02-21 14:33:41  no        2026-02-21 14:34:02
```

## Remote Usage via SSH

The real value of gritty is remote sessions that survive network interruptions.

`gritty connect` sets up an SSH tunnel to a remote host, auto-starts the remote daemon if needed, and prints a local socket path. You then use that socket to create and attach to sessions:

```bash
# Terminal 1: start the tunnel (stays running)
gritty connect user@remote-host
# Prints: Socket available at /run/user/1000/gritty/connect-12345.sock
#         Use: gritty new --ctl-socket /run/user/1000/gritty/connect-12345.sock
#         Or:  gritty attach -t <name> --ctl-socket /run/user/1000/gritty/connect-12345.sock

# Terminal 2: create a session through the tunnel
gritty new -t project --ctl-socket /run/user/1000/gritty/connect-12345.sock

# Custom SSH port
gritty connect user@remote-host:2222

# Extra SSH options (repeatable)
gritty connect user@remote-host -o "ProxyJump=bastion"

# Don't auto-start the remote daemon
gritty connect user@remote-host --no-daemon-start
```

Close your laptop, switch networks, lose your SSH tunnel — gritty detects the dead connection via heartbeat, the tunnel monitor respawns SSH, and your client auto-reconnects. Use `~.` to detach cleanly, or Ctrl-C the tunnel process to tear everything down.

### Manual SSH tunnel

If you prefer to manage the tunnel yourself:

```bash
# On the remote host
gritty daemon

# From your laptop: get the socket path and forward it
REMOTE_SOCK=$(ssh user@remote-host gritty socket-path)
ssh -N -T -L /tmp/gritty-remote.sock:$REMOTE_SOCK \
  -o ServerAliveInterval=3 -o ServerAliveCountMax=2 \
  -o StreamLocalBindUnlink=yes \
  -o ExitOnForwardFailure=yes \
  user@remote-host &

# Create or attach through the tunnel
gritty new -t project --ctl-socket /tmp/gritty-remote.sock
gritty attach -t project --ctl-socket /tmp/gritty-remote.sock
```

## Commands

| Command | Aliases | Description |
|---------|---------|-------------|
| `gritty daemon` | `d` | Start the daemon (self-backgrounds by default) |
| `gritty new-session` | `new` | Create a session and auto-attach |
| `gritty attach -t <id\|name>` | `a` | Attach to a session (detaches other clients) |
| `gritty connect user@host` | `c` | SSH tunnel to remote host (prints socket path, stays running) |
| `gritty list-sessions` | `ls`, `list` | List active sessions |
| `gritty kill-session -t <id\|name>` | | Kill a session |
| `gritty kill-server` | | Kill the daemon and all sessions |
| `gritty socket-path` | `socket` | Print the default socket path |

**Options:**
- `-t <name>` on `new-session`/`attach`: session name (or auto-assigned integer ID)
- `--foreground` on `daemon`: run in foreground instead of self-backgrounding
- `--no-daemon-start` on `connect`: don't auto-start remote daemon
- `-o <option>` on `connect`: extra SSH options (repeatable)
- `--no-redraw` on `attach`: skip Ctrl-L redraw after attaching
- `--no-escape` on `new-session`/`attach`: disable `~` escape sequences
- `--ctl-socket <path>` (global): override the daemon socket path

## Escape Sequences

After a newline (or at session start), `~` enters escape mode:

| Sequence | Action |
|----------|--------|
| `~.` | Detach from session (clean exit, no auto-reconnect) |
| `~^Z` | Suspend the client (SIGTSTP) |
| `~?` | Print help |
| `~~` | Send a literal `~` |

Disable with `--no-escape`.

## Design

### No Network Protocol

gritty contains zero networking code. Sessions live on Unix domain sockets. For remote access, you forward the socket over SSH — the same SSH that already handles your keys, your `.ssh/config`, your bastion hosts, your MFA.

This means no ports to open, no firewall rules to maintain, no TLS certificates to manage, and no authentication system to trust beyond the one you already use.

### Security by Composition

gritty delegates encryption and authentication to SSH rather than reimplementing them. Locally, the socket is `0600`, the directory is `0700`, and every `accept()` verifies the peer UID. The attack surface is small because there's very little to attack.

### Single-Socket Architecture

All communication — control messages and session relay — flows through one daemon socket. When a client connects to a session, the daemon hands off the raw connection and gets out of the loop. No per-session sockets, no port allocation, no cleanup races.

### Persistence Model

The PTY and shell process keep running when the client disconnects. While disconnected, the shell blocks on write when its kernel PTY buffer fills up (~4KB) and resumes when a new client drains it. There's no scroll-back replay or screen reconstruction — just a live PTY that never dies.

## How It Works

A background daemon listens on a single Unix domain socket. Each session owns a PTY with a login shell. Communication uses a simple framed protocol: `[type: u8][length: u32 BE][payload]`.

When a client connects, it sends a control frame declaring intent (new session, attach, list, etc.). For session operations, the daemon transfers the socket connection to the session task via an in-process channel — the daemon is out of the loop after that.

The client sends a Ping frame every 5 seconds; the server replies with Pong. If no Pong arrives within 15 seconds, the client treats the connection as dead and enters a reconnect loop — retrying every second until it succeeds or the user hits Ctrl-C.

When a new client attaches to an already-occupied session, the existing client receives a Detached frame and exits cleanly. One session, one client.

## Tips

### Aliases

```bash
alias gn='gritty new -t'
alias ga='gritty attach -t'
alias gl='gritty ls'
alias gk='gritty kill-session -t'
```

### Debugging

```bash
# Run daemon in foreground with debug logging
RUST_LOG=debug gritty daemon --foreground
```

### Reconnect Behavior

When auto-reconnecting, your terminal stays in raw mode. You'll see `[reconnecting...]` and `[reconnected]` messages. Press Ctrl-C to abort the reconnect and exit instead.

If the session is gone (shell exited while you were disconnected), the reconnect will report the error and exit cleanly.

## Prior Art

gritty is inspired by:

- [mosh](https://mosh.org/) — persistent remote terminal using UDP and SSP
- [Eternal Terminal](https://eternalterminal.dev/) — persistent SSH sessions over a custom protocol
- [tmux](https://github.com/tmux/tmux) / [screen](https://www.gnu.org/software/screen/) — terminal multiplexers with session persistence

gritty differs by having no network protocol of its own. Where mosh and ET implement custom transport and encryption, gritty uses Unix domain sockets and delegates networking entirely to SSH. Where tmux and screen are full multiplexers with windows, panes, and key bindings, gritty does one thing: persistent sessions with auto-reconnect.

## Status & Roadmap

Early stage. Works on Linux. Not yet packaged for distribution.

**Planned:**
- **Daemon auto-start** — start the daemon on demand (systemd socket activation, launchd, or on first `new-session`)
- **Zero-downtime upgrades** — daemon re-execs itself with a new binary, preserving sessions and child processes across upgrades
- **Read-only attach** — multiple clients viewing the same session for pair programming or demos

## License

MIT OR Apache-2.0
