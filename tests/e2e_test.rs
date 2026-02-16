use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::timeout;
use tokio_util::codec::Framed;
use ttyleport::protocol::{Frame, FrameCodec};

async fn connect_to_server(socket_path: &std::path::Path) -> Framed<UnixStream, FrameCodec> {
    // Give server time to bind
    tokio::time::sleep(Duration::from_millis(200)).await;

    let stream = UnixStream::connect(socket_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);

    // Send initial resize (server expects this)
    framed
        .send(Frame::Resize { cols: 80, rows: 24 })
        .await
        .unwrap();

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

#[tokio::test]
async fn server_spawns_shell_and_relays_output() {
    let socket_path = std::env::temp_dir().join("ttyleport-test-output.sock");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let server = tokio::spawn(async move { ttyleport::server::run(&path, Arc::new(OnceLock::new())).await });

    let mut framed = connect_to_server(&socket_path).await;

    // Should receive some shell output (prompt or motd)
    let initial = read_available_data(&mut framed, Duration::from_secs(2)).await;
    assert!(!initial.is_empty(), "expected shell output after connect");

    // Send exit to cleanly close
    let _ = framed.send(Frame::Data(Bytes::from("exit\n"))).await;
    let _ = timeout(Duration::from_secs(3), server).await;
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn server_relays_command_output() {
    let socket_path = std::env::temp_dir().join("ttyleport-test-echo.sock");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let server = tokio::spawn(async move { ttyleport::server::run(&path, Arc::new(OnceLock::new())).await });

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
    let socket_path = std::env::temp_dir().join("ttyleport-test-exit.sock");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let _server = tokio::spawn(async move { ttyleport::server::run(&path, Arc::new(OnceLock::new())).await });

    let mut framed = connect_to_server(&socket_path).await;

    // Drain initial prompt
    read_available_data(&mut framed, Duration::from_secs(1)).await;

    // Tell shell to exit with specific code
    framed
        .send(Frame::Data(Bytes::from("exit 42\n")))
        .await
        .unwrap();

    // Read until we get an Exit frame or stream ends
    let mut got_exit = false;
    loop {
        match timeout(Duration::from_secs(3), framed.next()).await {
            Ok(Some(Ok(Frame::Exit { code }))) => {
                assert_eq!(code, 42, "expected exit code 42, got {code}");
                got_exit = true;
                break;
            }
            Ok(Some(Ok(Frame::Data(_)))) => continue, // skip output
            _ => break,
        }
    }
    assert!(got_exit, "expected Exit frame from server");
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn reconnect_preserves_shell_session() {
    let socket_path = std::env::temp_dir().join("ttyleport-test-reconnect.sock");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let server = tokio::spawn(async move { ttyleport::server::run(&path, Arc::new(OnceLock::new())).await });

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
    let socket_path = std::env::temp_dir().join("ttyleport-test-shell-dies.sock");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let server = tokio::spawn(async move { ttyleport::server::run(&path, Arc::new(OnceLock::new())).await });

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
