//! Length-prefixed little-endian messages over a Unix stream socket. See
//! docs/bctl-encoder.md for the printed contract.

use crate::damage::Rect;
use std::io::{self, ErrorKind, Read, Write};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_MESSAGE_BYTES: usize = 64 << 20;

pub const ERR_PROTOCOL: u16 = 1;
pub const ERR_CONFIG: u16 = 2;
pub const ERR_ENCODE: u16 = 3;

const MSG_CONFIGURE: u8 = 1;
const MSG_FRAME: u8 = 2;
const MSG_REQUEST_KEYFRAME: u8 = 3;
const MSG_SHUTDOWN: u8 = 4;

const MSG_ACCESS_UNIT: u8 = 1;
const MSG_ERROR: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Bgrx,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Configure {
    pub version: u16,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: PixelFormat,
    pub fps: u32,
    pub crf: f32,
    pub vbv_kbps: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rows {
    pub y0: u32,
    pub y1: u32,
    pub row_bytes: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub generation: u64,
    pub full: bool,
    pub damage: Vec<Rect>,
    pub rows: Rows,
    pub pixels: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParentMessage {
    Configure(Configure),
    Frame(Frame),
    RequestKeyframe,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelperMessage {
    AccessUnit {
        generation: u64,
        keyframe: bool,
        data: Vec<u8>,
    },
    Error {
        code: u16,
        message: String,
    },
}

#[derive(Debug)]
pub enum ReadError {
    Eof,
    Io(io::Error),
    Protocol(String),
}

impl ReadError {
    pub fn text(&self) -> String {
        match self {
            ReadError::Eof => "connection closed".into(),
            ReadError::Io(e) => e.to_string(),
            ReadError::Protocol(m) => m.clone(),
        }
    }
}

impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self {
        ReadError::Io(e)
    }
}

pub fn write_parent_message<W: Write>(w: &mut W, msg: &ParentMessage) -> io::Result<()> {
    write_payload(w, &parent_payload(msg))
}

pub fn write_helper_message<W: Write>(w: &mut W, msg: &HelperMessage) -> io::Result<()> {
    write_payload(w, &helper_payload(msg))
}

fn write_payload<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "message too long"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)
}

pub fn parent_payload(msg: &ParentMessage) -> Vec<u8> {
    let mut b = Vec::new();
    match msg {
        ParentMessage::Configure(c) => {
            b.push(MSG_CONFIGURE);
            put_u16(&mut b, c.version);
            put_u32(&mut b, c.width);
            put_u32(&mut b, c.height);
            put_u32(&mut b, c.stride);
            b.push(match c.pixel_format {
                PixelFormat::Bgrx => 0,
            });
            put_u32(&mut b, c.fps);
            put_u32(&mut b, c.crf.to_bits());
            put_u32(&mut b, c.vbv_kbps);
        }
        ParentMessage::Frame(f) => {
            b.push(MSG_FRAME);
            put_u64(&mut b, f.generation);
            b.push(u8::from(f.full));
            put_u16(&mut b, f.damage.len() as u16);
            for r in &f.damage {
                put_u32(&mut b, r.x);
                put_u32(&mut b, r.y);
                put_u32(&mut b, r.w);
                put_u32(&mut b, r.h);
            }
            put_u32(&mut b, f.rows.y0);
            put_u32(&mut b, f.rows.y1);
            put_u32(&mut b, f.rows.row_bytes);
            b.extend_from_slice(&f.pixels);
        }
        ParentMessage::RequestKeyframe => b.push(MSG_REQUEST_KEYFRAME),
        ParentMessage::Shutdown => b.push(MSG_SHUTDOWN),
    }
    b
}

pub fn helper_payload(msg: &HelperMessage) -> Vec<u8> {
    let mut b = Vec::new();
    match msg {
        HelperMessage::AccessUnit {
            generation,
            keyframe,
            data,
        } => {
            b.push(MSG_ACCESS_UNIT);
            put_u64(&mut b, *generation);
            b.push(u8::from(*keyframe));
            put_u32(&mut b, data.len() as u32);
            b.extend_from_slice(data);
        }
        HelperMessage::Error { code, message } => {
            b.push(MSG_ERROR);
            put_u16(&mut b, *code);
            let bytes = message.as_bytes();
            put_u16(&mut b, bytes.len() as u16);
            b.extend_from_slice(bytes);
        }
    }
    b
}

pub fn read_parent_message<R: Read>(r: &mut R) -> Result<ParentMessage, ReadError> {
    let Some(payload) = read_frame(r)? else {
        return Err(ReadError::Eof);
    };
    let mut c = Cursor::new(&payload);
    let kind = c.u8()?;
    let msg = match kind {
        MSG_CONFIGURE => {
            let version = c.u16()?;
            let width = c.u32()?;
            let height = c.u32()?;
            let stride = c.u32()?;
            let pixel_format = match c.u8()? {
                0 => PixelFormat::Bgrx,
                other => return Err(protocol(format!("unknown pixel format {other}"))),
            };
            let fps = c.u32()?;
            let crf = f32::from_bits(c.u32()?);
            let vbv_kbps = c.u32()?;
            ParentMessage::Configure(Configure {
                version,
                width,
                height,
                stride,
                pixel_format,
                fps,
                crf,
                vbv_kbps,
            })
        }
        MSG_FRAME => {
            let generation = c.u64()?;
            let full = match c.u8()? {
                0 => false,
                1 => true,
                other => return Err(protocol(format!("full flag is {other}, not 0 or 1"))),
            };
            let count = c.u16()? as usize;
            let mut damage = Vec::with_capacity(count);
            for _ in 0..count {
                let r = Rect {
                    x: c.u32()?,
                    y: c.u32()?,
                    w: c.u32()?,
                    h: c.u32()?,
                };
                if r.w == 0 || r.h == 0 {
                    return Err(protocol(format!("damage rect {r:?} is empty")));
                }
                damage.push(r);
            }
            let y0 = c.u32()?;
            let y1 = c.u32()?;
            let row_bytes = c.u32()?;
            if y1 < y0 {
                return Err(protocol(format!("row range {y0}..{y1} runs backwards")));
            }
            let expected = (y1 - y0) as usize;
            let expected = expected
                .checked_mul(row_bytes as usize)
                .ok_or_else(|| protocol("pixel row length overflows".into()))?;
            let pixels = c.take(expected)?.to_vec();
            ParentMessage::Frame(Frame {
                generation,
                full,
                damage,
                rows: Rows {
                    y0,
                    y1,
                    row_bytes,
                },
                pixels,
            })
        }
        MSG_REQUEST_KEYFRAME => ParentMessage::RequestKeyframe,
        MSG_SHUTDOWN => ParentMessage::Shutdown,
        other => return Err(protocol(format!("unknown message type {other}"))),
    };
    c.done()?;
    Ok(msg)
}

pub fn read_helper_message<R: Read>(r: &mut R) -> Result<HelperMessage, ReadError> {
    let Some(payload) = read_frame(r)? else {
        return Err(ReadError::Eof);
    };
    let mut c = Cursor::new(&payload);
    let kind = c.u8()?;
    let msg = match kind {
        MSG_ACCESS_UNIT => {
            let generation = c.u64()?;
            let keyframe = match c.u8()? {
                0 => false,
                1 => true,
                other => return Err(protocol(format!("keyframe flag is {other}, not 0 or 1"))),
            };
            let len = c.u32()? as usize;
            let data = c.take(len)?.to_vec();
            HelperMessage::AccessUnit {
                generation,
                keyframe,
                data,
            }
        }
        MSG_ERROR => {
            let code = c.u16()?;
            let len = c.u16()? as usize;
            let bytes = c.take(len)?;
            let message = std::str::from_utf8(bytes)
                .map_err(|e| protocol(format!("error message is not UTF-8: {e}")))?
                .to_string();
            HelperMessage::Error { code, message }
        }
        other => return Err(protocol(format!("unknown message type {other}"))),
    };
    c.done()?;
    Ok(msg)
}

fn read_frame<R: Read>(r: &mut R) -> Result<Option<Vec<u8>>, ReadError> {
    let mut len = [0u8; 4];
    let got = fill(r, &mut len)?;
    if got == 0 {
        return Ok(None);
    }
    if got < len.len() {
        return Err(protocol("truncated length prefix".into()));
    }
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 {
        return Err(protocol("empty message".into()));
    }
    if len > MAX_MESSAGE_BYTES {
        return Err(protocol(format!(
            "message of {len} bytes exceeds the {MAX_MESSAGE_BYTES}-byte limit"
        )));
    }
    let mut payload = vec![0u8; len];
    let got = fill(r, &mut payload)?;
    if got < len {
        return Err(protocol(format!(
            "truncated message: {got} of {len} bytes"
        )));
    }
    Ok(Some(payload))
}

fn fill<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut off = 0;
    while off < buf.len() {
        match r.read(&mut buf[off..]) {
            Ok(0) => return Ok(off),
            Ok(n) => off += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(off)
}

fn protocol(m: String) -> ReadError {
    ReadError::Protocol(m)
}

struct Cursor<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, at: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ReadError> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| protocol("message offset overflows".into()))?;
        if end > self.buf.len() {
            return Err(protocol(format!(
                "message ends at {} bytes, field needs {}",
                self.buf.len(),
                end
            )));
        }
        let out = &self.buf[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, ReadError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ReadError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, ReadError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, ReadError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn done(&self) -> Result<(), ReadError> {
        if self.at != self.buf.len() {
            return Err(protocol(format!(
                "message has {} trailing bytes",
                self.buf.len() - self.at
            )));
        }
        Ok(())
    }
}

fn put_u16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor as IoCursor;

    fn framed(payload: Vec<u8>) -> Vec<u8> {
        let mut b = (payload.len() as u32).to_le_bytes().to_vec();
        b.extend_from_slice(&payload);
        b
    }

    fn round_parent(msg: ParentMessage) -> ParentMessage {
        let bytes = framed(parent_payload(&msg));
        read_parent_message(&mut IoCursor::new(bytes)).unwrap()
    }

    fn round_helper(msg: HelperMessage) -> HelperMessage {
        let bytes = framed(helper_payload(&msg));
        read_helper_message(&mut IoCursor::new(bytes)).unwrap()
    }

    #[test]
    fn parent_messages_round_trip() {
        let configure = ParentMessage::Configure(Configure {
            version: PROTOCOL_VERSION,
            width: 1280,
            height: 800,
            stride: 5120,
            pixel_format: PixelFormat::Bgrx,
            fps: 30,
            crf: 24.0,
            vbv_kbps: 8000,
        });
        assert_eq!(round_parent(configure.clone()), configure);
        let frame = ParentMessage::Frame(Frame {
            generation: 7,
            full: false,
            damage: vec![Rect {
                x: 10,
                y: 20,
                w: 30,
                h: 40,
            }],
            rows: Rows {
                y0: 16,
                y1: 64,
                row_bytes: 5120,
            },
            pixels: vec![9; 48 * 5120],
        });
        assert_eq!(round_parent(frame.clone()), frame);
        assert_eq!(
            round_parent(ParentMessage::RequestKeyframe),
            ParentMessage::RequestKeyframe
        );
        assert_eq!(
            round_parent(ParentMessage::Shutdown),
            ParentMessage::Shutdown
        );
    }

    #[test]
    fn helper_messages_round_trip() {
        let au = HelperMessage::AccessUnit {
            generation: 3,
            keyframe: true,
            data: vec![0, 0, 0, 1, 0x67],
        };
        assert_eq!(round_helper(au.clone()), au);
        let err = HelperMessage::Error {
            code: ERR_PROTOCOL,
            message: "bad".into(),
        };
        assert_eq!(round_helper(err.clone()), err);
    }

    #[test]
    fn truncated_lengths_and_payloads_are_protocol_errors() {
        let mut r = IoCursor::new(vec![5, 0]);
        assert!(matches!(
            read_parent_message(&mut r),
            Err(ReadError::Protocol(_))
        ));
        let mut r = IoCursor::new(vec![8, 0, 0, 0, 2, 3]);
        assert!(matches!(
            read_parent_message(&mut r),
            Err(ReadError::Protocol(_))
        ));
    }

    #[test]
    fn a_clean_boundary_is_eof_not_an_error() {
        let mut r = IoCursor::new(Vec::new());
        assert!(matches!(read_parent_message(&mut r), Err(ReadError::Eof)));
    }

    #[test]
    fn oversized_empty_and_unknown_messages_are_rejected() {
        let mut r = IoCursor::new(0u32.to_le_bytes().to_vec());
        assert!(matches!(
            read_parent_message(&mut r),
            Err(ReadError::Protocol(_))
        ));
        let mut r = IoCursor::new(((MAX_MESSAGE_BYTES + 1) as u32).to_le_bytes().to_vec());
        assert!(matches!(
            read_parent_message(&mut r),
            Err(ReadError::Protocol(_))
        ));
        let mut r = IoCursor::new({
            let p = [99u8];
            let mut b = (p.len() as u32).to_le_bytes().to_vec();
            b.extend_from_slice(&p);
            b
        });
        assert!(matches!(
            read_parent_message(&mut r),
            Err(ReadError::Protocol(_))
        ));
    }

    #[test]
    fn trailing_bytes_and_wrong_pixel_counts_are_rejected() {
        let mut b = parent_payload(&ParentMessage::Configure(Configure {
            version: PROTOCOL_VERSION,
            width: 16,
            height: 16,
            stride: 64,
            pixel_format: PixelFormat::Bgrx,
            fps: 30,
            crf: 24.0,
            vbv_kbps: 0,
        }));
        b.push(0);
        let mut framed = (b.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(&b);
        assert!(matches!(
            read_parent_message(&mut IoCursor::new(framed)),
            Err(ReadError::Protocol(_))
        ));

        let mut b = parent_payload(&ParentMessage::Frame(Frame {
            generation: 1,
            full: false,
            damage: vec![],
            rows: Rows {
                y0: 0,
                y1: 16,
                row_bytes: 64,
            },
            pixels: vec![0; 10],
        }));
        b.truncate(b.len());
        let mut framed = (b.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(&b);
        assert!(matches!(
            read_parent_message(&mut IoCursor::new(framed)),
            Err(ReadError::Protocol(_))
        ));
    }
}
