use crate::protocol::{Frame, FrameCodec, SessionEntry};
use crate::server::{self, SessionMetadata};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;
use tracing::{error, info, warn};

struct SessionState {
    handle: JoinHandle<anyhow::Result<()>>,
    metadata: Arc<OnceLock<SessionMetadata>>,
    client_tx: mpsc::UnboundedSender<Framed<UnixStream, FrameCodec>>,
    name: Option<String>,
}

/// Returns the daemon socket path.
/// Prefers $XDG_RUNTIME_DIR/ttyleport/ctl.sock, falls back to /tmp/ttyleport-$UID/ctl.sock.
pub fn control_socket_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(xdg).join("ttyleport").join("ctl.sock")
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/ttyleport-{uid}")).join("ctl.sock")
    }
}

fn reap_sessions(sessions: &mut HashMap<u32, SessionState>) {
    sessions.retain(|id, state| {
        if state.handle.is_finished() {
            info!(id, "session ended");
            false
        } else {
            true
        }
    });
}

/// Resolve a session identifier (name or id string) to a session id.
fn resolve_session(sessions: &HashMap<u32, SessionState>, target: &str) -> Option<u32> {
    // Try name match first
    for (&id, state) in sessions {
        if state.name.as_deref() == Some(target) {
            return Some(id);
        }
    }
    // Then try parsing as numeric id
    if let Ok(id) = target.parse::<u32>()
        && sessions.contains_key(&id)
    {
        return Some(id);
    }
    None
}

fn build_session_entries(sessions: &HashMap<u32, SessionState>) -> Vec<SessionEntry> {
    let mut entries: Vec<_> = sessions
        .iter()
        .map(|(&id, state)| {
            if let Some(meta) = state.metadata.get() {
                SessionEntry {
                    id: id.to_string(),
                    name: state.name.clone().unwrap_or_default(),
                    pty_path: meta.pty_path.clone(),
                    shell_pid: meta.shell_pid,
                    created_at: meta.created_at,
                    attached: meta.attached.load(Ordering::Relaxed),
                }
            } else {
                SessionEntry {
                    id: id.to_string(),
                    name: state.name.clone().unwrap_or_default(),
                    pty_path: String::new(),
                    shell_pid: 0,
                    created_at: 0,
                    attached: false,
                }
            }
        })
        .collect();
    entries.sort_by_key(|e| e.id.parse::<u32>().unwrap_or(u32::MAX));
    entries
}

/// Run the daemon, listening on its socket.
pub async fn run(ctl_path: &Path) -> anyhow::Result<()> {
    // Restrictive umask for all files/sockets created by the daemon
    unsafe {
        libc::umask(0o077);
    }

    // Ensure parent directory exists with secure permissions
    if let Some(parent) = ctl_path.parent() {
        crate::security::secure_create_dir_all(parent)?;
    }

    let listener = crate::security::bind_unix_listener(ctl_path)?;
    info!(path = %ctl_path.display(), "daemon listening");

    let mut sessions: HashMap<u32, SessionState> = HashMap::new();
    let mut next_id: u32 = 0;

    loop {
        reap_sessions(&mut sessions);

        let (stream, _addr) = listener.accept().await?;
        if let Err(e) = crate::security::verify_peer_uid(&stream) {
            warn!("{e}");
            continue;
        }
        let mut framed = Framed::new(stream, FrameCodec);

        // Handle one control request per connection
        let Some(Ok(frame)) = framed.next().await else {
            continue;
        };

        match frame {
            Frame::NewSession { name } => {
                // Check for duplicate name
                let name_opt = if name.is_empty() { None } else { Some(name) };
                if let Some(ref n) = name_opt {
                    let dup = sessions.values().any(|s| s.name.as_deref() == Some(n));
                    if dup {
                        let _ = framed
                            .send(Frame::Error {
                                message: format!("session name already exists: {n}"),
                            })
                            .await;
                        continue;
                    }
                }

                let id = next_id;
                next_id += 1;

                let (client_tx, client_rx) = mpsc::unbounded_channel();
                let metadata = Arc::new(OnceLock::new());
                let meta_clone = Arc::clone(&metadata);
                let handle = tokio::spawn(async move { server::run(client_rx, meta_clone).await });

                sessions.insert(
                    id,
                    SessionState {
                        handle,
                        metadata,
                        client_tx: client_tx.clone(),
                        name: name_opt.clone(),
                    },
                );

                info!(id, name = ?name_opt, "session created");

                let _ = framed
                    .send(Frame::SessionCreated {
                        id: id.to_string(),
                    })
                    .await;

                // Hand off connection to session for auto-attach
                let _ = client_tx.send(framed);
            }
            Frame::Attach { session } => {
                if let Some(id) = resolve_session(&sessions, &session) {
                    let state = &sessions[&id];
                    let _ = framed.send(Frame::Ok).await;
                    if state.client_tx.send(framed).is_err() {
                        // Session task already exited
                    }
                } else {
                    let _ = framed
                        .send(Frame::Error {
                            message: format!("no such session: {session}"),
                        })
                        .await;
                }
            }
            Frame::ListSessions => {
                reap_sessions(&mut sessions);
                let entries = build_session_entries(&sessions);
                let _ = framed.send(Frame::SessionInfo { sessions: entries }).await;
            }
            Frame::KillSession { session } => {
                if let Some(id) = resolve_session(&sessions, &session) {
                    let state = sessions.remove(&id).unwrap();
                    state.handle.abort();
                    info!(id, "session killed");
                    let _ = framed.send(Frame::Ok).await;
                } else {
                    let _ = framed
                        .send(Frame::Error {
                            message: format!("no such session: {session}"),
                        })
                        .await;
                }
            }
            Frame::KillServer => {
                info!("kill-server received, shutting down");
                for (id, state) in sessions.drain() {
                    state.handle.abort();
                    info!(id, "session aborted");
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
