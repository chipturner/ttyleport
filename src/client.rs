use crate::protocol::{Frame, FrameCodec};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use nix::sys::termios::{self, SetArg, Termios};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixStream;
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::codec::Framed;

struct RawModeGuard {
    fd: BorrowedFd<'static>,
    original: Termios,
}

impl RawModeGuard {
    fn enter(fd: BorrowedFd<'static>) -> nix::Result<Self> {
        let original = termios::tcgetattr(&fd)?;
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(&fd, SetArg::TCSAFLUSH, &raw)?;
        Ok(Self { fd, original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(&self.fd, SetArg::TCSAFLUSH, &self.original);
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

    let stdin = io::stdin();
    let stdin_fd = stdin.as_fd();
    // Safety: stdin lives for the duration of the program
    let stdin_borrowed: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(stdin_fd.as_raw_fd()) };
    let _guard = RawModeGuard::enter(stdin_borrowed)?;

    // Send initial window size
    let (cols, rows) = get_terminal_size();
    framed.send(Frame::Resize { cols, rows }).await?;

    // Set stdin to non-blocking for AsyncFd
    let raw_fd = stdin_fd.as_raw_fd();
    let flags = nix::fcntl::fcntl(raw_fd, nix::fcntl::FcntlArg::F_GETFL)?;
    let mut oflags = nix::fcntl::OFlag::from_bits_truncate(flags);
    oflags |= nix::fcntl::OFlag::O_NONBLOCK;
    nix::fcntl::fcntl(raw_fd, nix::fcntl::FcntlArg::F_SETFL(oflags))?;

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
