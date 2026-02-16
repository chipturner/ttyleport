use crate::protocol::{Frame, FrameCodec, SessionEntry};
use crate::server::{self, SessionMetadata};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;
use tracing::{error, info};

struct SessionState {
    handle: JoinHandle<anyhow::Result<()>>,
    metadata: Arc<OnceLock<SessionMetadata>>,
}

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

fn reap_sessions(sessions: &mut HashMap<PathBuf, SessionState>) {
    sessions.retain(|path, state| {
        if state.handle.is_finished() {
            info!(path = %path.display(), "session ended");
            false
        } else {
            true
        }
    });
}

fn build_session_entries(sessions: &HashMap<PathBuf, SessionState>) -> Vec<SessionEntry> {
    sessions
        .iter()
        .map(|(p, state)| {
            if let Some(meta) = state.metadata.get() {
                SessionEntry {
                    path: p.display().to_string(),
                    pty_path: meta.pty_path.clone(),
                    shell_pid: meta.shell_pid,
                    created_at: meta.created_at,
                    attached: meta.attached.load(Ordering::Relaxed),
                }
            } else {
                SessionEntry {
                    path: p.display().to_string(),
                    pty_path: String::new(),
                    shell_pid: 0,
                    created_at: 0,
                    attached: false,
                }
            }
        })
        .collect()
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

    let mut sessions: HashMap<PathBuf, SessionState> = HashMap::new();

    loop {
        reap_sessions(&mut sessions);

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
                let metadata = Arc::new(OnceLock::new());
                let meta_clone = Arc::clone(&metadata);
                let handle = tokio::spawn(async move {
                    server::run(&sp, meta_clone).await
                });
                sessions.insert(session_path, SessionState { handle, metadata });

                info!(path = %path, "session created");
                let _ = framed.send(Frame::Ok).await;
            }
            Frame::ListSessions => {
                reap_sessions(&mut sessions);
                let entries = build_session_entries(&sessions);
                let _ = framed.send(Frame::SessionInfo { sessions: entries }).await;
            }
            Frame::KillSession { path } => {
                let session_path = PathBuf::from(&path);
                if let Some(state) = sessions.remove(&session_path) {
                    state.handle.abort();
                    let _ = std::fs::remove_file(&session_path);
                    info!(path = %path, "session killed");
                    let _ = framed.send(Frame::Ok).await;
                } else {
                    let _ = framed
                        .send(Frame::Error {
                            message: format!("no such session: {path}"),
                        })
                        .await;
                }
            }
            Frame::KillServer => {
                info!("kill-server received, shutting down");
                for (path, state) in sessions.drain() {
                    state.handle.abort();
                    let _ = std::fs::remove_file(&path);
                }
                let _ = framed.send(Frame::Ok).await;
                let _ = std::fs::remove_file(ctl_path);
                break;
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

    Ok(())
}
