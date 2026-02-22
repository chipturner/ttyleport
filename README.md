# ttyleport

Persistent terminal sessions over Unix domain sockets.

Your shell keeps running when you disconnect. When you reconnect, you pick up exactly where you left off. Heartbeat detection and auto-reconnect make network interruptions transparent — close your laptop, change wifi, lose your SSH tunnel, and ttyleport recovers automatically.

## Why ttyleport?

Tools like [mosh](https://mosh.org/) and [Eternal Terminal](https://eternalterminal.dev/) solve the same problem — persistent sessions over unreliable connections — but they implement their own network protocols, requiring open ports, firewall rules, and custom authentication.

ttyleport takes a different approach:

- **No network protocol.** Sessions live on Unix domain sockets. For remote access, forward the socket over SSH — SSH handles encryption, authentication, and tunneling.
- **Single binary, zero config.** No server config files, no port allocation, no root required.
- **Security by composition.** Instead of reimplementing crypto and auth, ttyleport delegates to SSH, which already does it well.
- **Auto-reconnect with heartbeat.** The client pings every 5 seconds. If the connection dies, it reconnects and resumes the session automatically.

## Quick start

```bash
# Build
cargo build --release

# Start the daemon (runs in foreground; background it or use a second terminal)
ttyleport daemon &

# Create a named session (auto-attaches)
ttyleport new -t work

# Detach: just close the terminal or kill the client

# Reattach later
ttyleport attach -t work

# List sessions
ttyleport ls

# Clean up
ttyleport kill-session -t work
ttyleport kill-server
```

## Remote usage via SSH

The real value of ttyleport is remote sessions that survive network interruptions. One command handles everything — SSH tunnel setup, remote daemon start, session negotiation, and relay:

```bash
# Connect to remote host, create or reattach to a named session
ttyleport connect user@remote-host -t project

# List remote sessions
ttyleport connect user@remote-host --ls

# Force create a new session (error if name exists)
ttyleport connect user@remote-host -t project --new

# Custom SSH port
ttyleport connect user@remote-host:2222 -t project

# Pass extra SSH options
ttyleport connect user@remote-host -t project -o "ProxyJump=bastion"
```

Close your laptop, switch networks, lose your SSH tunnel — ttyleport detects the dead connection, respawns the tunnel, and auto-reconnects. Use `~.` to detach cleanly.

### Manual SSH tunnel (advanced)

If you prefer to manage the tunnel yourself:

```bash
# On the remote host
ttyleport daemon &

# From your laptop: get socket path, forward it, attach
REMOTE_SOCK=$(ssh user@remote-host ttyleport socket-path)
ssh -N -T -L /tmp/ttyleport-remote.sock:$REMOTE_SOCK \
  -o ServerAliveInterval=3 -o ServerAliveCountMax=2 \
  -o StreamLocalBindUnlink=yes \
  -o ExitOnForwardFailure=yes \
  user@remote-host &
ttyleport --ctl-socket /tmp/ttyleport-remote.sock attach -t project
```

## Commands

| Command | Aliases | Description |
|---------|---------|-------------|
| `ttyleport daemon` | `d` | Start the daemon (foreground) |
| `ttyleport new-session` | `new` | Create a session and auto-attach |
| `ttyleport attach -t <id\|name>` | `a` | Attach to a session (detaches other clients) |
| `ttyleport connect user@host` | `c` | Connect to remote host via SSH tunnel |
| `ttyleport list-sessions` | `ls`, `list` | List active sessions |
| `ttyleport kill-session -t <id\|name>` | | Kill a session |
| `ttyleport kill-server` | | Kill the daemon and all sessions |
| `ttyleport socket-path` | `socket` | Print the default socket path |

**Options:**
- `-t <name>` on `new-session`/`attach`/`connect`: session name
- `--new` on `connect`: force create (error if name exists)
- `--ls` on `connect`: list remote sessions and exit
- `--no-daemon-start` on `connect`: don't auto-start remote daemon
- `-o <option>` on `connect`: extra SSH options (repeatable)
- `--no-redraw` on `attach`/`connect`: skip Ctrl-L redraw after attaching
- `--no-escape` on `new-session`/`attach`/`connect`: disable `~` escape sequences
- `--ctl-socket <path>` (global): override the daemon socket path

## How it works

A background daemon manages sessions over a single Unix domain socket. Each session owns a PTY with a shell process. When a client connects, the daemon hands off the socket connection to the session — the daemon is out of the loop after that.

Sessions persist because the PTY and shell keep running when the client disconnects. The shell blocks on write when its PTY buffer fills up (~4KB) and resumes when a new client drains it.

The client sends a Ping frame every 5 seconds. The server replies with Pong. If no Pong arrives within 15 seconds, the client treats the connection as dead and enters a reconnect loop — trying to connect, re-attach, and resume the relay every second until it succeeds or the user hits Ctrl-C.

## Status

Early stage. Works on Linux. Not yet packaged for distribution.

## Roadmap

- **Daemon auto-start** — start the daemon on demand (systemd socket activation, launchd, or on first `new-session`)
- **Zero-downtime upgrades** — daemon re-execs itself with a new binary, preserving sessions and child processes across upgrades
- **Read-only attach** — multiple clients viewing the same session for pair programming or demos

## License

MIT OR Apache-2.0
