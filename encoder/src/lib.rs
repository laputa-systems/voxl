//! Voxl's encoder core without capture, transport or audio, driven by damaged BGRX
//! rows over a Unix socket.

#![deny(unsafe_code)]

pub mod convert;
pub mod damage;
pub mod protocol;
pub mod session;
pub mod settle;

// Voxl's x264 boundary: x264 is GPL-2.0-or-later, so only this crate's helper
// binary links it. See docs/bctl-encoder.md.
#[allow(unsafe_code)]
mod x264_ffi;

#[cfg(unix)]
pub mod serve;

pub use damage::Rect;
pub use protocol::{
    Configure, Frame, HelperMessage, ParentMessage, PixelFormat, Rows, ERR_CONFIG, ERR_ENCODE,
    ERR_PROTOCOL, PROTOCOL_VERSION,
};
pub use session::{EncodeOutput, Session, SessionConfig, SessionError, PAINT_OVER_CRF};
