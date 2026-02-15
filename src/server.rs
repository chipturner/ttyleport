use crate::protocol::{Frame, FrameCodec};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use nix::pty::openpty;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
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
                            nix::unistd::write(inner, &data)
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
