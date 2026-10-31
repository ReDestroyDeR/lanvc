//! Voice transport: a full UDP mesh. Peers find each other three ways:
//! LAN broadcast (default), and — when a rendezvous server is configured —
//! its roster (public ip:port per peer, used for UDP hole punching) with a
//! server relay as the fallback when punching fails.
//!
//! Datagrams (little endian):
//!   "LVC2" | u8 type | u32 session | body
//!   type 0 ANNOUNCE: u16 port | u8 name_len | name
//!   type 1 AUDIO:    u32 seq  | raw Opus packet (10 ms, 48 kHz, mono)
//!   type 3 S_HELLO (to server):  u8 name_len | name
//!   type 4 S_ROOM (from server): u16 count | { u32 session, u32 ipv4,
//!                                  u16 port, u8 name_len, name }*
//!   type 5 S_RELAY (to server):  u32 target | u32 seq | opus packet
//!   type 6 S_RELAYED (from server): u32 from | u32 seq | opus packet
//!
//! With a room key set, every datagram travels inside an LVE1 envelope
//! (XChaCha20-Poly1305) — see crypt.rs.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::aec;
use crate::crypt::KeyCache;
use crate::opus;

pub const PORT: u16 = 45454;

pub const MAGIC: &[u8; 4] = b"LVC2";
pub const T_ANNOUNCE: u8 = 0;
pub const T_AUDIO: u8 = 1;
pub const T_S_HELLO: u8 = 3;
pub const T_S_ROOM: u8 = 4;
pub const T_S_RELAY: u8 = 5;
pub const T_S_RELAYED: u8 = 6;

const SERVER_SESSION: u32 = 0xFFFF_FFFF; // nonce namespace for server-side seals
pub(crate) const PATH_DIRECT: u8 = 0;
pub(crate) const PATH_WS: u8 = 2;
const PATH_RELAY: u8 = 1;

const ANNOUNCE_EVERY: Duration = Duration::from_secs(1);
const HELLO_EVERY: Duration = Duration::from_secs(5);
const PEER_TIMEOUT: Duration = Duration::from_secs(8);
const ADDR_ADOPT_STALE: Duration = Duration::from_secs(3);
const SPEAKING_FOR: Duration = Duration::from_millis(300);
const VAD_HANGOVER: Duration = Duration::from_millis(400); // keep tx open after speech
const VOICE_FIFO_MAX: usize = 1 << 15; // ~0.7 s of samples: hard latency cap
const REF_RING_MAX: usize = 1 << 13; // per-peer AEC reference ring
const PEER_DECODE_RATE: u32 = 48_000; // Opus decodes natively at 48 kHz
const PEER_DECODE_FRAME: usize = 480; // our packets are 10 ms

pub type Ui = Arc<Mutex<UiState>>;
pub type Peers = Arc<Mutex<HashMap<u32, PeerEntry>>>;
pub type Voices = Arc<Mutex<HashMap<u32, VoiceFeed>>>;
/// Decoded peer audio at PEER_DECODE_RATE: the cancellation references.
pub type Refs = Arc<Mutex<HashMap<u32, VecDeque<f32>>>>;
/// Resolved rendezvous server address (None until a server is configured
/// and its hostname resolves).
pub type Srv = Arc<Mutex<Option<SocketAddr>>>;
pub type MicQ = Arc<Mutex<VecDeque<f32>>>;

#[derive(Default)]
pub struct UiState {
    pub name: String,
    pub mic_muted: bool,
    pub spk_muted: bool,
    pub aec_enabled: bool,
    pub mic_threshold: u8, // 0..100 % speech probability; 0 = always transmit
    pub self_speaking: bool,
    pub volume: f32,
    pub server: String,  // "host" or "host:port" of the rendezvous server
    pub room_key: String, // empty = plaintext LAN mode
    pub hosting: bool,   // this client answers HELLOs and relays for friends
    pub peers: Vec<PeerInfo>,
    pub self_addr: String,
    pub error: String,
}

pub struct PeerInfo {
    pub session: u32,
    pub name: String,
    pub addr: SocketAddr,
    pub path: u8, // PATH_DIRECT / PATH_RELAY
    pub speaking: bool,
}

pub struct PeerEntry {
    pub name: String,
    pub addr: SocketAddr,
    pub path: u8,
    pub last_seen: Instant,
}

pub struct VoiceFeed {
    pub samples: VecDeque<f32>,
    pub last_voice: Option<Instant>,
    // streaming resampler (only active if rates differ)
    rs: Resampler,
}

/// Streaming linear resampler, continuous across chunks.
#[derive(Default)]
pub struct Resampler {
    last: f32,
    t: f64,
}

impl Resampler {
    pub fn process(&mut self, pcm: &[f32], from: u32, to: u32) -> Vec<f32> {
        let mut out = Vec::with_capacity(pcm.len());
        if from == to || from == 0 || to == 0 {
            out.extend_from_slice(pcm);
            return out;
        }
        let ratio = from as f64 / to as f64;
        let mut t = self.t;
        let mut last = self.last;
        for &s in pcm {
            while t < 1.0 {
                out.push(last + (s - last) * (t as f32));
                t += ratio;
            }
            t -= 1.0;
            last = s;
        }
        self.t = t;
        self.last = last;
        out
    }
}

fn broadcast_addr() -> SocketAddr {
    SocketAddr::from(([255, 255, 255, 255], PORT))
}

pub fn msg(ty: u8, session: u32, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(9 + body.len());
    v.extend_from_slice(MAGIC);
    v.push(ty);
    v.extend_from_slice(&session.to_le_bytes());
    v.extend_from_slice(body);
    v
}

pub struct Rx<'a> {
    pub ty: u8,
    pub session: u32,
    pub body: &'a [u8],
}

pub fn parse(buf: &[u8]) -> Option<Rx<'_>> {
    if buf.len() < 9 || &buf[..4] != MAGIC {
        return None;
    }
    Some(Rx {
        ty: buf[4],
        session: u32::from_le_bytes(buf[5..9].try_into().ok()?),
        body: &buf[9..],
    })
}

/// Parse an ANNOUNCE body into (udp port, display name).
pub(crate) fn parse_announce(body: &[u8]) -> Option<(u16, String)> {
    if body.len() < 3 {
        return None;
    }
    let port = u16::from_le_bytes([body[0], body[1]]);
    let len = (body[2] as usize).min(body.len() - 3);
    Some((port, String::from_utf8_lossy(&body[3..3 + len]).into_owned()))
}

fn announce_body(port: u16, name: &str) -> Vec<u8> {
    let nb = name.as_bytes();
    let mut b = Vec::with_capacity(3 + nb.len().min(64));
    b.extend_from_slice(&port.to_le_bytes());
    b.push(nb.len().min(64) as u8);
    b.extend_from_slice(&nb[..nb.len().min(64)]);
    b
}

fn hello_body(name: &str) -> Vec<u8> {
    let nb = name.as_bytes();
    let mut b = Vec::with_capacity(1 + nb.len().min(64));
    b.push(nb.len().min(64) as u8);
    b.extend_from_slice(&nb[..nb.len().min(64)]);
    b
}

/// Room roster for hosted rooms: friends register via S_HELLO and get
/// introduced to each other; S_RELAY datagrams are forwarded between them.
pub struct Rendezvous {
    regs: HashMap<u32, Reg>,
}

struct Reg {
    addr: SocketAddr,
    name: String,
    last_seen: Instant,
}

impl Default for Rendezvous {
    fn default() -> Self {
        Self { regs: HashMap::new() }
    }
}

impl Rendezvous {
    pub fn retain(&mut self) {
        let cutoff = Instant::now() - Duration::from_secs(30);
        self.regs.retain(|_, r| r.last_seen > cutoff);
    }

    pub fn peer_count(&self) -> usize {
        self.regs.len()
    }

    /// Register a friend and answer with the roster. `host` = Some((session,
    /// listen_port, name)) when the local client is hosting: its own entry
    /// is appended as ip 0.0.0.0 (friends substitute the address the roster
    /// arrived from). Returns the friend's name.
    #[allow(clippy::too_many_arguments)]
    pub fn hello(
        &mut self,
        sock: &UdpSocket,
        kc: &mut KeyCache,
        cipher: Option<&chacha20poly1305::XChaCha20Poly1305>,
        session: u32,
        body: &[u8],
        src: SocketAddr,
        host: Option<(u32, u16, String)>,
    ) -> String {
        let len = body.first().copied().unwrap_or(0) as usize;
        let name =
            String::from_utf8_lossy(&body[1..1 + len.min(body.len().saturating_sub(1))]).into_owned();
        self.regs.insert(session, Reg { addr: src, name: name.clone(), last_seen: Instant::now() });

        let mut entries: Vec<(u32, SocketAddr, String)> = self
            .regs
            .iter()
            .filter(|(s, _)| **s != session)
            .map(|(s, r)| (*s, r.addr, r.name.clone()))
            .collect();
        if let Some((hs, port, hname)) = &host {
            // 0.0.0.0 = "the room host": friends use the roster's source addr
            entries.insert(0, (*hs, SocketAddr::from(([0, 0, 0, 0], *port)), hname.clone()));
        }

        let mut b = Vec::new();
        b.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (s, addr, rname) in entries {
            b.extend_from_slice(&s.to_le_bytes());
            if let SocketAddr::V4(v4) = addr {
                b.extend_from_slice(&v4.ip().octets());
            } else {
                b.extend_from_slice(&[0; 4]);
            }
            b.extend_from_slice(&addr.port().to_le_bytes());
            let nb = rname.as_bytes();
            b.push(nb.len().min(64) as u8);
            b.extend_from_slice(&nb[..nb.len().min(64)]);
        }
        let m = kc.seal(cipher, &msg(T_S_ROOM, session, &b), SERVER_SESSION);
        let _ = sock.send_to(&m, src);
        name
    }

    /// Forward a friend's relayed audio to its target. Returns false when
    /// the target is unknown (caller may treat it as audio for itself).
    pub fn relay(
        &mut self,
        sock: &UdpSocket,
        kc: &mut KeyCache,
        cipher: Option<&chacha20poly1305::XChaCha20Poly1305>,
        from: u32,
        body: &[u8],
        fallback: Option<SocketAddr>,
    ) -> bool {
        if body.len() < 5 {
            return true;
        }
        let target = u32::from_le_bytes(body[..4].try_into().unwrap());
        let addr = self
            .regs
            .get(&target)
            .map(|r| r.addr)
            .or(fallback);
        match addr {
            Some(addr) => {
                let m = kc.seal(cipher, &msg(T_S_RELAYED, from, &body[4..]), SERVER_SESSION);
                let _ = sock.send_to(&m, addr);
                true
            }
            None => false,
        }
    }
}

// ---- receive thread ------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn spawn_recv(
    sock: Arc<UdpSocket>,
    session: u32,
    ui: Ui,
    peers: Peers,
    voices: Voices,
    refs: Refs,
    srv: Srv,
    out_rate: u32,
) {
    std::thread::Builder::new()
        .name("net-recv".into())
        .spawn(move || recv_loop(sock, session, ui, peers, voices, refs, srv, out_rate))
        .expect("spawn recv thread");
}

#[allow(clippy::too_many_arguments)]
fn recv_loop(
    sock: Arc<UdpSocket>,
    session: u32,
    ui: Ui,
    peers: Peers,
    voices: Voices,
    refs: Refs,
    srv: Srv,
    out_rate: u32,
) {
    let mut decoders: HashMap<u32, opus::Decoder> = HashMap::new();
    let mut kc = KeyCache::default();
    let mut rz = Rendezvous::default();
    let mut buf = vec![0u8; 65536];
    sock.set_read_timeout(Some(Duration::from_millis(300))).ok();

    loop {
        if let Ok((n, src)) = sock.recv_from(&mut buf) {
            let cipher = kc.refresh(&ui.lock().unwrap().room_key).cloned();
            let plain = match &cipher {
                Some(c) => KeyCache::open(c, &buf[..n]),
                None => Some(buf[..n].to_vec()),
            };
            if let Some(plain) = plain {
                if let Some(rx) = parse(&plain) {
                    // NB: S_ROOM echoes our own session by design
                    let hosting = ui.lock().unwrap().hosting;
                    if rx.session != session || rx.ty == T_S_ROOM {
                        match rx.ty {
                            T_ANNOUNCE => handle_announce(
                                &sock, &ui, &peers, session, rx.session, rx.body, src,
                            ),
                            T_AUDIO => handle_audio(
                                rx.session, rx.body, src, false, &srv, &peers, &voices, &refs,
                                &mut decoders, out_rate,
                            ),
                            T_S_HELLO if hosting => {
                                // this client is the room host: register the
                                // friend, answer with the roster, add as peer
                                let listen_port =
                                    sock.local_addr().map(|a| a.port()).unwrap_or(PORT);
                                let host_name = ui.lock().unwrap().name.clone();
                                let fname = rz.hello(
                                    &sock, &mut kc, cipher.as_ref(), rx.session, rx.body, src,
                                    Some((session, listen_port, host_name)),
                                );
                                upsert_peer(&peers, rx.session, src, fname, PATH_DIRECT);
                            }
                            T_S_RELAY if hosting => {
                                let target = if rx.body.len() >= 4 {
                                    u32::from_le_bytes(rx.body[..4].try_into().unwrap())
                                } else {
                                    0
                                };
                                let fallback = if target == session {
                                    None
                                } else {
                                    peers
                                        .lock()
                                        .unwrap()
                                        .get(&target)
                                        .filter(|p| p.addr != src)
                                        .map(|p| p.addr)
                                };
                                if !rz.relay(
                                    &sock, &mut kc, cipher.as_ref(), rx.session, rx.body, fallback,
                                ) && target == session
                                {
                                    // a friend relayed audio to *us*
                                    handle_audio(
                                        rx.session, &rx.body[4..], src, false, &srv, &peers,
                                        &voices, &refs, &mut decoders, out_rate,
                                    );
                                }
                            }
                            T_S_ROOM => handle_roster(rx.body, &peers, session, src),
                            T_S_RELAYED => handle_audio(
                                rx.session, rx.body, src, true, &srv, &peers, &voices, &refs,
                                &mut decoders, out_rate,
                            ),
                            _ => {}
                        }
                    }
                }
            }
        }
        rz.retain();
        sweep(&ui, &peers, &voices, &refs, &mut decoders);
    }
}

/// Insert-or-refresh a peer (announce / HELLO arrivals). A working address
/// is never clobbered by an arrival from another path until it goes stale.
pub(crate) fn upsert_peer(peers: &Peers, them: u32, addr: SocketAddr, name: String, path: u8) -> bool {
    let now = Instant::now();
    let mut p = peers.lock().unwrap();
    match p.get_mut(&them) {
        Some(e) => {
            let stale = now.duration_since(e.last_seen) > ADDR_ADOPT_STALE;
            e.name = name;
            e.last_seen = now;
            if stale {
                e.addr = addr;
                e.path = path;
            }
            false
        }
        None => {
            p.insert(them, PeerEntry { name, addr, path, last_seen: now });
            true
        }
    }
}

pub(crate) fn handle_announce(
    sock: &UdpSocket,
    ui: &Ui,
    peers: &Peers,
    ours: u32,
    them: u32,
    body: &[u8],
    src: SocketAddr,
) {
    if body.len() < 3 {
        return;
    }
    let port = u16::from_le_bytes([body[0], body[1]]);
    let len = (body[2] as usize).min(body.len() - 3);
    let name = String::from_utf8_lossy(&body[3..3 + len]).into_owned();
    let addr = SocketAddr::new(src.ip(), port);

    let is_new = upsert_peer(peers, them, addr, name, PATH_DIRECT);
    if is_new {
        // one-shot direct reply so the other side learns about us even if it
        // never saw one of our broadcasts
        let lport = sock.local_addr().map(|a| a.port()).unwrap_or(PORT);
        let m = {
            let name = ui.lock().unwrap().name.clone();
            msg(T_ANNOUNCE, ours, &announce_body(lport, &name))
        };
        let _ = sock.send_to(&m, addr);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_audio(
    them: u32,
    body: &[u8],
    src: SocketAddr,
    is_relayed: bool,
    srv: &Srv,
    peers: &Peers,
    voices: &Voices,
    refs: &Refs,
    decoders: &mut HashMap<u32, opus::Decoder>,
    out_rate: u32,
) {
    if body.len() < 5 {
        return;
    }
    let payload = &body[4..]; // skip seq
    let now = Instant::now();

    {
        let srv_addr = *srv.lock().unwrap();
        let mut p = peers.lock().unwrap();
        let e = p.entry(them).or_insert_with(|| PeerEntry {
            name: "?".into(),
            addr: if is_relayed { srv_addr.unwrap_or(src) } else { src },
            path: if is_relayed { PATH_RELAY } else { PATH_DIRECT },
            last_seen: now,
        });
        e.last_seen = now;
        // a direct datagram from a known session wins over the relay path
        if !is_relayed && (e.path == PATH_RELAY || e.addr != src) {
            e.addr = src;
            e.path = PATH_DIRECT;
        }
    }
    let pcm = {
        let d = decoders
            .entry(them)
            .or_insert_with(|| opus::Decoder::new(PEER_DECODE_FRAME).expect("opus decoder"));
        d.decode(payload)
    };
    if pcm.is_empty() {
        return;
    }

    // cancellation reference for everyone else's transmit path
    {
        let mut r = refs.lock().unwrap();
        let ring = r.entry(them).or_default();
        ring.extend(pcm.iter().copied());
        while ring.len() > REF_RING_MAX {
            ring.drain(..ring.len() - REF_RING_MAX);
        }
    }

    // playback
    let mut v = voices.lock().unwrap();
    let feed = v.entry(them).or_insert_with(|| VoiceFeed {
        samples: VecDeque::new(),
        last_voice: None,
        rs: Resampler::default(),
    });
    push_voice(feed, &pcm, PEER_DECODE_RATE, out_rate);
    feed.last_voice = Some(Instant::now());
}

/// Roster from the rendezvous server: every entry is a remote candidate —
/// registered as a relay-path peer; the tx thread punches toward it and the
/// path switches to direct as soon as direct audio flows.
fn handle_roster(body: &[u8], peers: &Peers, ours: u32, roster_src: SocketAddr) {
    if body.len() < 2 {
        return;
    }
    let count = u16::from_le_bytes([body[0], body[1]]) as usize;
    let mut off = 2;
    let now = Instant::now();
    let mut p = peers.lock().unwrap();
    for _ in 0..count {
        if off + 11 > body.len() {
            break;
        }
        let s = u32::from_le_bytes(body[off..off + 4].try_into().unwrap());
        let octets: [u8; 4] = body[off + 4..off + 8].try_into().unwrap();
        let ip = IpAddr::V4(octets.into());
        let port = u16::from_le_bytes(body[off + 8..off + 10].try_into().unwrap());
        // ip 0.0.0.0 = "the room host": use the address the roster came from
        let peer_addr = if ip.is_unspecified() { roster_src } else { SocketAddr::new(ip, port) };
        let nl = body[off + 10] as usize;
        off += 11;
        if off + nl > body.len() {
            break;
        }
        let name = String::from_utf8_lossy(&body[off..off + nl]).into_owned();
        off += nl;
        if s == ours {
            continue;
        }
        match p.get_mut(&s) {
            Some(e) => {
                e.name = name;
                e.last_seen = now;
                if e.path == PATH_RELAY {
                    e.addr = peer_addr;
                }
            }
            None => {
                p.insert(s, PeerEntry {
                    name,
                    addr: peer_addr,
                    path: PATH_RELAY,
                    last_seen: now,
                });
            }
        }
    }
}

fn push_voice(feed: &mut VoiceFeed, pcm: &[f32], in_rate: u32, out_rate: u32) {
    let out = feed.rs.process(pcm, in_rate, out_rate);
    feed.samples.extend(out);
    while feed.samples.len() > VOICE_FIFO_MAX {
        feed.samples.pop_front();
    }
}

fn sweep(
    ui: &Ui,
    peers: &Peers,
    voices: &Voices,
    refs: &Refs,
    decoders: &mut HashMap<u32, opus::Decoder>,
) {
    let now = Instant::now();
    peers
        .lock()
        .unwrap()
        .retain(|_, p| now.duration_since(p.last_seen) < PEER_TIMEOUT);
    {
        let pl = peers.lock().unwrap();
        voices.lock().unwrap().retain(|k, _| pl.contains_key(k));
        refs.lock().unwrap().retain(|k, _| pl.contains_key(k));
        decoders.retain(|k, _| pl.contains_key(k));
    }

    let pl = peers.lock().unwrap();
    let vl = voices.lock().unwrap();
    let mut list: Vec<PeerInfo> = pl
        .iter()
        .map(|(s, p)| PeerInfo {
            session: *s,
            name: p.name.clone(),
            addr: p.addr,
            path: p.path,
            speaking: vl
                .get(s)
                .and_then(|f| f.last_voice)
                .is_some_and(|t| now.duration_since(t) < SPEAKING_FOR),
        })
        .collect();
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    ui.lock().unwrap().peers = list;
}

// ---- transmit thread -----------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn spawn_tx(
    sock: Arc<UdpSocket>,
    session: u32,
    ui: Ui,
    peers: Peers,
    enc: opus::Encoder,
    mic: MicQ,
    aec_ref: MicQ,
    refs: Refs,
    srv: Srv,
    no_lan: bool,
    ws_tx: crate::ws::WsTx,
    frame_len: usize,
    rate: u32,
    out_rate: u32,
) {
    std::thread::Builder::new()
        .name("net-tx".into())
        .spawn(move || {
            tx_loop(
                sock, session, ui, peers, enc, mic, aec_ref, refs, srv, no_lan, ws_tx, frame_len,
                rate, out_rate,
            )
        })
        .expect("spawn tx thread");
}

#[allow(clippy::too_many_arguments)]
fn tx_loop(
    sock: Arc<UdpSocket>,
    session: u32,
    ui: Ui,
    peers: Peers,
    mut enc: opus::Encoder,
    mic: MicQ,
    aec_ref: MicQ,
    refs: Refs,
    srv: Srv,
    no_lan: bool,
    ws_tx: crate::ws::WsTx,
    frame_len: usize,
    rate: u32,
    out_rate: u32,
) {
    let frame_dur = Duration::from_nanos((1e9 * frame_len as f64 / rate.max(1) as f64) as u64);
    let mut next = Instant::now();
    let mut last_announce = Instant::now() - ANNOUNCE_EVERY;
    let mut last_hello = Instant::now() - HELLO_EVERY;
    let mut last_server_field = String::new();
    let mut seq: u32 = 0;

    // echo cancellation + preprocessor state
    let mut aec: Option<aec::Aec> = None;
    let mut prep = aec::Preprocess::new(frame_len, rate as i32).ok();
    let mut ref_rs = Resampler::default();
    let mut peer_rs: HashMap<u32, Resampler> = HashMap::new();
    let mut peer_mdf: HashMap<u32, aec::Aec> = HashMap::new();
    let mut hold_until = Instant::now();
    let mut kc = KeyCache::default();

    loop {
        next += frame_dur;
        let now = Instant::now();

        let (muted, aec_on, room_key, server_field) = {
            let s = ui.lock().unwrap();
            (s.mic_muted, s.aec_enabled, s.room_key.clone(), s.server.clone())
        };
        let ws_mode = crate::ws::is_ws_url(&server_field);
        let cipher = kc.refresh(&room_key).cloned();
        let mut seal = |m: Vec<u8>| kc.seal(cipher.as_ref(), &m, session);

        if now.duration_since(last_announce) >= ANNOUNCE_EVERY {
            let m = {
                let name = ui.lock().unwrap().name.clone();
                msg(T_ANNOUNCE, session, &announce_body(
                    sock.local_addr().map(|a| a.port()).unwrap_or(PORT),
                    &name,
                ))
            };
            let m = seal(m);
            if !no_lan {
                let _ = sock.send_to(&m, broadcast_addr());
            }
            send_to_peers(&sock, &peers, &m); // punch + covers one-way broadcast
            if ws_mode {
                crate::ws::push(&ws_tx, m); // hub broadcasts it to the room
            }
            last_announce = now;
        }
        if now.duration_since(last_hello) >= HELLO_EVERY {
            let want = ui.lock().unwrap().server.trim().to_string();
            if want != last_server_field {
                last_server_field = want.clone();
                let resolved = if let Ok(sa) = want.parse::<SocketAddr>() {
                    Some(sa)
                } else {
                    use std::net::ToSocketAddrs;
                    let host_port =
                        if want.contains(':') { want.clone() } else { format!("{want}:{PORT}") };
                    host_port
                        .to_socket_addrs()
                        .ok()
                        .and_then(|mut it| it.find(|a| a.is_ipv4()))
                };
                *srv.lock().unwrap() = resolved;
            }
            if let Some(sa) = *srv.lock().unwrap() {
                let m = {
                    let name = ui.lock().unwrap().name.clone();
                    msg(T_S_HELLO, session, &hello_body(&name))
                };
                let _ = sock.send_to(&seal(m), sa);
            }
            last_hello = now;
        }

        if muted {
            mic.lock().unwrap().clear();
            aec_ref.lock().unwrap().clear();
            refs.lock().unwrap().clear();
            if ui.lock().unwrap().self_speaking {
                ui.lock().unwrap().self_speaking = false;
            }
        } else {
            let mut frame = pop_frame(&mic, frame_len);

            // 1) own-speaker cancellation: subtract what our speakers played
            //    from what our mic hears (this removes every peer's voice
            //    arriving via our speakers, in one filter).
            if aec_on {
                if aec.is_none() {
                    aec = aec::Aec::new(frame_len, 4096, rate as i32).ok();
                    if let (Some(a), Some(p)) = (aec.as_ref(), prep.as_mut()) {
                        p.set_echo_state(a);
                    }
                }
                if let Some(a) = aec.as_mut() {
                    let rframe = pop_ref_frame(&aec_ref, frame_len);
                    let mut rrs = ref_rs.process(&rframe, out_rate, rate);
                    rrs.resize(frame_len, 0.0);
                    frame = a.process(&frame, &rrs);
                }
            } else if aec.take().is_some() {
                // dropped the canceller: re-link the preprocessor without it
                prep = aec::Preprocess::new(frame_len, rate as i32).ok();
            }

            // 2) per-peer cancellation: subtract each peer's decoded voice
            //    arriving through the air (mouth -> our mic), so only our
            //    near-end voice survives in the uplink.
            {
                let mut rfs = refs.lock().unwrap();
                peer_mdf.retain(|k, _| rfs.contains_key(k));
                for (s, ring) in rfs.iter_mut() {
                    let mdf = match peer_mdf.entry(*s) {
                        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                        std::collections::hash_map::Entry::Vacant(e) => {
                            match aec::Aec::new(frame_len, 4096, rate as i32) {
                                Ok(a) => e.insert(a),
                                Err(_) => continue,
                            }
                        }
                    };
                    let take = ring.len().min(frame_len);
                    if take < frame_len {
                        continue; // reference still filling up
                    }
                    let raw: Vec<f32> = ring.drain(..take).collect();
                    let rs = peer_rs.entry(*s).or_default();
                    let mut rframe = rs.process(&raw, PEER_DECODE_RATE, rate);
                    rframe.truncate(frame_len);
                    rframe.resize(frame_len, 0.0);
                    frame = mdf.process(&frame, &rframe);
                }
            }

            // 3) spectral noise suppression + speech probability (0..1)
            let prob = match prep.as_mut() {
                Some(p) => p.run(&mut frame),
                None => 1.0,
            };

            // 4) voice activation: transmit only while speech is detected
            //    (threshold 0 = always on). Hangover keeps word tails intact.
            let thresh = ui.lock().unwrap().mic_threshold;
            let loud = prob * 100.0 >= thresh as f32;
            if loud {
                hold_until = now + VAD_HANGOVER;
            }
            let open = thresh == 0 || loud || now < hold_until;
            if ui.lock().unwrap().self_speaking != open {
                ui.lock().unwrap().self_speaking = open;
            }

            if !open {
                // staying quiet: drop backlog so nothing stale is sent later
                mic.lock().unwrap().clear();
                aec_ref.lock().unwrap().clear();
                refs.lock().unwrap().clear();
            } else {
                let pkt = enc.encode(&frame);
                if !pkt.is_empty() {
                    seq = seq.wrapping_add(1);
                    let mut body = Vec::with_capacity(4 + pkt.len());
                    body.extend_from_slice(&seq.to_le_bytes());
                    body.extend_from_slice(&pkt);
                    let direct = seal(msg(T_AUDIO, session, &body));

                    // WebSocket hub mode: the hub broadcasts to everyone
                    if ws_mode {
                        crate::ws::push(&ws_tx, direct.clone());
                    }

                    // UDP mesh: direct sends + server-relayed peers
                    let srv_addr = *srv.lock().unwrap();
                    let mut pl = peers.lock().unwrap();
                    for (s, p) in pl.iter_mut() {
                        if ws_mode {
                            // ws-discovered peers already receive `direct` via the hub
                            continue;
                        }
                        if p.path == PATH_RELAY {
                            // punched path not confirmed yet: go via server
                            if let Some(sa) = srv_addr {
                                let mut rb = Vec::with_capacity(4 + body.len());
                                rb.extend_from_slice(&s.to_le_bytes());
                                rb.extend_from_slice(&body);
                                let _ = sock.send_to(&seal(msg(T_S_RELAY, session, &rb)), sa);
                            }
                        } else {
                            let _ = sock.send_to(&direct, p.addr);
                        }
                    }
                }
            }
        }

        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now; // fell behind: resynchronize
        }
    }
}

/// Wait for exactly `n` samples; pad with silence if the mic starves.
fn pop_frame(mic: &MicQ, n: usize) -> Vec<f32> {
    let deadline = Instant::now() + Duration::from_millis(120);
    loop {
        {
            let mut q = mic.lock().unwrap();
            if q.len() >= n {
                return q.drain(..n).collect();
            }
        }
        if Instant::now() >= deadline {
            mic.lock().unwrap().clear();
            return vec![0.0; n];
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Non-blocking drain of up to `n` playback-reference samples, padded with
/// silence; if the queue runs long (clock skew) the oldest samples are
/// dropped so the AEC reference never lags behind the physical speakers.
fn pop_ref_frame(aec_ref: &MicQ, n: usize) -> Vec<f32> {
    let mut q = aec_ref.lock().unwrap();
    while q.len() > 4 * n {
        q.pop_front();
    }
    let take = q.len().min(n);
    let mut v: Vec<f32> = q.drain(..take).collect();
    v.resize(n, 0.0);
    v
}

fn send_to_peers(sock: &UdpSocket, peers: &Peers, m: &[u8]) {
    for p in peers.lock().unwrap().values() {
        let _ = sock.send_to(m, p.addr);
    }
}
