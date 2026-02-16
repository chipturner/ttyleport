use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::timeout;
use tokio_util::codec::Framed;
use ttyleport::protocol::{Frame, FrameCodec};

/// Helper: send a control frame and get the response.
async fn control_request(
    ctl_path: &std::path::Path,
    frame: Frame,
) -> Frame {
    let stream = UnixStream::connect(ctl_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    framed.send(frame).await.unwrap();
    timeout(Duration::from_secs(3), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error")
}

#[tokio::test]
async fn daemon_creates_and_lists_sessions() {
    let ctl_path = std::env::temp_dir().join("ttyleport-daemon-test-ctl.sock");
    let session_path = std::env::temp_dir().join("ttyleport-daemon-test-session.sock");
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move {
        ttyleport::daemon::run(&ctl).await
    });

    // Give daemon time to bind
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Create a session
    let resp = control_request(
        &ctl_path,
        Frame::CreateSession {
            path: session_path.display().to_string(),
        },
    )
    .await;
    assert_eq!(resp, Frame::Ok);

    // Give session time to bind its socket
    tokio::time::sleep(Duration::from_millis(200)).await;

    // List sessions — should see one
    let resp = control_request(&ctl_path, Frame::ListSessions).await;
    match &resp {
        Frame::SessionInfo { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].path, session_path.display().to_string());
        }
        other => panic!("expected SessionInfo, got {other:?}"),
    }

    // Verify we can connect to the session socket and get shell output
    let stream = UnixStream::connect(&session_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    framed
        .send(Frame::Resize { cols: 80, rows: 24 })
        .await
        .unwrap();

    // Should get shell output — drain all available data
    let mut got_data = false;
    loop {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::Data(_)))) => { got_data = true; }
            _ => break,
        }
    }
    assert!(got_data, "expected shell output after connect");

    // Clean up: exit the shell
    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);
}

#[tokio::test]
async fn daemon_rejects_duplicate_session() {
    let ctl_path = std::env::temp_dir().join("ttyleport-daemon-test-dup-ctl.sock");
    let session_path = std::env::temp_dir().join("ttyleport-daemon-test-dup-session.sock");
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move {
        ttyleport::daemon::run(&ctl).await
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Create first session
    let resp = control_request(
        &ctl_path,
        Frame::CreateSession {
            path: session_path.display().to_string(),
        },
    )
    .await;
    assert_eq!(resp, Frame::Ok);

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

    // Clean up
    tokio::time::sleep(Duration::from_millis(200)).await;
    let stream = UnixStream::connect(&session_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    framed.send(Frame::Resize { cols: 80, rows: 24 }).await.unwrap();
    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);
}

#[tokio::test]
async fn daemon_kills_session() {
    let ctl_path = std::env::temp_dir().join("ttyleport-daemon-test-kill-ctl.sock");
    let session_path = std::env::temp_dir().join("ttyleport-daemon-test-kill-session.sock");
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);

    let ctl = ctl_path.clone();
    let _daemon = tokio::spawn(async move {
        ttyleport::daemon::run(&ctl).await
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Create a session
    let resp = control_request(
        &ctl_path,
        Frame::CreateSession {
            path: session_path.display().to_string(),
        },
    )
    .await;
    assert_eq!(resp, Frame::Ok);

    tokio::time::sleep(Duration::from_millis(200)).await;

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
    let ctl_path = std::env::temp_dir().join("ttyleport-daemon-test-killsrv-ctl.sock");
    let session_path = std::env::temp_dir().join("ttyleport-daemon-test-killsrv-session.sock");
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(&session_path);

    let ctl = ctl_path.clone();
    let daemon = tokio::spawn(async move {
        ttyleport::daemon::run(&ctl).await
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Create a session
    let resp = control_request(
        &ctl_path,
        Frame::CreateSession {
            path: session_path.display().to_string(),
        },
    )
    .await;
    assert_eq!(resp, Frame::Ok);

    tokio::time::sleep(Duration::from_millis(200)).await;

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
