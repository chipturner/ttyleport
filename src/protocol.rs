use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::io;
use tokio_util::codec::{Decoder, Encoder};

const TYPE_DATA: u8 = 0x01;
const TYPE_RESIZE: u8 = 0x02;
const TYPE_EXIT: u8 = 0x03;
const TYPE_DETACHED: u8 = 0x04;
const TYPE_CREATE_SESSION: u8 = 0x10;
const TYPE_LIST_SESSIONS: u8 = 0x11;
const TYPE_SESSION_INFO: u8 = 0x12;
const TYPE_OK: u8 = 0x13;
const TYPE_ERROR: u8 = 0x14;
const TYPE_KILL_SESSION: u8 = 0x15;
const TYPE_KILL_SERVER: u8 = 0x16;

const HEADER_LEN: usize = 5; // type(1) + length(4)

/// Metadata for one session, returned in SessionInfo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub path: String,
    pub pty_path: String,
    pub shell_pid: u32,
    pub created_at: u64,
    pub attached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Data(Bytes),
    Resize { cols: u16, rows: u16 },
    Exit { code: i32 },
    /// Sent to a client when another client takes over the session.
    Detached,
    // Control frames
    CreateSession { path: String },
    ListSessions,
    SessionInfo { sessions: Vec<SessionEntry> },
    Ok,
    Error { message: String },
    KillSession { path: String },
    KillServer,
}

pub struct FrameCodec;

impl Decoder for FrameCodec {
    type Item = Frame;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, io::Error> {
        if src.len() < HEADER_LEN {
            return Ok(None);
        }

        let frame_type = src[0];
        let payload_len = u32::from_be_bytes([src[1], src[2], src[3], src[4]]) as usize;

        if src.len() < HEADER_LEN + payload_len {
            src.reserve(HEADER_LEN + payload_len - src.len());
            return Ok(None);
        }

        src.advance(HEADER_LEN);
        let payload = src.split_to(payload_len);

        match frame_type {
            TYPE_DATA => Ok(Some(Frame::Data(payload.freeze()))),
            TYPE_RESIZE => {
                if payload.len() != 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "resize frame must be 4 bytes",
                    ));
                }
                let cols = u16::from_be_bytes([payload[0], payload[1]]);
                let rows = u16::from_be_bytes([payload[2], payload[3]]);
                Ok(Some(Frame::Resize { cols, rows }))
            }
            TYPE_EXIT => {
                if payload.len() != 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "exit frame must be 4 bytes",
                    ));
                }
                let code = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                Ok(Some(Frame::Exit { code }))
            }
            TYPE_DETACHED => Ok(Some(Frame::Detached)),
            TYPE_CREATE_SESSION => {
                let path = String::from_utf8(payload.to_vec()).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, e)
                })?;
                Ok(Some(Frame::CreateSession { path }))
            }
            TYPE_LIST_SESSIONS => Ok(Some(Frame::ListSessions)),
            TYPE_SESSION_INFO => {
                let text = String::from_utf8(payload.to_vec()).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, e)
                })?;
                let sessions = if text.is_empty() {
                    Vec::new()
                } else {
                    text.lines()
                        .filter_map(|line| {
                            let parts: Vec<&str> = line.split('\t').collect();
                            if parts.len() == 5 {
                                Some(SessionEntry {
                                    path: parts[0].to_string(),
                                    pty_path: parts[1].to_string(),
                                    shell_pid: parts[2].parse().unwrap_or(0),
                                    created_at: parts[3].parse().unwrap_or(0),
                                    attached: parts[4] == "1",
                                })
                            } else {
                                None
                            }
                        })
                        .collect()
                };
                Ok(Some(Frame::SessionInfo { sessions }))
            }
            TYPE_OK => Ok(Some(Frame::Ok)),
            TYPE_ERROR => {
                let message = String::from_utf8(payload.to_vec()).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, e)
                })?;
                Ok(Some(Frame::Error { message }))
            }
            TYPE_KILL_SESSION => {
                let path = String::from_utf8(payload.to_vec()).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, e)
                })?;
                Ok(Some(Frame::KillSession { path }))
            }
            TYPE_KILL_SERVER => Ok(Some(Frame::KillServer)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown frame type: 0x{frame_type:02x}"),
            )),
        }
    }
}

impl Encoder<Frame> for FrameCodec {
    type Error = io::Error;

    fn encode(&mut self, frame: Frame, dst: &mut BytesMut) -> Result<(), io::Error> {
        match frame {
            Frame::Data(data) => {
                dst.put_u8(TYPE_DATA);
                dst.put_u32(data.len() as u32);
                dst.extend_from_slice(&data);
            }
            Frame::Resize { cols, rows } => {
                dst.put_u8(TYPE_RESIZE);
                dst.put_u32(4);
                dst.put_u16(cols);
                dst.put_u16(rows);
            }
            Frame::Exit { code } => {
                dst.put_u8(TYPE_EXIT);
                dst.put_u32(4);
                dst.put_i32(code);
            }
            Frame::Detached => {
                dst.put_u8(TYPE_DETACHED);
                dst.put_u32(0);
            }
            Frame::CreateSession { path } => {
                dst.put_u8(TYPE_CREATE_SESSION);
                dst.put_u32(path.len() as u32);
                dst.extend_from_slice(path.as_bytes());
            }
            Frame::ListSessions => {
                dst.put_u8(TYPE_LIST_SESSIONS);
                dst.put_u32(0);
            }
            Frame::SessionInfo { sessions } => {
                let text: String = sessions
                    .iter()
                    .map(|e| {
                        format!(
                            "{}\t{}\t{}\t{}\t{}",
                            e.path,
                            e.pty_path,
                            e.shell_pid,
                            e.created_at,
                            if e.attached { "1" } else { "0" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                dst.put_u8(TYPE_SESSION_INFO);
                dst.put_u32(text.len() as u32);
                dst.extend_from_slice(text.as_bytes());
            }
            Frame::Ok => {
                dst.put_u8(TYPE_OK);
                dst.put_u32(0);
            }
            Frame::Error { message } => {
                dst.put_u8(TYPE_ERROR);
                dst.put_u32(message.len() as u32);
                dst.extend_from_slice(message.as_bytes());
            }
            Frame::KillSession { path } => {
                dst.put_u8(TYPE_KILL_SESSION);
                dst.put_u32(path.len() as u32);
                dst.extend_from_slice(path.as_bytes());
            }
            Frame::KillServer => {
                dst.put_u8(TYPE_KILL_SERVER);
                dst.put_u32(0);
            }
        }
        Ok(())
    }
}
