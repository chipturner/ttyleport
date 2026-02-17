use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tokio_util::codec::Framed;
use ttyleport::protocol::{Frame, FrameCodec};

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Limit concurrent e2e tests to avoid PTY/CPU exhaustion under parallel load.
static CONCURRENCY: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(4));

/// Generate a unique socket path per test to avoid parallel conflicts.
fn unique_sock(label: &str) -> std::path::PathBuf {
    let pid = std::process::id();
    let seq = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("ttyleport-e2e-{label}-{pid}-{seq}.sock"))
}

async fn connect_to_server(socket_path: &std::path::Path) -> Framed<UnixStream, FrameCodec> {
    // Wait for server to bind — retry to handle parallel test load
    let stream = loop {
        match UnixStream::connect(socket_path).await {
            Ok(s) => break s,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    };
    let mut framed = Framed::new(stream, FrameCodec);

    // Send initial resize
    framed
        .send(Frame::Resize { cols: 80, rows: 24 })
        .await
        .unwrap();

    // Wait for shell to actually produce output (confirms it's alive).
    // Under parallel test load, shell startup can take several seconds.
    // We peek by waiting for a Data frame, then push it back by reading
    // all available data in the caller's drain step.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match timeout(Duration::from_secs(1), framed.next()).await {
            Ok(Some(Ok(Frame::Data(_)))) => break,
            _ if tokio::time::Instant::now() >= deadline => {
                panic!("shell did not produce output within 10s")
            }
            _ => continue,
        }
    }

    framed
}

/// Drain all available Data frames within a timeout, return concatenated bytes.
async fn read_available_data(
    framed: &mut Framed<UnixStream, FrameCodec>,
    wait: Duration,
) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        match timeout(wait, framed.next()).await {
            Ok(Some(Ok(Frame::Data(data)))) => out.extend_from_slice(&data),
            _ => break,
        }
    }
    out
}

/// Read frames until we see an Exit frame or timeout.
async fn expect_exit_frame(
    framed: &mut Framed<UnixStream, FrameCodec>,
    wait: Duration,
) -> Option<i32> {
    loop {
        match timeout(wait, framed.next()).await {
            Ok(Some(Ok(Frame::Exit { code }))) => return Some(code),
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            _ => return None,
        }
    }
}

#[tokio::test]
async fn server_spawns_shell_and_relays_output() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let socket_path = unique_sock("output");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let server =
        tokio::spawn(
            async move { ttyleport::server::run(&path, Arc::new(OnceLock::new())).await },
        );

    let mut framed = connect_to_server(&socket_path).await;
    // connect_to_server already verified shell output arrived
    read_available_data(&mut framed, Duration::from_millis(500)).await;

    // Send exit to cleanly close
    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn server_relays_command_output() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let socket_path = unique_sock("echo");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let server =
        tokio::spawn(
            async move { ttyleport::server::run(&path, Arc::new(OnceLock::new())).await },
        );

    let mut framed = connect_to_server(&socket_path).await;

    // Drain initial prompt
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Send a command whose output we can identify
    framed
        .send(Frame::Data(Bytes::from("echo TTYLEPORT_TEST_OK\n")))
        .await
        .unwrap();

    // Read output — should contain our marker string
    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("TTYLEPORT_TEST_OK"),
        "expected command output in relay, got: {output_str}"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn server_sends_exit_frame_on_shell_exit() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let socket_path = unique_sock("exit");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let _server =
        tokio::spawn(
            async move { ttyleport::server::run(&path, Arc::new(OnceLock::new())).await },
        );

    let mut framed = connect_to_server(&socket_path).await;

    // Drain initial prompt
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Tell shell to exit with specific code
    framed
        .send(Frame::Data(Bytes::from("exit 42\n")))
        .await
        .unwrap();

    let code = expect_exit_frame(&mut framed, Duration::from_secs(5)).await;
    assert_eq!(code, Some(42), "expected exit code 42");
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn reconnect_preserves_shell_session() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let socket_path = unique_sock("reconnect");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let server =
        tokio::spawn(
            async move { ttyleport::server::run(&path, Arc::new(OnceLock::new())).await },
        );

    // First connection: set a variable in the shell
    let mut framed = connect_to_server(&socket_path).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed
        .send(Frame::Data(Bytes::from("export TTYLEPORT_MARKER=alive\n")))
        .await
        .unwrap();
    read_available_data(&mut framed, Duration::from_millis(500)).await;

    // Disconnect by dropping the framed stream
    drop(framed);

    // Give server time to notice disconnect and re-accept
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Second connection: verify the variable is still set
    let stream = UnixStream::connect(&socket_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    framed
        .send(Frame::Resize { cols: 80, rows: 24 })
        .await
        .unwrap();

    // Drain any buffered output
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed
        .send(Frame::Data(Bytes::from("echo $TTYLEPORT_MARKER\n")))
        .await
        .unwrap();

    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("alive"),
        "shell session should persist across reconnect, got: {output_str}"
    );

    // Clean up
    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn server_exits_when_shell_dies_while_disconnected() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let socket_path = unique_sock("shell-dies");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let server =
        tokio::spawn(
            async move { ttyleport::server::run(&path, Arc::new(OnceLock::new())).await },
        );

    // Connect and tell shell to exit after a delay, then disconnect
    let mut framed = connect_to_server(&socket_path).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Schedule shell to exit after we disconnect
    framed
        .send(Frame::Data(Bytes::from("sleep 0.5 && exit 0\n")))
        .await
        .unwrap();
    read_available_data(&mut framed, Duration::from_millis(200)).await;

    // Disconnect
    drop(framed);

    // Server should exit once the shell dies (within a few seconds)
    let result = timeout(Duration::from_secs(5), server).await;
    assert!(
        result.is_ok(),
        "server should exit after shell dies while disconnected"
    );
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn second_client_detaches_first() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    let socket_path = unique_sock("takeover");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let server = tokio::spawn(async move {
        ttyleport::server::run(&path, Arc::new(OnceLock::new())).await
    });

    // First client connects
    let mut client1 = connect_to_server(&socket_path).await;
    read_available_data(&mut client1, Duration::from_secs(1)).await;

    // Second client connects — should take over the session
    let stream2 = UnixStream::connect(&socket_path).await.unwrap();
    let mut client2 = Framed::new(stream2, FrameCodec);
    client2
        .send(Frame::Resize { cols: 80, rows: 24 })
        .await
        .unwrap();

    // First client should receive Detached
    let mut got_detached = false;
    loop {
        match timeout(Duration::from_secs(3), client1.next()).await {
            Ok(Some(Ok(Frame::Detached))) => {
                got_detached = true;
                break;
            }
            Ok(Some(Ok(Frame::Data(_)))) => continue,
            _ => break,
        }
    }
    assert!(got_detached, "first client should receive Detached frame");

    // Second client should be able to interact with the shell
    read_available_data(&mut client2, Duration::from_secs(1)).await;

    client2
        .send(Frame::Data(Bytes::from("echo TAKEOVER_OK\n")))
        .await
        .unwrap();

    let output = read_available_data(&mut client2, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("TAKEOVER_OK"),
        "second client should be able to use the session, got: {output_str}"
    );

    // Clean up
    let _ = client2.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
    let _ = std::fs::remove_file(&socket_path);
}

// ─── New hardening tests ────────────────────────────────────────────────────

#[tokio::test]
async fn exit_code_zero_sends_exit_frame() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    // Regression test: PTY EOF/EIO race previously caused server to close
    // without sending Exit frame when exit code was 0.
    let socket_path = unique_sock("exit0");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let _server =
        tokio::spawn(
            async move { ttyleport::server::run(&path, Arc::new(OnceLock::new())).await },
        );

    let mut framed = connect_to_server(&socket_path).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed
        .send(Frame::Data(Bytes::from("exit 0\n")))
        .await
        .unwrap();

    let code = expect_exit_frame(&mut framed, Duration::from_secs(5)).await;
    assert_eq!(code, Some(0), "expected exit code 0");
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn exit_code_signal_death() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    // Shell killed by SIGKILL — exit code should be non-zero
    let socket_path = unique_sock("sigkill");
    let _ = std::fs::remove_file(&socket_path);

    let meta = Arc::new(OnceLock::new());
    let meta_clone = Arc::clone(&meta);
    let path = socket_path.clone();
    let _server = tokio::spawn(async move { ttyleport::server::run(&path, meta_clone).await });

    let mut framed = connect_to_server(&socket_path).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Wait for metadata to be available so we can get the shell PID
    tokio::time::sleep(Duration::from_millis(100)).await;
    let shell_pid = meta.get().map(|m| m.shell_pid).unwrap_or(0);
    assert!(shell_pid > 0, "should have shell PID in metadata");

    // Kill the shell with SIGKILL from outside
    unsafe {
        libc::kill(shell_pid as i32, libc::SIGKILL);
    }

    // Should get an Exit frame (code will be non-zero since killed by signal)
    let code = expect_exit_frame(&mut framed, Duration::from_secs(5)).await;
    assert!(code.is_some(), "expected Exit frame after SIGKILL");
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn rapid_reconnect_cycles() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    // Rapidly disconnect and reconnect 3 times, shell should survive all.
    let socket_path = unique_sock("rapid-reconnect");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let server =
        tokio::spawn(
            async move { ttyleport::server::run(&path, Arc::new(OnceLock::new())).await },
        );

    // First connection: set a marker
    let mut framed = connect_to_server(&socket_path).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;
    framed
        .send(Frame::Data(Bytes::from(
            "export RAPID_TEST_MARKER=survived\n",
        )))
        .await
        .unwrap();
    read_available_data(&mut framed, Duration::from_millis(500)).await;

    // Rapidly disconnect and reconnect 3 times
    for i in 0..3 {
        drop(framed);
        tokio::time::sleep(Duration::from_millis(200)).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap_or_else(|e| {
            panic!("reconnect {i} failed: {e}");
        });
        framed = Framed::new(stream, FrameCodec);
        framed
            .send(Frame::Resize { cols: 80, rows: 24 })
            .await
            .unwrap();
        read_available_data(&mut framed, Duration::from_millis(500)).await;
    }

    // Verify marker still set after all cycles
    framed
        .send(Frame::Data(Bytes::from("echo $RAPID_TEST_MARKER\n")))
        .await
        .unwrap();
    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("survived"),
        "shell should survive rapid reconnect cycles, got: {output_str}"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn control_frame_on_session_socket_is_ignored() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    // Sending control frames (ListSessions, KillServer, etc.) to a session
    // socket should be silently ignored, not crash the server.
    let socket_path = unique_sock("ctrl-on-session");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let server =
        tokio::spawn(
            async move { ttyleport::server::run(&path, Arc::new(OnceLock::new())).await },
        );

    let mut framed = connect_to_server(&socket_path).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Send various control frames that make no sense on a session socket
    framed.send(Frame::ListSessions).await.unwrap();
    framed.send(Frame::KillServer).await.unwrap();
    framed
        .send(Frame::CreateSession {
            path: "/tmp/bogus.sock".to_string(),
        })
        .await
        .unwrap();

    // Give server a moment to process
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Server should still be alive — verify by running a command
    framed
        .send(Frame::Data(Bytes::from("echo STILL_ALIVE\n")))
        .await
        .unwrap();

    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("STILL_ALIVE"),
        "server should survive control frames on session socket, got: {output_str}"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn pty_buffer_saturation_and_resume() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    // When no client is connected, shell output fills the kernel PTY buffer (~4KB).
    // The shell blocks until a client reconnects and drains it.
    let socket_path = unique_sock("pty-saturation");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let server =
        tokio::spawn(
            async move { ttyleport::server::run(&path, Arc::new(OnceLock::new())).await },
        );

    let mut framed = connect_to_server(&socket_path).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Tell shell to produce output after we disconnect, then set a marker.
    // The `seq` output will fill the PTY buffer, blocking the shell.
    // When we reconnect and drain, the shell unblocks and runs the echo.
    framed
        .send(Frame::Data(Bytes::from(
            "{ sleep 0.3; seq 1 2000; echo PTY_DRAINED_OK; } &\n",
        )))
        .await
        .unwrap();
    read_available_data(&mut framed, Duration::from_millis(200)).await;

    // Disconnect
    drop(framed);

    // Wait for output to start filling the PTY buffer
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Reconnect and drain
    let stream = UnixStream::connect(&socket_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    framed
        .send(Frame::Resize { cols: 80, rows: 24 })
        .await
        .unwrap();

    // Drain all buffered + new output
    let output = read_available_data(&mut framed, Duration::from_secs(3)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("PTY_DRAINED_OK"),
        "shell should resume after PTY buffer drain, got last 200 chars: {}",
        &output_str[output_str.len().saturating_sub(200)..]
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn resize_propagates_to_pty() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    // Verify that sending a Resize frame actually changes the PTY window size.
    let socket_path = unique_sock("resize");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let server =
        tokio::spawn(
            async move { ttyleport::server::run(&path, Arc::new(OnceLock::new())).await },
        );

    let mut framed = connect_to_server(&socket_path).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Set a specific window size
    framed
        .send(Frame::Resize { cols: 132, rows: 43 })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Query the PTY size via stty
    framed
        .send(Frame::Data(Bytes::from("stty size\n")))
        .await
        .unwrap();
    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("43 132"),
        "PTY should reflect resize (43 rows, 132 cols), got: {output_str}"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn metadata_reflects_attached_state() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    // Verify SessionMetadata.attached flag toggles on connect/disconnect.
    let socket_path = unique_sock("meta-attached");
    let _ = std::fs::remove_file(&socket_path);

    let meta = Arc::new(OnceLock::new());
    let meta_clone = Arc::clone(&meta);
    let path = socket_path.clone();
    let server = tokio::spawn(async move { ttyleport::server::run(&path, meta_clone).await });

    // Before connect: not attached
    tokio::time::sleep(Duration::from_millis(300)).await;
    let m = meta.get().expect("metadata should be set after server starts");
    assert!(
        !m.attached.load(std::sync::atomic::Ordering::Relaxed),
        "should not be attached before client connects"
    );

    // Connect
    let mut framed = connect_to_server(&socket_path).await;
    read_available_data(&mut framed, Duration::from_millis(500)).await;
    assert!(
        m.attached.load(std::sync::atomic::Ordering::Relaxed),
        "should be attached after client connects"
    );

    // Disconnect
    drop(framed);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !m.attached.load(std::sync::atomic::Ordering::Relaxed),
        "should not be attached after client disconnects"
    );

    // Reconnect and clean up
    let stream = UnixStream::connect(&socket_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    framed
        .send(Frame::Resize { cols: 80, rows: 24 })
        .await
        .unwrap();
    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn client_explicit_exit_frame() {
    let _permit = CONCURRENCY.acquire().await.unwrap();
    // Client sends Frame::Exit — server treats it like disconnect (ClientGone).
    let socket_path = unique_sock("client-exit");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let server =
        tokio::spawn(
            async move { ttyleport::server::run(&path, Arc::new(OnceLock::new())).await },
        );

    let mut framed = connect_to_server(&socket_path).await;
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Send Exit frame from client side
    framed.send(Frame::Exit { code: 0 }).await.unwrap();

    // Give server time to notice
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Server should still be running (it treats Exit as client disconnect).
    // Reconnect to verify shell is alive.
    let stream = UnixStream::connect(&socket_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);
    framed
        .send(Frame::Resize { cols: 80, rows: 24 })
        .await
        .unwrap();
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    framed
        .send(Frame::Data(Bytes::from("echo EXIT_FRAME_OK\n")))
        .await
        .unwrap();
    let output = read_available_data(&mut framed, Duration::from_secs(2)).await;
    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains("EXIT_FRAME_OK"),
        "shell should survive client Exit frame, got: {output_str}"
    );

    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
    let _ = std::fs::remove_file(&socket_path);
}
