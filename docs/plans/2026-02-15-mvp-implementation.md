# ttyleport MVP Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a single Rust binary that relays a TTY over a Unix domain socket with two modes: `serve` (spawn shell + listen) and `connect` (attach terminal + connect).

**Architecture:** Single binary with clap subcommands. A custom tokio-util codec handles length-prefixed binary frames. Server uses `nix::pty::openpty` + `tokio::process::Command` to spawn a shell on a PTY, wrapping the master fd with `AsyncFd`. Client puts the local terminal in raw mode via a drop guard and relays stdin/stdout. SIGWINCH propagation via `tokio::signal::unix`.

**Tech Stack:** Rust, tokio (full features), clap (derive), nix (pty, term, ioctl), bytes, tokio-util (codec)

---

### Task 1: Project scaffold

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`

**Step 1: Create Cargo.toml**

```toml
[package]
name = "ttyleport"
version = "0.1.0"
edition = "2024"

[lib]
name = "ttyleport"
path = "src/lib.rs"

[[bin]]
name = "ttyleport"
path = "src/main.rs"

[dependencies]
anyhow = "1"
bytes = "1"
clap = { version = "4", features = ["derive"] }
futures-util = "0.3"
libc = "0.2"
nix = { version = "0.29", features = ["term", "pty", "process", "signal", "fs"] }
tokio = { version = "1", features = ["full"] }
tokio-util = { version = "0.7", features = ["codec"] }
```

**Step 2: Create src/lib.rs**

```rust
pub mod protocol;
```

(Other modules added in later tasks.)

**Step 3: Create src/main.rs with CLI skeleton**

```rust
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "ttyleport", about = "Teleport a TTY over a socket")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Listen on a socket, spawn a shell, and relay the PTY
    Serve {
        /// Path to the Unix domain socket
        socket: PathBuf,
    },
    /// Connect to a socket and attach the local terminal
    Connect {
        /// Path to the Unix domain socket
        socket: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve { socket } => todo!("serve on {}", socket.display()),
        Command::Connect { socket } => todo!("connect to {}", socket.display()),
    }
}
```

**Step 4: Verify it compiles**

Run: `cargo build`
Expected: compiles (warnings about unused are fine)

**Step 5: Commit**

```
git add Cargo.toml src/main.rs src/lib.rs
git commit -m "scaffold: project structure with clap CLI"
```

---

### Task 2: Wire protocol codec

**Files:**
- Create: `src/protocol.rs`
- Create: `tests/protocol_test.rs`

**Step 1: Write the failing tests**

Create `tests/protocol_test.rs`:

```rust
use bytes::{Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};
use ttyleport::protocol::{Frame, FrameCodec};

#[test]
fn encode_data_frame() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec
        .encode(Frame::Data(Bytes::from("hello")), &mut buf)
        .unwrap();
    // type(1) + len(4) + payload(5) = 10
    assert_eq!(buf.len(), 10);
    assert_eq!(buf[0], 0x01);
    assert_eq!(&buf[5..], b"hello");
}

#[test]
fn encode_resize_frame() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec
        .encode(Frame::Resize { cols: 80, rows: 24 }, &mut buf)
        .unwrap();
    // type(1) + len(4) + payload(4) = 9
    assert_eq!(buf.len(), 9);
    assert_eq!(buf[0], 0x02);
}

#[test]
fn encode_exit_frame() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec.encode(Frame::Exit { code: 42 }, &mut buf).unwrap();
    assert_eq!(buf.len(), 9);
    assert_eq!(buf[0], 0x03);
}

#[test]
fn roundtrip_data() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Data(Bytes::from("hello world"));
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_resize() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Resize { cols: 120, rows: 40 };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_exit() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Exit { code: 0 };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn decode_incomplete_returns_none() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::from(&[0x01, 0x00, 0x00][..]);
    assert!(codec.decode(&mut buf).unwrap().is_none());
}

#[test]
fn decode_partial_payload_returns_none() {
    let mut codec = FrameCodec;
    // Header says 5 bytes payload, but only 2 present
    let mut buf = BytesMut::from(&[0x01, 0x00, 0x00, 0x00, 0x05, 0xAA, 0xBB][..]);
    assert!(codec.decode(&mut buf).unwrap().is_none());
}

#[test]
fn decode_invalid_type_returns_error() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::from(&[0xFF, 0x00, 0x00, 0x00, 0x00][..]);
    assert!(codec.decode(&mut buf).is_err());
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --test protocol_test`
Expected: FAIL — `protocol` module doesn't exist yet

**Step 3: Implement the protocol module**

Create `src/protocol.rs`:

```rust
use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::io;
use tokio_util::codec::{Decoder, Encoder};

const TYPE_DATA: u8 = 0x01;
const TYPE_RESIZE: u8 = 0x02;
const TYPE_EXIT: u8 = 0x03;

const HEADER_LEN: usize = 5; // type(1) + length(4)

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Data(Bytes),
    Resize { cols: u16, rows: u16 },
    Exit { code: i32 },
}

pub struct FrameCodec;

impl Decoder for FrameCodec {
    type Item = Frame;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, io::Error> {
        if src.len() < HEADER_LEN {
            return Ok(None);
        }

        let frame_type = src[0];
        let payload_len = u32::from_be_bytes([src[1], src[2], src[3], src[4]]) as usize;

        if src.len() < HEADER_LEN + payload_len {
            src.reserve(HEADER_LEN + payload_len - src.len());
            return Ok(None);
        }

        src.advance(HEADER_LEN);
        let payload = src.split_to(payload_len);

        match frame_type {
            TYPE_DATA => Ok(Some(Frame::Data(payload.freeze()))),
            TYPE_RESIZE => {
                if payload.len() != 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "resize frame must be 4 bytes",
                    ));
                }
                let cols = u16::from_be_bytes([payload[0], payload[1]]);
                let rows = u16::from_be_bytes([payload[2], payload[3]]);
                Ok(Some(Frame::Resize { cols, rows }))
            }
            TYPE_EXIT => {
                if payload.len() != 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "exit frame must be 4 bytes",
                    ));
                }
                let code = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                Ok(Some(Frame::Exit { code }))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown frame type: 0x{frame_type:02x}"),
            )),
        }
    }
}

impl Encoder<Frame> for FrameCodec {
    type Error = io::Error;

    fn encode(&mut self, frame: Frame, dst: &mut BytesMut) -> Result<(), io::Error> {
        match frame {
            Frame::Data(data) => {
                dst.put_u8(TYPE_DATA);
                dst.put_u32(data.len() as u32);
                dst.extend_from_slice(&data);
            }
            Frame::Resize { cols, rows } => {
                dst.put_u8(TYPE_RESIZE);
                dst.put_u32(4);
                dst.put_u16(cols);
                dst.put_u16(rows);
            }
            Frame::Exit { code } => {
                dst.put_u8(TYPE_EXIT);
                dst.put_u32(4);
                dst.put_i32(code);
            }
        }
        Ok(())
    }
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test --test protocol_test`
Expected: all 9 tests PASS

**Step 5: Commit**

```
git add src/protocol.rs tests/protocol_test.rs
git commit -m "feat: wire protocol codec with encode/decode"
```

---

### Task 3: Server mode

**Files:**
- Create: `src/server.rs`
- Modify: `src/lib.rs` (add `pub mod server;`)
- Modify: `src/main.rs` (wire up `serve` subcommand)

**Step 1: Implement server module**

Create `src/server.rs`:

```rust
use crate::protocol::{Frame, FrameCodec};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use nix::pty::openpty;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixListener;
use tokio::process::Command;
use tokio_util::codec::Framed;

pub async fn run(socket_path: &Path) -> anyhow::Result<()> {
    // Clean up stale socket file
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    eprintln!("listening on {}", socket_path.display());

    let (stream, _addr) = listener.accept().await?;
    eprintln!("client connected");

    // Allocate PTY
    let pty = openpty(None, None)?;
    let master: OwnedFd = pty.master;
    let slave: OwnedFd = pty.slave;

    // Spawn shell on slave PTY
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let slave_fd = slave.as_raw_fd();

    let mut child = unsafe {
        Command::new(&shell)
            .pre_exec(move || {
                nix::unistd::setsid().map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                libc::ioctl(slave_fd, libc::TIOCSCTTY, 0);
                Ok(())
            })
            .stdin(Stdio::from_raw_fd(slave.as_raw_fd()))
            .stdout(Stdio::from_raw_fd(slave.as_raw_fd()))
            .stderr(Stdio::from_raw_fd(slave.as_raw_fd()))
            .spawn()?
    };

    // Close slave in parent — child owns it now
    drop(slave);

    // Set master to non-blocking for AsyncFd
    let raw_master = master.as_raw_fd();
    let flags = nix::fcntl::fcntl(raw_master, nix::fcntl::FcntlArg::F_GETFL)?;
    let mut oflags = nix::fcntl::OFlag::from_bits_truncate(flags);
    oflags |= nix::fcntl::OFlag::O_NONBLOCK;
    nix::fcntl::fcntl(raw_master, nix::fcntl::FcntlArg::F_SETFL(oflags))?;

    let async_master = AsyncFd::new(master)?;
    let mut framed = Framed::new(stream, FrameCodec);

    let mut buf = vec![0u8; 4096];

    loop {
        tokio::select! {
            frame = framed.next() => {
                match frame {
                    Some(Ok(Frame::Data(data))) => {
                        let mut guard = async_master.writable().await?;
                        match guard.try_io(|inner| {
                            nix::unistd::write(inner.as_raw_fd(), &data)
                                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
                        }) {
                            Ok(Ok(_)) => {}
                            Ok(Err(e)) => return Err(e.into()),
                            Err(_would_block) => continue,
                        }
                    }
                    Some(Ok(Frame::Resize { cols, rows })) => {
                        let ws = libc::winsize {
                            ws_row: rows,
                            ws_col: cols,
                            ws_xpixel: 0,
                            ws_ypixel: 0,
                        };
                        unsafe {
                            libc::ioctl(
                                async_master.as_raw_fd(),
                                libc::TIOCSWINSZ,
                                &ws as *const _,
                            );
                        }
                    }
                    Some(Ok(Frame::Exit { .. })) | None => break,
                    Some(Err(e)) => return Err(e.into()),
                }
            }

            ready = async_master.readable() => {
                let mut guard = ready?;
                match guard.try_io(|inner| {
                    nix::unistd::read(inner.as_raw_fd(), &mut buf)
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
                }) {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => {
                        framed.send(Frame::Data(Bytes::copy_from_slice(&buf[..n]))).await?;
                    }
                    Ok(Err(e)) => {
                        if e.raw_os_error() == Some(libc::EIO) {
                            break;
                        }
                        return Err(e.into());
                    }
                    Err(_would_block) => continue,
                }
            }

            status = child.wait() => {
                let code = status?.code().unwrap_or(1);
                let _ = framed.send(Frame::Exit { code }).await;
                break;
            }
        }
    }

    let _ = std::fs::remove_file(socket_path);
    Ok(())
}
```

**Step 2: Update src/lib.rs**

```rust
pub mod protocol;
pub mod server;
```

**Step 3: Wire up main.rs Serve arm**

Replace the `Serve` todo with:
```rust
Command::Serve { socket } => server::run(&socket).await,
```

Add `mod server;` or use the lib path.

**Step 4: Verify it compiles**

Run: `cargo build`
Expected: compiles

**Step 5: Commit**

```
git add src/server.rs src/main.rs src/lib.rs
git commit -m "feat: server mode — PTY spawn + socket relay"
```

---

### Task 4: Client mode

**Files:**
- Create: `src/client.rs`
- Modify: `src/lib.rs` (add `pub mod client;`)
- Modify: `src/main.rs` (wire up `connect` subcommand)

**Step 1: Implement client module**

Create `src/client.rs`:

```rust
use crate::protocol::{Frame, FrameCodec};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use nix::sys::termios::{self, SetArg, Termios};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixStream;
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::codec::Framed;

struct RawModeGuard {
    fd: i32,
    original: Termios,
}

impl RawModeGuard {
    fn enter(fd: i32) -> nix::Result<Self> {
        let original = termios::tcgetattr(fd)?;
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(fd, SetArg::TCSAFLUSH, &raw)?;
        Ok(Self { fd, original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(self.fd, SetArg::TCSAFLUSH, &self.original);
    }
}

fn get_terminal_size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) };
    (ws.ws_col, ws.ws_row)
}

pub async fn run(socket_path: &Path) -> anyhow::Result<i32> {
    let stream = UnixStream::connect(socket_path).await?;
    let mut framed = Framed::new(stream, FrameCodec);

    let stdin_fd = io::stdin().as_raw_fd();
    let _guard = RawModeGuard::enter(stdin_fd)?;

    // Send initial window size
    let (cols, rows) = get_terminal_size();
    framed.send(Frame::Resize { cols, rows }).await?;

    // Set stdin to non-blocking for AsyncFd
    let flags = nix::fcntl::fcntl(stdin_fd, nix::fcntl::FcntlArg::F_GETFL)?;
    let mut oflags = nix::fcntl::OFlag::from_bits_truncate(flags);
    oflags |= nix::fcntl::OFlag::O_NONBLOCK;
    nix::fcntl::fcntl(stdin_fd, nix::fcntl::FcntlArg::F_SETFL(oflags))?;

    let async_stdin = AsyncFd::new(io::stdin())?;
    let mut sigwinch = signal(SignalKind::window_change())?;

    let mut buf = vec![0u8; 4096];
    let mut exit_code: i32 = 0;

    loop {
        tokio::select! {
            ready = async_stdin.readable() => {
                let mut guard = ready?;
                match guard.try_io(|inner| inner.get_ref().read(&mut buf)) {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => {
                        framed.send(Frame::Data(Bytes::copy_from_slice(&buf[..n]))).await?;
                    }
                    Ok(Err(e)) => return Err(e.into()),
                    Err(_would_block) => continue,
                }
            }

            frame = framed.next() => {
                match frame {
                    Some(Ok(Frame::Data(data))) => {
                        io::stdout().write_all(&data)?;
                        io::stdout().flush()?;
                    }
                    Some(Ok(Frame::Exit { code })) => {
                        exit_code = code;
                        break;
                    }
                    Some(Ok(Frame::Resize { .. })) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                }
            }

            _ = sigwinch.recv() => {
                let (cols, rows) = get_terminal_size();
                framed.send(Frame::Resize { cols, rows }).await?;
            }
        }
    }

    Ok(exit_code)
}
```

**Step 2: Update src/lib.rs**

```rust
pub mod client;
pub mod protocol;
pub mod server;
```

**Step 3: Wire up main.rs Connect arm**

Replace the `Connect` todo with:
```rust
Command::Connect { socket } => {
    let code = client::run(&socket).await?;
    std::process::exit(code);
}
```

**Step 4: Verify it compiles**

Run: `cargo build`
Expected: compiles

**Step 5: Commit**

```
git add src/client.rs src/main.rs src/lib.rs
git commit -m "feat: client mode — raw terminal + socket relay + SIGWINCH"
```

---

### Task 5: Integration test

**Files:**
- Create: `tests/e2e_test.rs`

**Step 1: Write integration test**

Create `tests/e2e_test.rs`:

```rust
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::timeout;
use tokio_util::codec::Framed;
use ttyleport::protocol::{Frame, FrameCodec};

#[tokio::test]
async fn server_spawns_shell_and_relays_output() {
    let socket_path = std::env::temp_dir().join("ttyleport-test.sock");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let server = tokio::spawn(async move {
        ttyleport::server::run(&path).await
    });

    // Give server time to bind
    tokio::time::sleep(Duration::from_millis(200)).await;

    let stream = UnixStream::connect(&socket_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);

    // Send initial resize
    framed
        .send(Frame::Resize { cols: 80, rows: 24 })
        .await
        .unwrap();

    // Should receive shell output (prompt)
    let frame = timeout(Duration::from_secs(3), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert!(matches!(frame, Frame::Data(_)));

    // Send exit to cleanly close
    framed
        .send(Frame::Data(Bytes::from("exit\n")))
        .await
        .unwrap();

    let _ = timeout(Duration::from_secs(3), server).await;
    let _ = std::fs::remove_file(&socket_path);
}
```

**Step 2: Run the test**

Run: `cargo test --test e2e_test -- --nocapture`
Expected: PASS

**Step 3: Commit**

```
git add tests/e2e_test.rs
git commit -m "test: end-to-end integration test"
```

---

### Task 6: CLAUDE.md

**Files:**
- Create: `CLAUDE.md`

**Step 1: Create CLAUDE.md with project guidance**

(Content based on the final state of the codebase after all tasks complete.)

**Step 2: Commit**

```
git add CLAUDE.md
git commit -m "docs: add CLAUDE.md"
```
