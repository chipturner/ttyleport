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

The real value of ttyleport is remote sessions that survive network interruptions.

**On the remote host**, start the daemon:

```bash
ttyleport daemon &
ttyleport new -t project
# detach (Ctrl-C or close terminal)
```

**From your laptop**, forward the socket and attach:

```bash
# Get the remote socket path
REMOTE_SOCK=$(ssh user@remote-host ttyleport socket-path)

# Forward the remote daemon socket to a local path
ssh -L /tmp/ttyleport-remote.sock:$REMOTE_SOCK user@remote-host -N &

# Attach to the remote session via the forwarded socket
ttyleport --ctl-socket /tmp/ttyleport-remote.sock attach -t project
```

Close your laptop, switch networks, reconnect SSH — the client detects the dead connection and auto-reconnects when the tunnel is back.

**Tip:** Use [autossh](https://www.harding.motd.ca/autossh/) to keep the SSH tunnel alive automatically:

```bash
AUTOSSH_GATETIME=0 \
autossh -M 0 \
  -o ServerAliveInterval=3 -o ServerAliveCountMax=2 \
  -o StreamLocalBindUnlink=yes \
  -o ExitOnForwardFailure=yes \
  -o ConnectTimeout=5 \
  -N -T \
  -L /tmp/ttyleport-remote.sock:$REMOTE_SOCK \
  user@remote-host
```

`ServerAliveInterval=3` and `ServerAliveCountMax=2` detect a dead connection in 6 seconds — well under ttyleport's 15-second heartbeat timeout, so most network blips heal transparently. `StreamLocalBindUnlink=yes` removes the stale socket on reconnect so the new tunnel can bind. `AUTOSSH_GATETIME=0` retries immediately on failure.

## Commands

| Command | Aliases | Description |
|---------|---------|-------------|
| `ttyleport daemon` | `d` | Start the daemon (foreground) |
| `ttyleport new-session` | `new` | Create a session and auto-attach |
| `ttyleport attach -t <id\|name>` | `a`, `connect` | Attach to a session (detaches other clients) |
| `ttyleport list-sessions` | `ls`, `list` | List active sessions |
| `ttyleport kill-session -t <id\|name>` | | Kill a session |
| `ttyleport kill-server` | | Kill the daemon and all sessions |
| `ttyleport socket-path` | `socket` | Print the default socket path |

**Options:**
- `-t <name>` on `new-session`: give the session a human-friendly name
- `--no-redraw` on `attach`: skip sending Ctrl-L after attaching
- `--ctl-socket <path>` (global): override the daemon socket path

## How it works

A background daemon manages sessions over a single Unix domain socket. Each session owns a PTY with a shell process. When a client connects, the daemon hands off the socket connection to the session — the daemon is out of the loop after that.

Sessions persist because the PTY and shell keep running when the client disconnects. The shell blocks on write when its PTY buffer fills up (~4KB) and resumes when a new client drains it.

The client sends a Ping frame every 5 seconds. The server replies with Pong. If no Pong arrives within 15 seconds, the client treats the connection as dead and enters a reconnect loop — trying to connect, re-attach, and resume the relay every second until it succeeds or the user hits Ctrl-C.

## Status

Early stage. Works on Linux. Not yet packaged for distribution.

## Roadmap

- **SSH wrapper** — `ttyleport connect user@host` that handles socket forwarding, remote daemon startup, and attach in one command
- **Detach key** — configurable key sequence (e.g. `~.`) to cleanly detach from a session without killing the client
- **Daemon auto-start** — start the daemon on demand (systemd socket activation, launchd, or on first `new-session`)
- **Zero-downtime upgrades** — daemon re-execs itself with a new binary, preserving sessions and child processes across upgrades
- **Read-only attach** — multiple clients viewing the same session for pair programming or demos

## License

MIT OR Apache-2.0
