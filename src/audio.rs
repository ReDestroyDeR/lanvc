//! Microphone capture and playback mixing via cpal (WASAPI on Windows).

#[cfg(windows)]
use std::collections::VecDeque;
#[cfg(windows)]
use std::sync::Arc;

#[cfg(windows)]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::net::{MicQ, Ui, Voices};

pub struct Audio {
    pub codec_rate: u32,  // mic sample rate = vorbis stream rate
    pub frame_len: usize, // samples per 10 ms codec frame
    pub out_rate: u32,    // speaker sample rate (FIFOs are pushed at this rate)
    pub mic: MicQ,
    #[cfg(windows)]
    // never read: held so the streams stay alive (dropping stops audio)
    #[allow(dead_code)]
    streams: Vec<cpal::Stream>,
}

pub fn setup(voices: &Voices, ui: &Ui, aec_ref: &MicQ) -> anyhow::Result<Audio> {
    #[cfg(windows)]
    {
        setup_windows(voices, ui, aec_ref)
    }
    #[cfg(not(windows))]
    {
        let _ = (voices, ui, aec_ref);
        anyhow::bail!("audio is only compiled into the Windows build")
    }
}

#[cfg(windows)]
fn setup_windows(voices: &Voices, ui: &Ui, aec_ref: &MicQ) -> anyhow::Result<Audio> {
    use cpal::SampleFormat;
    use std::sync::Mutex as StdMutex;

    let host = cpal::default_host();
    let in_dev = host
        .default_input_device()
        .ok_or_else(|| anyhow::anyhow!("no microphone found"))?;
    let out_dev = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no speakers found"))?;
    let in_cfg = in_dev.default_input_config()?;
    let out_cfg = out_dev.default_output_config()?;

    let mic: MicQ = Arc::new(StdMutex::new(VecDeque::new()));

    let in_stream = match in_cfg.sample_format() {
        SampleFormat::F32 => build_input(&in_dev, &in_cfg, mic.clone(), |s: f32| s)?,
        SampleFormat::I16 => {
            build_input(&in_dev, &in_cfg, mic.clone(), |s: i16| s as f32 / 32768.0)?
        }
        SampleFormat::U16 => {
            build_input(&in_dev, &in_cfg, mic.clone(), |s: u16| {
                (s as f32 - 32768.0) / 32768.0
            })?
        }
        other => anyhow::bail!("unsupported mic sample format {other:?}"),
    };


    let voices = voices.clone();
    let ui = ui.clone();
    let aec_ref = aec_ref.clone();
    let out_stream = match out_cfg.sample_format() {
        SampleFormat::F32 => {
            build_output::<f32>(&out_dev, &out_cfg, voices, aec_ref, ui, |v| v)?
        }
        SampleFormat::I16 => {
            build_output::<i16>(&out_dev, &out_cfg, voices, aec_ref, ui, |v| (v * 32767.0) as i16)?
        }
        SampleFormat::U16 => {
            build_output::<u16>(&out_dev, &out_cfg, voices, aec_ref, ui, |v| {
                ((v.clamp(-1.0, 1.0) + 1.0) * 32767.5) as u16
            })?
        }
        other => anyhow::bail!("unsupported speaker sample format {other:?}"),
    };

    Ok(Audio {
        codec_rate: in_cfg.sample_rate().0,
        frame_len: in_cfg.sample_rate().0 as usize / 100, // 10 ms
        out_rate: out_cfg.sample_rate().0,
        mic,
        streams: vec![in_stream, out_stream],
    })
}

#[cfg(windows)]
fn build_input<T: cpal::SizedSample + Send + 'static>(
    dev: &cpal::Device,
    cfg: &cpal::SupportedStreamConfig,
    mic: MicQ,
    conv: fn(T) -> f32,
) -> anyhow::Result<cpal::Stream> {
    let ch = cfg.channels().max(1) as usize;
    let err = |e| eprintln!("mic stream error: {e}");
    // prefer ~10 ms device buffers for latency; fall back to device defaults
    let mut sc: cpal::StreamConfig = cfg.clone().into();
    sc.buffer_size = cpal::BufferSize::Fixed((cfg.sample_rate().0 / 100).max(1));
    let stream = match dev.build_input_stream(&sc, cb_input(mic.clone(), ch, conv), err, None) {
        Ok(s) => s,
        Err(_) => dev.build_input_stream(
            &cfg.clone().into(),
            cb_input(mic.clone(), ch, conv),
            err,
            None,
        )?,
    };
    stream.play()?;
    Ok(stream)
}

#[cfg(windows)]
fn cb_input<T: cpal::SizedSample + Send + 'static>(
    mic: MicQ,
    ch: usize,
    conv: fn(T) -> f32,
) -> impl FnMut(&[T], &cpal::InputCallbackInfo) + Send + 'static {
    move |data: &[T], _: &cpal::InputCallbackInfo| {
        let mut q = mic.lock().unwrap();
        for frame in data.chunks_exact(ch) {
            let m = frame.iter().map(|s| conv(*s)).sum::<f32>() / ch as f32;
            if q.len() >= 96_000 {
                q.pop_front();
            }
            q.push_back(m);
        }
    }
}

#[cfg(windows)]
fn build_output<T: cpal::SizedSample + Send + 'static>(
    dev: &cpal::Device,
    cfg: &cpal::SupportedStreamConfig,
    voices: Voices,
    aec_ref: MicQ,
    ui: Ui,
    conv: fn(f32) -> T,
) -> anyhow::Result<cpal::Stream> {
    let ch = cfg.channels().max(1) as usize;
    let err = |e| eprintln!("speaker stream error: {e}");
    let mut sc: cpal::StreamConfig = cfg.clone().into();
    sc.buffer_size = cpal::BufferSize::Fixed((cfg.sample_rate().0 / 100).max(1));
    let stream = match dev.build_output_stream(
        &sc,
        cb_output(voices.clone(), aec_ref.clone(), ui.clone(), ch, conv),
        err,
        None,
    ) {
        Ok(s) => s,
        Err(_) => dev.build_output_stream(
            &cfg.clone().into(),
            cb_output(voices, aec_ref, ui, ch, conv),
            err,
            None,
        )?,
    };
    stream.play()?;
    Ok(stream)
}

#[cfg(windows)]
fn cb_output<T: cpal::SizedSample + Send + 'static>(
    voices: Voices,
    aec_ref: MicQ,
    ui: Ui,
    ch: usize,
    conv: fn(f32) -> T,
) -> impl FnMut(&mut [T], &cpal::OutputCallbackInfo) + Send + 'static {
    move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
        let (vol, muted) = {
            let s = ui.lock().unwrap();
            (s.volume, s.spk_muted)
        };
        let n = data.len() / ch;
        let mut feeds = voices.lock().unwrap();
        let mut mix = vec![0.0f32; n];
        for feed in feeds.values_mut() {
            for m in mix.iter_mut() {
                if let Some(s) = feed.samples.pop_front() {
                    *m += s;
                }
            }
        }
        let g = if muted { 0.0 } else { vol };
        // what physically comes out of the speakers, as mono — this is
        // the far-end reference the echo canceller subtracts from the mic
        let mut rq = aec_ref.lock().unwrap();
        for (frame, m) in data.chunks_mut(ch).zip(mix.iter()) {
            let s = (m * g).clamp(-1.0, 1.0);
            if rq.len() < 96_000 {
                rq.push_back(s);
            }
            for slot in frame {
                *slot = conv(s);
            }
        }
    }
}
