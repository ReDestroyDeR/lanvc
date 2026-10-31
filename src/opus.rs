//! Minimal Opus streaming wrapper: one packet per 10 ms frame, no container,
//! no headers (raw Opus packets are self-describing for our purposes).
//!
//! The libopus state is fully opaque, so there are no C structs to mirror.

use std::ffi::{c_int, c_void};

extern "C" {
    fn opus_encoder_create(
        fs: c_int,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut c_void;
    fn opus_encode(
        st: *mut c_void,
        pcm: *const i16,
        frame_size: c_int,
        data: *mut u8,
        max_data_bytes: c_int,
    ) -> c_int;
    fn opus_encoder_ctl(st: *mut c_void, request: c_int, ...) -> c_int;
    fn opus_encoder_destroy(st: *mut c_void);

    fn opus_decoder_create(fs: c_int, channels: c_int, error: *mut c_int) -> *mut c_void;
    fn opus_decode(
        st: *mut c_void,
        data: *const u8,
        len: c_int,
        pcm: *mut i16,
        frame_capacity: c_int,
        decode_fec: c_int,
    ) -> c_int;
    fn opus_decoder_destroy(st: *mut c_void);
}

// from opus_defines.h
const OPUS_APPLICATION_VOIP: c_int = 2048;
const OPUS_SET_BITRATE: c_int = 4002;

const MAX_PACKET: usize = 512;

pub struct Encoder {
    st: *mut c_void,
    frame: usize,
}

// Single-owner handle; libopus keeps no globals.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(rate: i32, frame_size: usize, bitrate: i32) -> anyhow::Result<Self> {
        unsafe {
            let mut err: c_int = 0;
            let st = opus_encoder_create(rate, 1, OPUS_APPLICATION_VOIP, &mut err);
            anyhow::ensure!(!st.is_null() && err == 0, "opus_encoder_create failed ({err})");
            opus_encoder_ctl(st, OPUS_SET_BITRATE, bitrate);
            Ok(Self { st, frame: frame_size })
        }
    }

    /// Encode one frame of mono samples into one Opus packet
    /// (empty on codec error, which should never happen on sane input).
    pub fn encode(&mut self, mono: &[f32]) -> Vec<u8> {
        let n = self.frame.min(mono.len());
        let pcm: Vec<i16> = mono[..n]
            .iter()
            .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect();
        let mut out = vec![0u8; MAX_PACKET];
        let len = unsafe {
            opus_encode(
                self.st,
                pcm.as_ptr(),
                n as c_int,
                out.as_mut_ptr(),
                out.len() as c_int,
            )
        };
        if len <= 0 {
            Vec::new()
        } else {
            out.truncate(len as usize);
            out
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if !self.st.is_null() {
            unsafe { opus_encoder_destroy(self.st) };
        }
    }
}

pub struct Decoder {
    st: *mut c_void,
    frame: usize,
}

unsafe impl Send for Decoder {}

impl Decoder {
    /// Decodes at 48 kHz (all Opus packets decode there) in mono; callers
    /// resample to the playback rate if it differs.
    pub fn new(frame_size: usize) -> anyhow::Result<Self> {
        unsafe {
            let mut err: c_int = 0;
            let st = opus_decoder_create(48_000, 1, &mut err);
            anyhow::ensure!(!st.is_null() && err == 0, "opus_decoder_create failed ({err})");
            Ok(Self { st, frame: frame_size })
        }
    }

    /// Decode one packet into mono samples (empty on error).
    pub fn decode(&mut self, packet: &[u8]) -> Vec<f32> {
        if packet.is_empty() {
            return Vec::new();
        }
        let mut pcm = vec![0i16; self.frame];
        let n = unsafe {
            opus_decode(
                self.st,
                packet.as_ptr(),
                packet.len() as c_int,
                pcm.as_mut_ptr(),
                self.frame as c_int,
                0,
            )
        };
        if n <= 0 {
            Vec::new()
        } else {
            pcm[..n as usize].iter().map(|s| *s as f32 / 32768.0).collect()
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if !self.st.is_null() {
            unsafe { opus_decoder_destroy(self.st) };
        }
    }
}
