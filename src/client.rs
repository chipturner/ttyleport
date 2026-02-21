use crate::protocol::{Frame, FrameCodec};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use nix::sys::termios::{self, SetArg, Termios};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::codec::Framed;
use tracing::{debug, info};

const SEND_TIMEOUT: Duration = Duration::from_secs(5);

struct NonBlockGuard {
    fd: std::os::fd::RawFd,
    original_flags: nix::fcntl::OFlag,
}

impl NonBlockGuard {
    fn set(fd: std::os::fd::RawFd) -> nix::Result<Self> {
        let flags = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL)?;
        let original_flags = nix::fcntl::OFlag::from_bits_truncate(flags);
        nix::fcntl::fcntl(
            fd,
            nix::fcntl::FcntlArg::F_SETFL(original_flags | nix::fcntl::OFlag::O_NONBLOCK),
        )?;
        Ok(Self { fd, original_flags })
    }
}

impl Drop for NonBlockGuard {
    fn drop(&mut self) {
        let _ = nix::fcntl::fcntl(self.fd, nix::fcntl::FcntlArg::F_SETFL(self.original_flags));
    }
}

struct RawModeGuard {
    fd: BorrowedFd<'static>,
    original: Termios,
}

impl RawModeGuard {
    fn enter(fd: BorrowedFd<'static>) -> nix::Result<Self> {
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

/// Write all bytes to stdout, retrying on WouldBlock.
/// Needed because setting O_NONBLOCK on stdin also affects stdout
/// when they share the same terminal file description.
fn write_stdout(data: &[u8]) -> io::Result<()> {
    let mut stdout = io::stdout();
    let mut written = 0;
    while written < data.len() {
        match stdout.write(&data[written..]) {
            Ok(n) => written += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::yield_now();
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    stdout.flush()
}

fn get_terminal_size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) };
    (ws.ws_col, ws.ws_row)
}

async fn connect(socket_path: &Path) -> io::Result<Framed<UnixStream, FrameCodec>> {
    let stream = loop {
        match UnixStream::connect(socket_path).await {
            Ok(s) => break s,
            Err(e)
                if e.kind() == io::ErrorKind::ConnectionRefused
                    || e.kind() == io::ErrorKind::NotFound =>
            {
                debug!(path = %socket_path.display(), "waiting for session...");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(e) => return Err(e),
        }
    };
    info!(path = %socket_path.display(), "connected");
    Ok(Framed::new(stream, FrameCodec))
}

/// Send a frame with a timeout. Returns false if the send failed or timed out.
async fn timed_send(framed: &mut Framed<UnixStream, FrameCodec>, frame: Frame) -> bool {
    match tokio::time::timeout(SEND_TIMEOUT, framed.send(frame)).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            debug!("send error: {e}");
            false
        }
        Err(_) => {
            debug!("send timed out");
            false
        }
    }
}

/// Relay between stdin/stdout and the framed socket.
/// Returns `Some(code)` on clean shell exit, `None` on server disconnect.
async fn relay(
    framed: &mut Framed<UnixStream, FrameCodec>,
    async_stdin: &AsyncFd<io::Stdin>,
    sigwinch: &mut tokio::signal::unix::Signal,
    buf: &mut [u8],
) -> anyhow::Result<Option<i32>> {
    // Send initial window size
    let (cols, rows) = get_terminal_size();
    if !timed_send(framed, Frame::Resize { cols, rows }).await {
        return Ok(None);
    }

    loop {
        tokio::select! {
            ready = async_stdin.readable() => {
                let mut guard = ready?;
                match guard.try_io(|inner| inner.get_ref().read(buf)) {
                    Ok(Ok(0)) => {
                        debug!("stdin EOF");
                        return Ok(Some(0));
                    }
                    Ok(Ok(n)) => {
                        debug!(len = n, "stdin → socket");
                        if !timed_send(framed, Frame::Data(Bytes::copy_from_slice(&buf[..n]))).await {
                            return Ok(None);
                        }
                    }
                    Ok(Err(e)) => return Err(e.into()),
                    Err(_would_block) => continue,
                }
            }

            frame = framed.next() => {
                match frame {
                    Some(Ok(Frame::Data(data))) => {
                        debug!(len = data.len(), "socket → stdout");
                        write_stdout(&data)?;
                    }
                    Some(Ok(Frame::Exit { code })) => {
                        info!(code, "server sent exit");
                        return Ok(Some(code));
                    }
                    Some(Ok(Frame::Detached)) => {
                        info!("detached by another client");
                        return Ok(Some(0));
                    }
                    Some(Ok(_)) => {} // ignore control/resize frames
                    Some(Err(e)) => {
                        debug!("server connection error: {e}");
                        return Ok(None);
                    }
                    None => {
                        debug!("server disconnected");
                        return Ok(None);
                    }
                }
            }

            _ = sigwinch.recv() => {
                let (cols, rows) = get_terminal_size();
                debug!(cols, rows, "SIGWINCH → resize");
                if !timed_send(framed, Frame::Resize { cols, rows }).await {
                    return Ok(None);
                }
            }
        }
    }
}

pub async fn run(socket_path: &Path) -> anyhow::Result<i32> {
    // First connection before raw mode so Ctrl-C works while waiting
    let mut framed = connect(socket_path).await?;

    let stdin = io::stdin();
    let stdin_fd = stdin.as_fd();
    // Safety: stdin lives for the duration of the program
    let stdin_borrowed: BorrowedFd<'static> =
        unsafe { BorrowedFd::borrow_raw(stdin_fd.as_raw_fd()) };
    let _guard = RawModeGuard::enter(stdin_borrowed)?;

    // Set stdin to non-blocking for AsyncFd — guard restores on drop.
    // Declared BEFORE async_stdin so it drops AFTER AsyncFd (reverse drop order).
    let raw_fd = stdin_fd.as_raw_fd();
    let _nb_guard = NonBlockGuard::set(raw_fd)?;
    let async_stdin = AsyncFd::new(io::stdin())?;
    let mut sigwinch = signal(SignalKind::window_change())?;
    let mut buf = vec![0u8; 4096];

    loop {
        match relay(&mut framed, &async_stdin, &mut sigwinch, &mut buf).await? {
            Some(code) => return Ok(code),
            None => {
                info!("reconnecting...");
                // In raw mode, Ctrl-C arrives as byte 0x03 on stdin.
                // Select on both so the user can kill the client during reconnect.
                framed = loop {
                    tokio::select! {
                        result = connect(socket_path) => break result?,
                        ready = async_stdin.readable() => {
                            let mut guard = ready?;
                            if let Ok(Ok(n)) = guard.try_io(|inner| inner.get_ref().read(&mut buf))
                                && buf[..n].contains(&0x03)
                            {
                                return Ok(130);
                            }
                        }
                    }
                };
            }
        }
    }
}
