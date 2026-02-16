use bytes::{Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};
use ttyleport::protocol::{Frame, FrameCodec, SessionEntry};

#[test]
fn encode_data_frame() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec
        .encode(Frame::Data(Bytes::from("hello")), &mut buf)
        .unwrap();
    // type(1) + len(4) + payload(5) = 10
    assert_eq!(buf.len(), 10);
    assert_eq!(buf[0], 0x01);
    assert_eq!(&buf[5..], b"hello");
}

#[test]
fn encode_resize_frame() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec
        .encode(Frame::Resize { cols: 80, rows: 24 }, &mut buf)
        .unwrap();
    // type(1) + len(4) + payload(4) = 9
    assert_eq!(buf.len(), 9);
    assert_eq!(buf[0], 0x02);
}

#[test]
fn encode_exit_frame() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec.encode(Frame::Exit { code: 42 }, &mut buf).unwrap();
    assert_eq!(buf.len(), 9);
    assert_eq!(buf[0], 0x03);
}

#[test]
fn roundtrip_data() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Data(Bytes::from("hello world"));
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_resize() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Resize { cols: 120, rows: 40 };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_exit() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Exit { code: 0 };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn decode_incomplete_returns_none() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::from(&[0x01, 0x00, 0x00][..]);
    assert!(codec.decode(&mut buf).unwrap().is_none());
}

#[test]
fn decode_partial_payload_returns_none() {
    let mut codec = FrameCodec;
    // Header says 5 bytes payload, but only 2 present
    let mut buf = BytesMut::from(&[0x01, 0x00, 0x00, 0x00, 0x05, 0xAA, 0xBB][..]);
    assert!(codec.decode(&mut buf).unwrap().is_none());
}

#[test]
fn decode_invalid_type_returns_error() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::from(&[0xFF, 0x00, 0x00, 0x00, 0x00][..]);
    assert!(codec.decode(&mut buf).is_err());
}

#[test]
fn roundtrip_create_session() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::CreateSession {
        path: "/tmp/test.sock".to_string(),
    };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_list_sessions() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec.encode(Frame::ListSessions, &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(Frame::ListSessions, decoded);
}

#[test]
fn roundtrip_session_info() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::SessionInfo {
        sessions: vec![
            SessionEntry {
                path: "/tmp/a.sock".to_string(),
                pty_path: "/dev/pts/3".to_string(),
                shell_pid: 1234,
                created_at: 1700000000,
                attached: true,
            },
            SessionEntry {
                path: "/tmp/b.sock".to_string(),
                pty_path: "/dev/pts/5".to_string(),
                shell_pid: 5678,
                created_at: 1700000100,
                attached: false,
            },
        ],
    };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_session_info_empty() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::SessionInfo { sessions: vec![] };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_ok() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec.encode(Frame::Ok, &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(Frame::Ok, decoded);
}

#[test]
fn roundtrip_error() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::Error {
        message: "something failed".to_string(),
    };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_detached() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec.encode(Frame::Detached, &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(Frame::Detached, decoded);
}

#[test]
fn roundtrip_kill_session() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    let original = Frame::KillSession {
        path: "/tmp/test.sock".to_string(),
    };
    codec.encode(original.clone(), &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(original, decoded);
}

#[test]
fn roundtrip_kill_server() {
    let mut codec = FrameCodec;
    let mut buf = BytesMut::new();
    codec.encode(Frame::KillServer, &mut buf).unwrap();
    let decoded = codec.decode(&mut buf).unwrap().unwrap();
    assert_eq!(Frame::KillServer, decoded);
}
