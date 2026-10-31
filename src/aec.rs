//! Thin safe wrapper around speexdsp's MDF echo canceller.
//!
//! The canceller learns the linear path (gain + delay, up to `filter` taps)
//! from the far-end reference (what our speakers played) to the near-end
//! (mic) signal and subtracts it, leaving mostly the near-end voice.

use std::ffi::{c_int, c_void};

#[repr(C)]
pub(crate) struct SpeexEchoState {
    _opaque: [u8; 0],
}

extern "C" {
    fn speex_echo_state_init(frame_size: c_int, filter_length: c_int) -> *mut SpeexEchoState;
    fn speex_echo_state_destroy(st: *mut SpeexEchoState);
    fn speex_echo_cancellation(
        st: *mut SpeexEchoState,
        rec: *const i16,
        play: *const i16,
        out: *mut i16,
    );
    fn speex_echo_ctl(st: *mut SpeexEchoState, request: c_int, ptr: *mut c_void) -> c_int;
}

// from include/speex/speex_echo.h
const SPEEX_ECHO_SET_SAMPLING_RATE: c_int = 24;

pub struct Aec {
    st: *mut SpeexEchoState,
    frame: usize,
}

// Owned exclusively by the tx thread; the C state has no globals.
unsafe impl Send for Aec {}

impl Aec {
    pub fn new(frame_size: usize, filter_length: usize, rate: i32) -> anyhow::Result<Self> {
        unsafe {
            let st = speex_echo_state_init(frame_size as c_int, filter_length as c_int);
            anyhow::ensure!(!st.is_null(), "speex_echo_state_init failed");
            speex_echo_ctl(st, SPEEX_ECHO_SET_SAMPLING_RATE, &rate as *const i32 as *mut c_void);
            Ok(Self { st, frame: frame_size })
        }
    }

    /// `near` = mic samples, `far` = playback reference (what the speakers
    /// played, at the same sample rate). Returns near with the far component
    /// cancelled. Both slices are truncated to the configured frame size.
    pub fn process(&mut self, near: &[f32], far: &[f32]) -> Vec<f32> {
        let n = self.frame.min(near.len()).min(far.len());
        let to_i16 = |s: f32| (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        let rec: Vec<i16> = near[..n].iter().map(|s| to_i16(*s)).collect();
        let play: Vec<i16> = far[..n].iter().map(|s| to_i16(*s)).collect();
        let mut out = vec![0i16; n];
        unsafe {
            speex_echo_cancellation(self.st, rec.as_ptr(), play.as_ptr(), out.as_mut_ptr());
        }
        out.iter().map(|s| *s as f32 / 32768.0).collect()
    }
}

impl Drop for Aec {
    fn drop(&mut self) {
        if !self.st.is_null() {
            unsafe { speex_echo_state_destroy(self.st) };
        }
    }
}

impl Aec {
    pub(crate) fn raw(&self) -> *mut SpeexEchoState {
        self.st
    }
}

// ---- Preprocessor: spectral noise suppression + speech probability ------

#[repr(C)]
struct SpeexPreprocessState {
    _opaque: [u8; 0],
}

extern "C" {
    fn speex_preprocess_state_init(frame_size: c_int, sampling_rate: c_int) -> *mut SpeexPreprocessState;
    fn speex_preprocess_state_destroy(st: *mut SpeexPreprocessState);
    fn speex_preprocess_run(st: *mut SpeexPreprocessState, x: *mut i16);
    fn speex_preprocess_ctl(st: *mut SpeexPreprocessState, request: c_int, ptr: *mut c_void) -> c_int;
}

// from include/speex/speex_preprocess.h
const SPEEX_PREPROCESS_SET_DENOISE: c_int = 0;
const SPEEX_PREPROCESS_SET_NOISE_SUPPRESS: c_int = 18;
const SPEEX_PREPROCESS_SET_ECHO_STATE: c_int = 24;
const SPEEX_PREPROCESS_GET_PROB: c_int = 45;

pub struct Preprocess {
    st: *mut SpeexPreprocessState,
}

// Same ownership story as Aec.
unsafe impl Send for Preprocess {}

impl Preprocess {
    pub fn new(frame_size: usize, rate: i32) -> anyhow::Result<Self> {
        unsafe {
            let st = speex_preprocess_state_init(frame_size as c_int, rate);
            anyhow::ensure!(!st.is_null(), "speex_preprocess_state_init failed");
            let on: i32 = 1;
            speex_preprocess_ctl(st, SPEEX_PREPROCESS_SET_DENOISE, &on as *const i32 as *mut c_void);
            // -25 dB of steady-state noise suppression
            let sup: i32 = -25;
            speex_preprocess_ctl(
                st,
                SPEEX_PREPROCESS_SET_NOISE_SUPPRESS,
                &sup as *const i32 as *mut c_void,
            );
            Ok(Self { st })
        }
    }

    /// Feed one frame of f32 samples (converted to s16 internally, as the
    /// speexdsp public API is); returns the speech probability 0..1 for it.
    pub fn run(&mut self, frame: &mut [f32]) -> f32 {
        let mut x: Vec<i16> = frame
            .iter()
            .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect();
        let prob = unsafe {
            speex_preprocess_run(self.st, x.as_mut_ptr());
            let mut p: i32 = 0;
            speex_preprocess_ctl(self.st, SPEEX_PREPROCESS_GET_PROB, &mut p as *mut i32 as *mut c_void);
            p.clamp(0, 100) as f32 / 100.0
        };
        for (d, s) in frame.iter_mut().zip(x.iter()) {
            *d = *s as f32 / 32768.0;
        }
        prob
    }

    /// Link an echo canceller so residual echo is suppressed too.
    /// NB: unlike the other ctl requests, SET_ECHO_STATE takes the
    /// SpeexEchoState pointer *itself*, not a pointer to it.
    pub fn set_echo_state(&mut self, aec: &Aec) {
        unsafe {
            speex_preprocess_ctl(self.st, SPEEX_PREPROCESS_SET_ECHO_STATE, aec.raw() as *mut c_void);
        }
    }
}

impl Drop for Preprocess {
    fn drop(&mut self) {
        if !self.st.is_null() {
            unsafe { speex_preprocess_state_destroy(self.st) };
        }
    }
}
