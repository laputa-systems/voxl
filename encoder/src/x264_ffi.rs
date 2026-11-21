//! The only module in this crate allowed to use `unsafe`: bindgen bindings for
//! x264 and the safe wrapper around its encoder object.

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code
)]
mod raw {
    include!(concat!(env!("OUT_DIR"), "/x264.rs"));
}

use std::ptr;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub crf: f32,
    pub max_kbps: u32,
}

pub struct Planes<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub stride_y: usize,
    pub stride_uv: usize,
}

pub struct Encoded {
    pub bytes: Vec<u8>,
    pub keyframe: bool,
    pub qp: Option<f32>,
    pub crf_avg: f32,
}

pub struct Encoder {
    h: *mut raw::x264_t,
    param: raw::x264_param_t,
    pic: raw::x264_picture_t,
    pts: i64,
    width: u32,
    height: u32,
    fps: u32,
}

impl Encoder {
    pub fn new(cfg: &Config) -> Result<Self, String> {
        if cfg.width == 0 || cfg.height == 0 || cfg.width % 2 != 0 || cfg.height % 2 != 0 {
            return Err("x264 needs a non-zero even width and height".into());
        }
        unsafe {
            let mut param: raw::x264_param_t = std::mem::zeroed();
            if raw::x264_param_default_preset(
                &mut param,
                c"ultrafast".as_ptr(),
                c"zerolatency".as_ptr(),
            ) < 0
            {
                return Err("bad x264 preset ultrafast/zerolatency".into());
            }
            param.i_width = cfg.width as i32;
            param.i_height = cfg.height as i32;
            param.i_csp = raw::X264_CSP_I420 as i32;
            param.i_fps_num = cfg.fps;
            param.i_fps_den = 1;
            param.i_timebase_num = 1;
            param.i_timebase_den = 1000;
            param.b_vfr_input = 0;
            param.i_threads = 1;
            param.b_sliced_threads = 0;
            param.i_slice_count = 1;
            param.i_keyint_max = raw::X264_KEYINT_MAX_INFINITE as i32;
            param.i_keyint_min = 1;
            param.i_scenecut_threshold = 0;
            param.b_intra_refresh = 0;
            param.i_bframe = 0;
            param.b_annexb = 1;
            param.b_repeat_headers = 1;
            param.b_aud = 0;
            param.i_log_level = raw::X264_LOG_WARNING as i32;
            param.rc.i_rc_method = raw::X264_RC_CRF as i32;
            param.rc.f_rf_constant = cfg.crf;
            param.rc.i_lookahead = 0;
            param.analyse.i_me_method = raw::X264_ME_UMH as i32;
            param.analyse.i_me_range = 64;
            param.analyse.b_mb_info = 1;
            param.i_sync_lookahead = 0;
            param.rc.b_mb_tree = 0;
            if cfg.max_kbps > 0 {
                param.rc.i_vbv_max_bitrate = cfg.max_kbps as i32;
                param.rc.i_vbv_buffer_size = vbv_buffer_kbit(cfg.max_kbps, cfg.fps) as i32;
            }
            param.vui.i_colorprim = 1;
            param.vui.i_transfer = 13;
            param.vui.i_colmatrix = 6;
            param.vui.b_fullrange = 0;
            if raw::x264_param_apply_profile(&mut param, c"high".as_ptr()) < 0 {
                return Err("x264 high profile unavailable".into());
            }
            let h = raw::x264_encoder_open(&mut param);
            if h.is_null() {
                return Err("x264_encoder_open failed".into());
            }
            let mut pic: raw::x264_picture_t = std::mem::zeroed();
            raw::x264_picture_init(&mut pic);
            pic.img.i_csp = raw::X264_CSP_I420 as i32;
            pic.img.i_plane = 3;
            Ok(Encoder {
                h,
                param,
                pic,
                pts: 0,
                width: cfg.width,
                height: cfg.height,
                fps: cfg.fps,
            })
        }
    }

    pub fn set_rate(&mut self, crf: f32, max_kbps: u32) {
        let buf = if max_kbps > 0 {
            vbv_buffer_kbit(max_kbps, self.fps)
        } else {
            0
        };
        self.set_rate_buf(crf, max_kbps, buf);
    }

    pub fn set_rate_buf(&mut self, crf: f32, max_kbps: u32, buf_kbit: u32) {
        unsafe {
            self.param.rc.f_rf_constant = crf;
            self.param.rc.i_vbv_max_bitrate = max_kbps as i32;
            self.param.rc.i_vbv_buffer_size = if max_kbps > 0 { buf_kbit.max(1) as i32 } else { 0 };
            raw::x264_encoder_reconfig(self.h, &mut self.param);
        }
    }

    pub fn encode(
        &mut self,
        p: &Planes,
        ts_ms: i64,
        force_idr: bool,
        mb_info: Option<&mut [u8]>,
    ) -> Result<Encoded, String> {
        let mb_cols = self.width.div_ceil(16) as usize;
        let mb_rows = self.height.div_ceil(16) as usize;
        if let Some(info) = mb_info.as_ref() {
            if info.len() != mb_cols * mb_rows {
                return Err(format!(
                    "mb_info has {} bytes, {}x{} macroblocks need {}",
                    info.len(),
                    mb_cols,
                    mb_rows,
                    mb_cols * mb_rows
                ));
            }
        }
        if p.y.len() < self.width as usize * self.height as usize {
            return Err("Y plane shorter than width * height".into());
        }
        let cw = self.width.div_ceil(2) as usize;
        let ch = self.height.div_ceil(2) as usize;
        if p.u.len() < cw * ch || p.v.len() < cw * ch {
            return Err("chroma plane shorter than its size".into());
        }
        unsafe {
            self.pic.img.plane[0] = p.y.as_ptr() as *mut u8;
            self.pic.img.plane[1] = p.u.as_ptr() as *mut u8;
            self.pic.img.plane[2] = p.v.as_ptr() as *mut u8;
            self.pic.img.i_stride[0] = p.stride_y as i32;
            self.pic.img.i_stride[1] = p.stride_uv as i32;
            self.pic.img.i_stride[2] = p.stride_uv as i32;
            self.pic.i_type = if force_idr {
                raw::X264_TYPE_IDR as i32
            } else {
                raw::X264_TYPE_AUTO as i32
            };
            self.pic.i_qpplus1 = 0;
            self.pic.prop.mb_info = mb_info
                .map(|m| m.as_mut_ptr())
                .unwrap_or(ptr::null_mut());
            self.pic.prop.mb_info_free = None;
            // x264 requires strictly increasing presentation timestamps.
            self.pts = ts_ms.max(self.pts + 1);
            self.pic.i_pts = self.pts;
            let mut nals: *mut raw::x264_nal_t = ptr::null_mut();
            let mut n: i32 = 0;
            let mut pic_out: raw::x264_picture_t = std::mem::zeroed();
            let size = raw::x264_encoder_encode(
                self.h,
                &mut nals,
                &mut n,
                &mut self.pic,
                &mut pic_out,
            );
            if size < 0 {
                return Err("x264_encoder_encode failed".into());
            }
            let mut bytes = Vec::new();
            if size > 0 && !nals.is_null() {
                // x264 lays its NAL payloads out contiguously.
                let first = &*nals;
                bytes.extend_from_slice(std::slice::from_raw_parts(
                    first.p_payload,
                    size as usize,
                ));
            }
            Ok(Encoded {
                bytes,
                keyframe: pic_out.b_keyframe != 0,
                qp: (pic_out.i_qpplus1 > 0).then(|| (pic_out.i_qpplus1 - 1) as f32),
                crf_avg: pic_out.prop.f_crf_avg as f32,
            })
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { raw::x264_encoder_close(self.h) }
    }
}

// x264 treats a VBV buffer under 1.1 frames as a single-frame VBV, which may lower
// rows of a frame below its starting QP; the carried configuration relies on it not
// doing that.
fn vbv_buffer_kbit(max_kbps: u32, fps: u32) -> u32 {
    (max_kbps * 5).div_ceil(4 * fps.max(1))
}
