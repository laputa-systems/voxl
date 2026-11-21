//! The helper's connection loop. A protocol or encode failure is written back as an
//! Error message and closes that connection; only a Shutdown ends the process.

use crate::protocol::{self, HelperMessage, ParentMessage, ReadError, ERR_CONFIG, ERR_PROTOCOL};
use crate::session::{Session, SessionConfig};
use std::io::{self, Write};
use std::os::unix::net::{UnixListener, UnixStream};

pub fn run(listener: UnixListener) -> io::Result<()> {
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if let Ok(true) = serve_connection(&mut stream) {
                    return Ok(());
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

fn serve_connection(stream: &mut UnixStream) -> io::Result<bool> {
    let mut session: Option<Session> = None;
    loop {
        let msg = match protocol::read_parent_message(stream) {
            Ok(msg) => msg,
            Err(ReadError::Eof) => return Ok(false),
            Err(e) => {
                send_error(stream, ERR_PROTOCOL, &e.text());
                return Ok(false);
            }
        };
        match msg {
            ParentMessage::Configure(c) => {
                if session.is_some() {
                    send_error(stream, ERR_CONFIG, "helper is already configured");
                    return Ok(false);
                }
                match SessionConfig::from_configure(&c).and_then(Session::new) {
                    Ok(s) => session = Some(s),
                    Err(e) => {
                        send_error(stream, e.code(), e.message());
                        return Ok(false);
                    }
                }
            }
            ParentMessage::Frame(frame) => {
                let Some(s) = session.as_mut() else {
                    send_error(stream, ERR_PROTOCOL, "frame before configure");
                    return Ok(false);
                };
                match s.handle_frame(&frame) {
                    Ok(Some(out)) => {
                        let msg = HelperMessage::AccessUnit {
                            generation: out.generation,
                            keyframe: out.keyframe,
                            data: out.bytes,
                        };
                        if protocol::write_helper_message(stream, &msg).is_err() {
                            return Ok(false);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        send_error(stream, e.code(), e.message());
                        return Ok(false);
                    }
                }
            }
            ParentMessage::RequestKeyframe => match session.as_mut() {
                Some(s) => s.request_keyframe(),
                None => {
                    send_error(stream, ERR_PROTOCOL, "keyframe request before configure");
                    return Ok(false);
                }
            },
            ParentMessage::Shutdown => return Ok(true),
        }
    }
}

fn send_error(stream: &mut UnixStream, code: u16, message: &str) {
    let msg = HelperMessage::Error {
        code,
        message: message.to_string(),
    };
    let _ = protocol::write_helper_message(stream, &msg);
    let _ = stream.flush();
}
