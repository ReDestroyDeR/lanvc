//! lanvc — zero-config LAN voice chat.
//!
//! One UDP port, one room, Opus over a full mesh. Run it on every
//! machine on the LAN; instances find each other by broadcast.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod aec;
mod audio;
mod config;
mod crypt;
mod net;
mod opus;
mod server;
mod ws;
#[cfg(windows)]
mod wincon;
mod ui;

use std::collections::{HashMap, VecDeque};
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn main() {
    #[cfg(windows)]
    wincon::setup(); // first: route stdout/stderr (console or %TEMP%\lanvc.log)

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--selftest") {
        std::process::exit(selftest());
    }
    if let Err(e) = run(&args) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> anyhow::Result<()> {
    let port = arg_value(args, "--port")
        .map(|v| v.parse::<u16>())
        .transpose()
        .map_err(|_| anyhow::anyhow!("--port needs a number 0-65535"))?
        .unwrap_or(net::PORT);

    // headless rendezvous+relay server mode (--server <addr> is the
    // CLIENT setting pointing at one of these)
    if args.iter().any(|a| a == "--serve") {
        let key = arg_value(args, "--key").unwrap_or("").to_string();
        return server::run(port, key);
    }

    // headless WebSocket broadcast hub (self-hostable, for ws:// transports)
    if args.iter().any(|a| a == "--wshub") {
        return ws::run_hub(port);
    }

    let headless = args.iter().any(|a| a == "--headless");
    let no_lan = args.iter().any(|a| a == "--no-lan");
    let session = make_session();

    // persisted settings (config.toml), CLI flags override the name only
    let cfg = config::load();
    let mut name = cfg.name;
    if name.is_empty() {
        name = default_name();
    }
    if let Some(n) = arg_value(args, "--name") {
        name = n.to_string();
    }
    let host_flag = args.iter().any(|a| a == "--host");

    let ui: net::Ui = Arc::new(Mutex::new(net::UiState {
        name,
        mic_muted: false,
        spk_muted: false,
        aec_enabled: cfg.echo_cancel,
        mic_threshold: cfg.mic_threshold,
        self_speaking: false,
        volume: cfg.volume,
        server: arg_value(args, "--server").map(|s| s.to_string()).unwrap_or(cfg.server),
        room_key: arg_value(args, "--key").map(|s| s.to_string()).unwrap_or(cfg.room_key),
        hosting: cfg.hosting || host_flag,
        peers: Vec::new(),
        self_addr: String::new(),
        error: String::new(),
    }));
    let peers: net::Peers = Arc::new(Mutex::new(HashMap::new()));
    let voices: net::Voices = Arc::new(Mutex::new(HashMap::new()));
    let aec_ref: net::MicQ = Arc::new(Mutex::new(VecDeque::new()));
    let srv: net::Srv = Arc::new(Mutex::new(None));

    let sock = Arc::new(
        UdpSocket::bind(("0.0.0.0", port))
            .with_context(|| format!("cannot bind UDP port {port} (another instance? try --port 45455)"))?,
    );
    sock.set_broadcast(true)?;
    let listen_port = sock.local_addr()?.port();

    // Audio failing (no mic etc.) must not kill the app: we still join the
    // room and show the error in the status bar.
    let audio = match audio::setup(&voices, &ui, &aec_ref) {
        Ok(a) => Some(a),
        Err(e) => {
            ui.lock().unwrap().error = format!("no audio: {e:#}");
            None
        }
    };
    let (codec_rate, out_rate, frame_len, mic) = match &audio {
        Some(a) => (a.codec_rate, a.out_rate, a.frame_len, a.mic.clone()),
        None => (48_000, 48_000, 480, Arc::new(Mutex::new(VecDeque::new()))),
    };

    let enc = opus::Encoder::new(codec_rate as i32, frame_len, 32_000)
        .context("opus encoder init")?;
    let refs: net::Refs = Arc::new(Mutex::new(HashMap::new()));
    let ws_tx = ws::new_tx();
    if ws::is_ws_url(&ui.lock().unwrap().server) {
        ws::spawn_room(
            ui.lock().unwrap().server.trim().to_string(),
            session,
            ui.clone(),
            peers.clone(),
            voices.clone(),
            refs.clone(),
            out_rate,
            ws_tx.clone(),
        );
    }

    net::spawn_recv(
        sock.clone(),
        session,
        ui.clone(),
        peers.clone(),
        voices.clone(),
        refs.clone(),
        srv.clone(),
        out_rate,
    );
    net::spawn_tx(
        sock,
        session,
        ui.clone(),
        peers,
        enc,
        mic.clone(),
        aec_ref,
        refs,
        srv,
        no_lan,
        ws_tx,
        frame_len,
        codec_rate,
        out_rate,
    );

    ui.lock().unwrap().self_addr = format!("0.0.0.0:{listen_port}");

    // --tone: feed a synthetic sine into the mic queue (testing without a mic)
    if args.iter().any(|a| a == "--tone") {
        let mic = mic.clone();
        let rate = codec_rate as usize;
        std::thread::Builder::new()
            .name("fake-mic".into())
            .spawn(move || {
                let frame = rate / 50;
                let mut i = 0usize;
                loop {
                    let samples: Vec<f32> = (0..frame)
                        .map(|j| {
                            (2.0 * std::f32::consts::PI * 440.0 * (i * frame + j) as f32
                                / rate as f32)
                                .sin()
                        })
                        .collect();
                    i += 1;
                    let mut q = mic.lock().unwrap();
                    for s in samples {
                        if q.len() < 96_000 {
                            q.push_back(s);
                        }
                    }
                    drop(q);
                    std::thread::sleep(Duration::from_millis(20));
                }
            })
            .ok();
    }

    if headless {
        let voices = voices.clone();
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let s = ui.lock().unwrap();
            let vl = voices.lock().unwrap();
            let names: Vec<String> = s
                .peers
                .iter()
                .map(|p| {
                    let f = vl.get(&p.session);
                    let fifo = f.map(|f| f.samples.len()).unwrap_or(0);
                    let age = f
                        .and_then(|f| f.last_voice)
                        .map(|t| Instant::now().duration_since(t).as_secs_f32())
                        .unwrap_or(-1.0);
                    format!("{} ({}: {}) fifo={fifo} age={age:.1}", p.name, p.addr, p.speaking)
                })
                .collect();
            println!("[headless] peers: [{}]", names.join(", "));
        }
    }

    ui::run(ui).map_err(|e| anyhow::anyhow!("gui: {e}"))
}

/// Encode 1 s of a 440 Hz sine, decode it back, and sanity-check the result.
/// Runs the full vendored libvorbis path without needing any audio devices.
fn selftest() -> i32 {
    const RATE: usize = 48_000;
    const FRAME: usize = 480;
    const FREQ: f32 = 440.0;

    let mut enc = match opus::Encoder::new(RATE as i32, FRAME, 32_000) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("encoder init failed: {e:#}");
            return 1;
        }
    };
    let mut dec = match opus::Decoder::new(FRAME) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("decoder init failed: {e:#}");
            return 1;
        }
    };
    let mut pcm = Vec::new();
    let mut bytes = 0usize;
    let mut packets = 0usize;
    for i in 0..(RATE / FRAME) {
        let samples: Vec<f32> = (0..FRAME)
            .map(|j| {
                ((i * FRAME + j) as f32 * 2.0 * std::f32::consts::PI * FREQ / RATE as f32).sin()
            })
            .collect();
        let pkt = enc.encode(&samples);
        if pkt.is_empty() {
            eprintln!("encode produced no packet at frame {i}");
            return 1;
        }
        bytes += pkt.len();
        packets += 1;
        let out = dec.decode(&pkt);
        if out.is_empty() {
            eprintln!("decode failed at packet {i}");
            return 1;
        }
        pcm.extend(out);
    }

    let n = pcm.len();
    if n == 0 {
        eprintln!("decoded nothing");
        return 1;
    }
    let rms = (pcm.iter().map(|s| s * s).sum::<f32>() / n as f32).sqrt();
    let zc = pcm.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count() as f32;
    let freq = zc * RATE as f32 / n as f32;
    println!(
        "packets={packets} avg_bytes={} samples={n} rms={rms:.3} freq={freq:.1} Hz",
        bytes / packets.max(1)
    );
    let ok = rms > 0.3 && (freq - FREQ).abs() < 10.0;
    if !ok {
        println!("SELFTEST FAIL (codec stage)");
        return 1;
    }
    println!("codec: OK");

    // AEC stage: far = 300 Hz (what the speakers played), near = the far
    // signal delayed 10 ms and attenuated (the room path into the mic) plus
    // an independent 1200 Hz near voice. Expect the 300 Hz component in the
    // residual strongly suppressed after the canceller converges.
    const AR: usize = 48_000;
    const AFRAME: usize = 480;
    const AFILTER: usize = 4096;
    const DELAY: usize = 480;
    let mut a = match aec::Aec::new(AFRAME, AFILTER, AR as i32) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("aec init failed: {e:#}");
            return 1;
        }
    };
    let mut prep = match aec::Preprocess::new(AFRAME, AR as i32) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("preprocess init failed: {e:#}");
            return 1;
        }
    };
        prep.set_echo_state(&a);
    let mut far_all: Vec<f32> = Vec::with_capacity(AR);
    let mut near_all: Vec<f32> = Vec::with_capacity(AR);
    for i in 0..AR {
        let t = i as f32 / AR as f32;
        let far = 0.4 * (2.0 * std::f32::consts::PI * 300.0 * t).sin();
        let near = if i >= DELAY { 0.8 * far_all[i - DELAY] } else { 0.0 }
            + 0.25 * (2.0 * std::f32::consts::PI * 1200.0 * t).sin();
        far_all.push(far);
        near_all.push(near);
    }
    let mut res_all: Vec<f32> = Vec::with_capacity(AR);
    for chunk in (0..AR).step_by(AFRAME) {
        let mut r = a.process(&near_all[chunk..chunk + AFRAME], &far_all[chunk..chunk + AFRAME]);
        prep.run(&mut r);
        res_all.extend(r);
    }
    let win = AR - AR * 2 / 5; // measure on the last 40 % (post-convergence)
    let g_near = goertzel(&near_all[win..], 300.0, AR as f32);
    let g_res = goertzel(&res_all[win..], 300.0, AR as f32);
    let sup_db = 20.0 * (g_near / g_res.max(1e-9)).log10();
    println!("aec: far suppression {sup_db:.1} dB");
    if sup_db < 10.0 {
        println!("SELFTEST FAIL (aec stage)");
        return 1;
    }
    println!("SELFTEST OK");
    0
}

/// Single-bin DFT magnitude at `freq` (Goertzel), for the AEC selftest.
fn goertzel(sig: &[f32], freq: f32, rate: f32) -> f32 {
    let n = sig.len() as f32;
    let k = (freq / rate * n).round();
    let w = 2.0 * std::f32::consts::PI * k / n;
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &x in sig {
        let s0 = x + 2.0 * w.cos() * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2).sqrt()
}

fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

fn default_name() -> String {
    if let Ok(n) = std::env::var("COMPUTERNAME") {
        return n;
    }
    if let Ok(n) = std::fs::read_to_string("/etc/hostname") {
        let t = n.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    "unnamed".into()
}

fn make_session() -> u32 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut h);
    std::process::id().hash(&mut h);
    h.finish() as u32
}

use anyhow::Context as _;
