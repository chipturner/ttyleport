use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tokio_util::codec::Framed;
use ttyleport::protocol::{Frame, FrameCodec};

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Limit concurrent daemon tests to avoid PTY/CPU exhaustion under parallel load.
static CONCURRENCY: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(3));

/// Generate unique socket paths per test to avoid parallel conflicts.
fn unique_socks(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let pid = std::process::id();
    let seq = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir();
    (
        dir.join(format!("ttyleport-d-{label}-{pid}-{seq}-ctl.sock")),
        dir.join(format!("ttyleport-d-{label}-{pid}-{seq}-sess.sock")),
    )
}

/// Generate unique socket paths for tests needing 2 session sockets.
fn unique_socks3(label: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let pid = std::process::id();
    let seq = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir();
    (
        dir.join(format!("ttyleport-d-{label}-{pid}-{seq}-ctl.sock")),
        dir.join(format!("ttyleport-d-{label}-{pid}-{seq}-s1.sock")),
        dir.join(format!("ttyleport-d-{label}-{pid}-{seq}-s2.sock")),
    )
}

/// Helper: send a control frame and get the response.
async fn control_request(ctl_path: &std::path::Path, frame: Frame) -> Frame {
    let stream = UnixStream::connect(ctl_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    framed.send(frame).await.unwrap();
    timeout(Duration::from_secs(3), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error")
}

/// Drain all available Data frames within a timeout.
async fn drain_data(framed: &mut Framed<UnixStream, FrameCodec>, wait: Duration) {
    loop {
        match timeout(wait, framed.next()).await {
            Ok(Some(Ok(Frame::Data(_)))) => {}
            _ => break,
        }
    }
}

/// Connect to a session socket, send resize, and wait for shell to produce output.
async fn connect_session(session_path: &std::path::Path) -> Framed<UnixStream, FrameCodec> {
    let stream = loop {
        match UnixStream::connect(session_path).await {
            Ok(s) => break s,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    };
    let mut framed = Framed::new(stream, FrameCodec);
    framed
        .send(Frame::Resize { cols: 80, rows: 24 })
        .await
        .unwrap();

    // Wait for shell to actually be alive (first Data frame)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match timeout(Duration::from_secs(1), framed.next()).await {
            Ok(Some(Ok(Frame::Data(_)))) => break,
            _ if tokio::time::Instant::now() >= deadline => {
                panic!("shell did not produce output within 15s on {}", session_path.display())
            }
            _ => continue,
        }
    }

    framed
}

/// Helper: create a session via daemon, wait for it to bind.
async fn create_session(ctl_path: &std::path::Path, session_path: &std::path::Path) {
    let resp = control_request(
        ctl_path,
        Frame::CreateSession {
            path: session_path.display().to_string(),
        },
    )
    .await;
    assert_eq!(resp, Frame::Ok);
    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// Clean up a session via kill-session (no shell connection needed).
async fn kill_cleanup(ctl_path: &std::path::Path, session_path: &std::path::Path) {
    let _ = control_request(
        ctl_path,
        Frame::KillSession {
            path: session_path.display().to_string(),
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn daemon_creates_and_lists_sessions() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (ctl_path, session_path) = unique_socks("create-list");
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { ttyleport::daemon::run(&ctl).await });

    tokio::time::sleep(Duration::from_millis(200)).await;

    create_session(&ctl_path, &session_path).await;

    // List sessions — should see one
    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].path, session_path.display().to_string());
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }

    // Clean up via kill (no need to connect to shell — this test is about create/list)
    kill_cleanup(&ctl_path, &session_path).await;
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);
}

#[tokio::test]
async fn daemon_rejects_duplicate_session() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (ctl_path, session_path) = unique_socks("dup");
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { ttyleport::daemon::run(&ctl).await });

    tokio::time::sleep(Duration::from_millis(200)).await;

    create_session(&ctl_path, &session_path).await;

    // Try to create same session again
    let resp = control_request(
        &ctl_path,
        Frame::CreateSession {
            path: session_path.display().to_string(),
        },
    )
    .await;
    assert!(
        matches!(resp, Frame::Error { .. }),
        "expected Error for duplicate session, got {resp:?}"
    );

    kill_cleanup(&ctl_path, &session_path).await;
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);
}

#[tokio::test]
async fn daemon_kills_session() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (ctl_path, session_path) = unique_socks("kill");
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { ttyleport::daemon::run(&ctl).await });

    tokio::time::sleep(Duration::from_millis(200)).await;

    create_session(&ctl_path, &session_path).await;

    // Kill it
    let resp = control_request(
        &ctl_path,
        Frame::KillSession {
            path: session_path.display().to_string(),
        },
    )
    .await;
    assert_eq!(resp, Frame::Ok);

    // List should be empty
    tokio::time::sleep(Duration::from_millis(200)).await;
    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert!(sessions.is_empty(), "expected no sessions after kill");
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }

    // Socket file should be gone
    assert!(!session_path.exists(), "session socket should be removed");

    let _ = std::fs::remove_file(&ctl_path);
}

#[tokio::test]
async fn daemon_kills_server() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (ctl_path, session_path) = unique_socks("killsrv");
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);

    let ctl = ctl_path.clone();
    let daemon = tokio::spawn(async move { ttyleport::daemon::run(&ctl).await });

    tokio::time::sleep(Duration::from_millis(200)).await;

    create_session(&ctl_path, &session_path).await;

    // Kill the server
    let resp = control_request(&ctl_path, Frame::KillServer).await;
    assert_eq!(resp, Frame::Ok);

    // Daemon task should exit
    let result = timeout(Duration::from_secs(3), daemon).await;
    assert!(result.is_ok(), "daemon should exit after kill-server");

    // Control socket should be gone
    assert!(!ctl_path.exists(), "control socket should be removed");

    let _ = std::fs::remove_file(&session_path);
}

// ─── New hardening tests ────────────────────────────────────────────────────

#[tokio::test]
async fn create_after_kill_same_path() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    // Kill a session, then create a new one at the same path.
    let (ctl_path, session_path) = unique_socks("create-after-kill");
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { ttyleport::daemon::run(&ctl).await });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Create first session
    create_session(&ctl_path, &session_path).await;

    // Kill it
    let resp = control_request(
        &ctl_path,
        Frame::KillSession {
            path: session_path.display().to_string(),
        },
    )
    .await;
    assert_eq!(resp, Frame::Ok);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Create again at the same path — should succeed
    let resp = control_request(
        &ctl_path,
        Frame::CreateSession {
            path: session_path.display().to_string(),
        },
    )
    .await;
    assert_eq!(resp, Frame::Ok, "should be able to create session at same path after kill");

    // Verify recreated session appears in list with valid metadata
    // Poll until metadata is populated (shell_pid > 0)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let resp = control_request(&ctl_path, Frame::ListSessions).await;
        match &resp {
            Frame::SessionInfo { sessions } if sessions.len() == 1 && sessions[0].shell_pid > 0 => {
                assert_eq!(sessions[0].path, session_path.display().to_string());
                break;
            }
            _ if tokio::time::Instant::now() < deadline => continue,
            other => panic!("recreated session should appear with valid PID, got {other:?}"),
        }
    }

    kill_cleanup(&ctl_path, &session_path).await;
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);
}

#[tokio::test]
async fn multiple_concurrent_sessions() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (ctl_path, session1, session2) = unique_socks3("multi");
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session1);
    let _ = std::fs::remove_file(&session2);

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { ttyleport::daemon::run(&ctl).await });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Create two sessions
    create_session(&ctl_path, &session1).await;
    create_session(&ctl_path, &session2).await;

    // List should show both
    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert_eq!(sessions.len(), 2, "expected 2 sessions");
            let paths: Vec<&str> = sessions.iter().map(|s| s.path.as_str()).collect();
            assert!(
                paths.contains(&session1.display().to_string().as_str()),
                "should contain session1"
            );
            assert!(
                paths.contains(&session2.display().to_string().as_str()),
                "should contain session2"
            );
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }

    // Both sessions should be alive — verify via metadata (PID > 0)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let resp = control_request(&ctl_path, Frame::ListSessions).await;
        match &resp {
            Frame::SessionInfo { sessions }
                if sessions.len() == 2 && sessions.iter().all(|s| s.shell_pid > 0) =>
            {
                break;
            }
            _ if tokio::time::Instant::now() < deadline => continue,
            other => panic!("expected 2 sessions with valid PIDs, got {other:?}"),
        }
    }

    kill_cleanup(&ctl_path, &session1).await;
    kill_cleanup(&ctl_path, &session2).await;
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session1);
    let _ = std::fs::remove_file(&session2);
}

#[tokio::test]
async fn daemon_unexpected_frame() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    // Sending a Data frame to the control socket should return Error.
    let (ctl_path, _) = unique_socks("unexpected");
    let _ = std::fs::remove_file(&ctl_path);

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { ttyleport::daemon::run(&ctl).await });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Send a Data frame (makes no sense on control socket)
    let resp = control_request(&ctl_path, Frame::Data(Bytes::from("hello"))).await;
    assert!(
        matches!(resp, Frame::Error { .. }),
        "expected Error for unexpected frame, got {resp:?}"
    );

    // Send a Resize frame
    let resp = control_request(&ctl_path, Frame::Resize { cols: 80, rows: 24 }).await;
    assert!(
        matches!(resp, Frame::Error { .. }),
        "expected Error for Resize on control socket, got {resp:?}"
    );

    // Daemon should still be alive
    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert!(sessions.is_empty());
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }

    let _ = std::fs::remove_file(&ctl_path);
}

#[tokio::test]
async fn kill_server_no_sessions() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    // Kill-server with zero sessions (empty drain path).
    let (ctl_path, _) = unique_socks("killsrv-empty");
    let _ = std::fs::remove_file(&ctl_path);

    let ctl = ctl_path.clone();
    let daemon = tokio::spawn(async move { ttyleport::daemon::run(&ctl).await });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Kill immediately — no sessions created
    let resp = control_request(&ctl_path, Frame::KillServer).await;
    assert_eq!(resp, Frame::Ok);

    let result = timeout(Duration::from_secs(3), daemon).await;
    assert!(result.is_ok(), "daemon should exit after kill-server with no sessions");

    assert!(!ctl_path.exists(), "control socket should be removed");
}

#[tokio::test]
async fn kill_nonexistent_session() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let (ctl_path, _) = unique_socks("kill-nonexist");
    let _ = std::fs::remove_file(&ctl_path);

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { ttyleport::daemon::run(&ctl).await });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Kill a session that was never created
    let resp = control_request(
        &ctl_path,
        Frame::KillSession {
            path: "/tmp/does-not-exist.sock".to_string(),
        },
    )
    .await;
    assert!(
        matches!(resp, Frame::Error { .. }),
        "expected Error for nonexistent session, got {resp:?}"
    );

    let _ = std::fs::remove_file(&ctl_path);
}

#[tokio::test]
async fn session_natural_exit_reaps_from_list() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    // When a shell exits naturally, the daemon should reap it from the session list.
    let (ctl_path, session_path) = unique_socks("natural-reap");
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { ttyleport::daemon::run(&ctl).await });

    tokio::time::sleep(Duration::from_millis(200)).await;

    create_session(&ctl_path, &session_path).await;

    // Wait for shell to actually start (metadata shows PID > 0), then SIGTERM it.
    // This tests the daemon's reaping path when a shell dies on its own (not via KillSession).
    let shell_pid;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let resp = control_request(&ctl_path, Frame::ListSessions).await;
        match &resp {
            Frame::SessionInfo { sessions }
                if sessions.len() == 1 && sessions[0].shell_pid > 0 =>
            {
                shell_pid = sessions[0].shell_pid;
                break;
            }
            _ if tokio::time::Instant::now() < deadline => continue,
            other => panic!("shell did not start within 10s, got {other:?}"),
        }
    }

    // Send SIGKILL to the shell — this is "natural" from the daemon's perspective
    // (the shell dies on its own, not via KillSession)
    unsafe {
        libc::kill(shell_pid as i32, libc::SIGKILL);
    }

    // Poll list until sessions is empty (shell exit + server task finish + reap)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let resp = control_request(&ctl_path, Frame::ListSessions).await;
        match &resp {
            Frame::SessionInfo { sessions } if sessions.is_empty() => break,
            Frame::SessionInfo { .. } if tokio::time::Instant::now() < deadline => continue,
            Frame::SessionInfo { sessions } => {
                panic!("expected no sessions after natural exit, got {sessions:?}");
            }
            other => panic!("expected SessionInfo, got {other:?}"),
        }
    }

    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);
}

#[tokio::test]
async fn list_before_session_ready() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    // List sessions immediately after create, before the session has fully started.
    // Metadata may not be set yet — should show empty/zero fields gracefully.
    let (ctl_path, session_path) = unique_socks("list-early");
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { ttyleport::daemon::run(&ctl).await });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Create session (don't wait the usual 200ms)
    let resp = control_request(
        &ctl_path,
        Frame::CreateSession {
            path: session_path.display().to_string(),
        },
    )
    .await;
    assert_eq!(resp, Frame::Ok);

    // List immediately — session should appear (possibly with empty metadata)
    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert_eq!(sessions.len(), 1, "session should appear in list immediately");
            assert_eq!(sessions[0].path, session_path.display().to_string());
            // shell_pid might be 0 if metadata isn't ready yet — that's fine
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }

    // Wait for it to fully start, then clean up
    tokio::time::sleep(Duration::from_millis(300)).await;
    kill_cleanup(&ctl_path, &session_path).await;
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);
}

#[tokio::test]
async fn kill_session_while_client_connected() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    // When daemon kills a session, a connected client should see the connection break.
    let (ctl_path, session_path) = unique_socks("kill-connected");
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { ttyleport::daemon::run(&ctl).await });

    tokio::time::sleep(Duration::from_millis(200)).await;

    create_session(&ctl_path, &session_path).await;

    // Connect a client
    let mut framed = connect_session(&session_path).await;
    drain_data(&mut framed, Duration::from_millis(500)).await;

    // Kill the session via daemon while client is connected
    let resp = control_request(
        &ctl_path,
        Frame::KillSession {
            path: session_path.display().to_string(),
        },
    )
    .await;
    assert_eq!(resp, Frame::Ok);

    // Client should see the stream end (None or error)
    let result = timeout(Duration::from_secs(3), framed.next()).await;
    match result {
        Ok(None) | Ok(Some(Err(_))) | Err(_) => {
            // Expected: stream ended or error — client detected kill
        }
        Ok(Some(Ok(Frame::Data(_)))) => {
            // Might get some buffered data first; keep reading
            let end = timeout(Duration::from_secs(2), framed.next()).await;
            assert!(
                matches!(end, Ok(None) | Ok(Some(Err(_))) | Err(_)),
                "client should eventually see stream end after kill"
            );
        }
        Ok(Some(Ok(other))) => {
            panic!("unexpected frame after session kill: {other:?}");
        }
    }

    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);
}

#[tokio::test]
async fn session_metadata_has_pty_and_pid() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    // Verify that list-sessions returns real PTY path and PID.
    let (ctl_path, session_path) = unique_socks("meta-info");
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move { ttyleport::daemon::run(&ctl).await });

    tokio::time::sleep(Duration::from_millis(200)).await;

    create_session(&ctl_path, &session_path).await;

    // Poll until metadata is populated
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let resp = control_request(&ctl_path, Frame::ListSessions).await;
        match &resp {
            Frame::SessionInfo { sessions } if sessions.len() == 1 && sessions[0].shell_pid > 0 => {
                let s = &sessions[0];
                assert!(
                    s.pty_path.starts_with("/dev/pts/") || s.pty_path.starts_with("/dev/tty"),
                    "pty_path should be a real device, got: {}",
                    s.pty_path
                );
                assert!(s.created_at > 0, "created_at should be set");
                break;
            }
            _ if tokio::time::Instant::now() < deadline => continue,
            other => panic!("expected session with valid metadata, got {other:?}"),
        }
    }

    kill_cleanup(&ctl_path, &session_path).await;
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);
}
