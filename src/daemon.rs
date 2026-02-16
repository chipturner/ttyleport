use crate::protocol::{Frame, FrameCodec};
use crate::server;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;
use tracing::{error, info};

/// Returns the control socket path.
/// Prefers $XDG_RUNTIME_DIR/ttyleport/ctl.sock, falls back to /tmp/ttyleport-$UID/ctl.sock.
pub fn control_socket_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(xdg).join("ttyleport").join("ctl.sock")
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/ttyleport-{uid}")).join("ctl.sock")
    }
}

/// Run the daemon, listening on the control socket.
pub async fn run(ctl_path: &Path) -> anyhow::Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = ctl_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Clean up stale socket
    if ctl_path.exists() {
        std::fs::remove_file(ctl_path)?;
    }

    let listener = UnixListener::bind(ctl_path)?;
    info!(path = %ctl_path.display(), "daemon listening");

    let mut sessions: HashMap<PathBuf, JoinHandle<anyhow::Result<()>>> = HashMap::new();

    loop {
        // Reap finished sessions
        sessions.retain(|path, handle| {
            if handle.is_finished() {
                info!(path = %path.display(), "session ended");
                false
            } else {
                true
            }
        });

        let (stream, _addr) = listener.accept().await?;
        let mut framed = Framed::new(stream, FrameCodec);

        // Handle one control request per connection
        let Some(Ok(frame)) = framed.next().await else {
            continue;
        };

        match frame {
            Frame::CreateSession { path } => {
                let session_path = PathBuf::from(&path);

                if sessions.contains_key(&session_path) {
                    let _ = framed
                        .send(Frame::Error {
                            message: format!("session already exists: {path}"),
                        })
                        .await;
                    continue;
                }

                let sp = session_path.clone();
                let handle = tokio::spawn(async move {
                    server::run(&sp).await
                });
                sessions.insert(session_path, handle);

                info!(path = %path, "session created");
                let _ = framed.send(Frame::Ok).await;
            }
            Frame::ListSessions => {
                // Reap again before listing
                sessions.retain(|path, handle| {
                    if handle.is_finished() {
                        info!(path = %path.display(), "session ended");
                        false
                    } else {
                        true
                    }
                });

                let paths: Vec<String> = sessions
                    .keys()
                    .map(|p| p.display().to_string())
                    .collect();
                let _ = framed.send(Frame::SessionInfo { sessions: paths }).await;
            }
            other => {
                error!(?other, "unexpected frame on control socket");
                let _ = framed
                    .send(Frame::Error {
                        message: "unexpected frame type".to_string(),
                    })
                    .await;
            }
        }
    }
}
