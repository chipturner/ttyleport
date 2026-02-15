use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::timeout;
use tokio_util::codec::Framed;
use ttyleport::protocol::{Frame, FrameCodec};

#[tokio::test]
async fn server_spawns_shell_and_relays_output() {
    let socket_path = std::env::temp_dir().join("ttyleport-test.sock");
    let _ = std::fs::remove_file(&socket_path);

    let path = socket_path.clone();
    let server = tokio::spawn(async move {
        ttyleport::server::run(&path).await
    });

    // Give server time to bind
    tokio::time::sleep(Duration::from_millis(200)).await;

    let stream = UnixStream::connect(&socket_path).await.unwrap();
    let mut framed = Framed::new(stream, FrameCodec);

    // Send initial resize
    framed
        .send(Frame::Resize { cols: 80, rows: 24 })
        .await
        .unwrap();

    // Should receive shell output (prompt)
    let frame = timeout(Duration::from_secs(3), framed.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("decode error");
    assert!(matches!(frame, Frame::Data(_)));

    // Send exit to cleanly close (best-effort — shell may already have exited)
    let _ = framed
        .send(Frame::Data(Bytes::from("exit\n")))
        .await;

    let _ = timeout(Duration::from_secs(3), server).await;
    let _ = std::fs::remove_file(&socket_path);
}
