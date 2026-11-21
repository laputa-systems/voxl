#![cfg(unix)]

use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use voxl_encoder::protocol::{
    read_helper_message, write_parent_message, Configure, Frame, HelperMessage, ParentMessage,
    PixelFormat, Rows, ERR_PROTOCOL, PROTOCOL_VERSION,
};
use voxl_encoder::Rect;

const W: u32 = 1280;
const H: u32 = 800;
const STRIDE: u32 = W * 4;

struct Helper {
    child: Child,
    path: PathBuf,
    dir: PathBuf,
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

impl Helper {
    fn spawn(name: &str) -> Helper {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("ve-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s");
        let child = Command::new(env!("CARGO_BIN_EXE_voxl-frame-encoder"))
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn voxl-frame-encoder");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "helper did not create its socket at {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        Helper { child, path, dir }
    }

    fn connect(&self) -> UnixStream {
        let s = UnixStream::connect(&self.path).expect("connect to helper socket");
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        s.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
        s
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("helper did not exit");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn configure() -> ParentMessage {
    ParentMessage::Configure(Configure {
        version: PROTOCOL_VERSION,
        width: W,
        height: H,
        stride: STRIDE,
        pixel_format: PixelFormat::Bgrx,
        fps: 30,
        crf: 24.0,
        vbv_kbps: 8000,
    })
}

fn send(stream: &mut UnixStream, msg: &ParentMessage) {
    write_parent_message(stream, msg).unwrap();
    stream.flush().unwrap();
}

fn recv(stream: &mut UnixStream) -> HelperMessage {
    read_helper_message(stream).expect("read helper message")
}

fn background() -> Vec<u8> {
    let mut px = vec![0u8; STRIDE as usize * H as usize];
    for y in 0..H as usize {
        for x in 0..W as usize {
            let i = y * STRIDE as usize + x * 4;
            let v = ((x / 9 + y / 7) % 4) as u8 * 30 + 50;
            px[i] = v;
            px[i + 1] = 255 - v;
            px[i + 2] = (x * 3 + y) as u8;
            px[i + 3] = 255;
        }
    }
    px
}

fn noise(px: &mut [u8], rect: Rect, seed: u64) {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for y in rect.y as usize..(rect.y + rect.h) as usize {
        for x in rect.x as usize..(rect.x + rect.w) as usize {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let i = y * STRIDE as usize + x * 4;
            px[i] = state as u8;
            px[i + 1] = (state >> 8) as u8;
            px[i + 2] = (state >> 16) as u8;
            px[i + 3] = 255;
        }
    }
}

fn full_frame(generation: u64, px: &[u8]) -> ParentMessage {
    ParentMessage::Frame(Frame {
        generation,
        full: true,
        damage: Vec::new(),
        rows: Rows {
            y0: 0,
            y1: H,
            row_bytes: STRIDE,
        },
        pixels: px.to_vec(),
    })
}

fn partial_frame(generation: u64, px: &[u8], rect: Rect) -> ParentMessage {
    let y0 = (rect.y / 16) * 16;
    let y1 = (((rect.y + rect.h).div_ceil(16)) * 16).min(H);
    ParentMessage::Frame(Frame {
        generation,
        full: false,
        damage: vec![rect],
        rows: Rows {
            y0,
            y1,
            row_bytes: STRIDE,
        },
        pixels: px[y0 as usize * STRIDE as usize..y1 as usize * STRIDE as usize].to_vec(),
    })
}

#[test]
fn helper_encodes_thirty_frames_over_the_socket() {
    let mut helper = Helper::spawn("thirty");
    let mut stream = helper.connect();
    send(&mut stream, &configure());

    let mut px = background();
    let mut sizes = Vec::new();
    for generation in 1..=30u64 {
        if generation == 15 {
            send(&mut stream, &ParentMessage::RequestKeyframe);
        }
        if generation == 1 {
            send(&mut stream, &full_frame(generation, &px));
        } else {
            let rect = Rect {
                x: (generation as u32 * 37) % (W - 400),
                y: 100 + (generation as u32 * 13) % (H - 300),
                w: 320,
                h: 120,
            };
            noise(&mut px, rect, generation);
            send(&mut stream, &partial_frame(generation, &px, rect));
        }
        match recv(&mut stream) {
            HelperMessage::AccessUnit {
                generation: got,
                keyframe,
                data,
            } => {
                assert_eq!(got, generation, "access unit for the wrong frame");
                assert!(!data.is_empty(), "empty access unit");
                assert!(data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1]));
                let expect_key = generation == 1 || generation == 15;
                assert_eq!(
                    keyframe, expect_key,
                    "frame {generation} keyframe flag {keyframe}, expected {expect_key}"
                );
                sizes.push(data.len());
            }
            other => panic!("frame {generation}: helper sent {other:?}"),
        }
    }
    assert_eq!(sizes.len(), 30);
    let total: usize = sizes.iter().sum();
    println!(
        "30 frames: {} access units, {} bytes total, first {} bytes, median {} bytes",
        sizes.len(),
        total,
        sizes[0],
        {
            let mut s = sizes.clone();
            s.sort_unstable();
            s[s.len() / 2]
        }
    );

    send(&mut stream, &ParentMessage::Shutdown);
    drop(stream);
    let status = helper.wait();
    assert!(status.success(), "helper exited with {status}");
}

#[test]
fn truncated_and_malformed_messages_error_without_exiting() {
    let mut helper = Helper::spawn("bad");

    let mut stream = helper.connect();
    let mut truncated = 8u32.to_le_bytes().to_vec();
    truncated.extend_from_slice(&[1, 2, 3]);
    stream.write_all(&truncated).unwrap();
    stream.flush().unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    match recv(&mut stream) {
        HelperMessage::Error { code, message } => {
            assert_eq!(code, ERR_PROTOCOL);
            assert!(
                message.contains("truncated"),
                "error did not name the truncation: {message}"
            );
            println!("truncated message rejected: {code} {message}");
        }
        other => panic!("helper sent {other:?} for a truncated message"),
    }
    drop(stream);
    assert!(
        helper.child.try_wait().unwrap().is_none(),
        "helper exited after a truncated message"
    );

    let mut stream = helper.connect();
    let payload = [99u8];
    let mut malformed = (payload.len() as u32).to_le_bytes().to_vec();
    malformed.extend_from_slice(&payload);
    stream.write_all(&malformed).unwrap();
    stream.flush().unwrap();
    match recv(&mut stream) {
        HelperMessage::Error { code, message } => {
            assert_eq!(code, ERR_PROTOCOL);
            assert!(
                message.contains("unknown message type"),
                "unexpected error text: {message}"
            );
            println!("malformed message rejected: {code} {message}");
        }
        other => panic!("helper sent {other:?} for a malformed message"),
    }
    drop(stream);
    assert!(
        helper.child.try_wait().unwrap().is_none(),
        "helper exited after a malformed message"
    );

    let mut stream = helper.connect();
    send(&mut stream, &configure());
    send(&mut stream, &ParentMessage::Shutdown);
    drop(stream);
    let status = helper.wait();
    assert!(status.success(), "helper exited with {status}");
}
