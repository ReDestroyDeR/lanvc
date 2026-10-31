//! WebSocket room transport: connects to a broadcast hub (a Cloudflare
//! Worker on wss://voip.example.com, or a local `lanvc --wshub`) and
//! exchanges the exact same sealed packets as the UDP path — one binary
//! frame per packet, the hub broadcasts each frame to everyone else.
//! Nothing needs port forwarding: one outbound WSS per client passes every
//! NAT and firewall, and the room key keeps the hub blind to the audio.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::crypt::KeyCache;
use crate::net;

/// Outbound queue the tx thread fills; the ws thread drains it.
pub type WsTx = Arc<Mutex<VecDeque<Vec<u8>>>>;

pub fn new_tx() -> WsTx {
    Arc::new(Mutex::new(VecDeque::new()))
}

pub fn push(tx: &WsTx, frame: Vec<u8>) {
    let mut q = tx.lock().unwrap();
    if q.len() > 512 {
        q.pop_front();
    }
    q.push_back(frame);
}

pub fn is_ws_url(server: &str) -> bool {
    server.starts_with("ws://") || server.starts_with("wss://")
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_room(
    url: String,
    session: u32,
    ui: net::Ui,
    peers: net::Peers,
    voices: net::Voices,
    refs: net::Refs,
    out_rate: u32,
    txq: WsTx,
) {
    std::thread::Builder::new()
        .name("ws-room".into())
        .spawn(move || loop {
            match connect(&url) {
                Ok(ws) => {
                    let hub = peer_addr(&ws);
                    eprintln!("[ws] connected {url}");
                    let r = room_loop(
                        ws, hub, session, &ui, &peers, &voices, &refs, out_rate, &txq,
                    );
                    eprintln!("[ws] closed ({r:?})");
                }
                Err(e) => eprintln!("[ws] {url}: {e}"),
            }
            std::thread::sleep(Duration::from_secs(3));
        })
        .expect("spawn ws thread");
}

fn peer_addr(ws: &WebSocket<MaybeTlsStream<TcpStream>>) -> SocketAddr {
    match ws.get_ref() {
        MaybeTlsStream::Plain(s) => {
            s.peer_addr().unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)))
        }
        other => match other {
            tungstenite::stream::MaybeTlsStream::Rustls(s) => {
                s.get_ref().peer_addr().unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)))
            }
            _ => SocketAddr::from(([0, 0, 0, 0], 0)),
        },
    }
}

fn connect(url: &str) -> tungstenite::Result<WebSocket<MaybeTlsStream<TcpStream>>> {
    let (ws, _resp) = tungstenite::connect(url)?;
    set_timeout(&ws, Duration::from_millis(20));
    Ok(ws)
}

fn set_timeout(ws: &WebSocket<MaybeTlsStream<TcpStream>>, d: Duration) {
    let r = match ws.get_ref() {
        MaybeTlsStream::Plain(s) => s.set_read_timeout(Some(d)),
        other => match other {
            tungstenite::stream::MaybeTlsStream::Rustls(s) => {
                s.get_ref().set_read_timeout(Some(d))
            }
            _ => Ok(()),
        },
    };
    let _ = r;
}

#[allow(clippy::too_many_arguments)]
fn room_loop(
    mut ws: WebSocket<MaybeTlsStream<TcpStream>>,
    hub: SocketAddr,
    session: u32,
    ui: &net::Ui,
    peers: &net::Peers,
    voices: &net::Voices,
    refs: &net::Refs,
    out_rate: u32,
    txq: &WsTx,
) -> tungstenite::Result<()> {
    let mut decoders: HashMap<u32, crate::opus::Decoder> = HashMap::new();
    let mut kc = KeyCache::default();
    set_timeout(&ws, Duration::from_millis(20));
    loop {
        // drain the outbound queue
        loop {
            let frame = txq.lock().unwrap().pop_front();
            let Some(frame) = frame else { break };
            ws.write(Message::Binary(frame))?;
        }
        ws.flush()?;
        // inbound
        let msg = match ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                continue; // read timeout: nothing inbound right now
            }
            Err(e) => return Err(e),
        };
        match msg {
            Message::Binary(data) => {
                let cipher = kc.refresh(&ui.lock().unwrap().room_key).cloned();
                let plain = match &cipher {
                    Some(c) => KeyCache::open(c, &data),
                    None => Some(data),
                };
                if let Some(plain) = plain {
                    dispatch(&plain, hub, session, peers, voices, refs, &mut decoders, out_rate);
                }
            }
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch(
    plain: &[u8],
    hub: SocketAddr,
    session: u32,
    peers: &net::Peers,
    voices: &net::Voices,
    refs: &net::Refs,
    decoders: &mut HashMap<u32, crate::opus::Decoder>,
    out_rate: u32,
) {
    let Some(rx) = net::parse(plain) else { return };
    if rx.session == session {
        return;
    }
    let srv: net::Srv = Arc::new(Mutex::new(Some(hub)));
    match rx.ty {
        net::T_ANNOUNCE => {
            if let Some(name) = net::parse_announce(rx.body).map(|(_, n)| n) {
                net::upsert_peer(peers, rx.session, hub, name, net::PATH_WS);
            }
        }
        net::T_AUDIO => net::handle_audio(
            rx.session, rx.body, hub, false, &srv, peers, voices, refs, decoders, out_rate,
        ),
        _ => {}
    }
}

/// Self-hostable broadcast hub (`lanvc --wshub`, TCP): same semantics as
/// the Cloudflare Worker — every binary frame is relayed to all other
/// connected clients, uninterpreted.
/// Minimal hand-rolled WebSocket framing for the hub: our clients only
/// ever send small MASKED binary frames; we broadcast the payload to all
/// other clients as an UNMASKED frame. Ping/Close handled minimally.
fn read_frame(stream: &mut TcpStream) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut hdr = [0u8; 2];
    stream.read_exact(&mut hdr)?;
    let opcode = hdr[0] & 0x0F;
    let masked = hdr[1] & 0x80 != 0;
    let mut len = (hdr[1] & 0x7F) as usize;
    if len == 126 {
        let mut ext = [0u8; 2];
        stream.read_exact(&mut ext)?;
        len = u16::from_be_bytes(ext) as usize;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        stream.read_exact(&mut ext)?;
        len = u64::from_be_bytes(ext) as usize;
    }
    let mask = if masked {
        let mut m = [0u8; 4];
        stream.read_exact(&mut m)?;
        Some(m)
    } else {
        None
    };
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    if let Some(m) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i % 4];
        }
    }
    Ok(Some((opcode, payload)))
}

fn write_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut out = vec![0x80 | opcode];
    if payload.len() < 126 {
        out.push(payload.len() as u8);
    } else {
        out.push(126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    out.extend_from_slice(payload);
    stream.write_all(&out)
}

/// A started, stoppable hub. Drop (or `stop()`) to shut it down.
pub struct Hub {
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicUsize>,
    pub port: u16,
}

impl Hub {
    pub fn start(port: u16) -> anyhow::Result<Hub> {
        use std::net::TcpListener;
        let listener = TcpListener::bind(("0.0.0.0", port))
            .with_context(|| format!("cannot bind TCP port {port} for the room hub"))?;
        listener.set_nonblocking(true).ok();
        let stop = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicUsize::new(0));
        let (stop2, count2) = (stop.clone(), connected.clone());

        let clients: Arc<Mutex<Vec<Arc<Mutex<TcpStream>>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let clients2 = clients.clone();
        let _ = std::thread::Builder::new()
            .name("ws-hub".into())
            .spawn(move || {
                loop {
                    if stop2.load(Ordering::Relaxed) {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let clients = clients2.clone();
                            let stop2 = stop2.clone();
                            let count2 = count2.clone();
                            std::thread::spawn(move || {
                                let mut stream = stream;
                                // handshake on a BLOCKING socket first
                                let _ = stream.set_nonblocking(false);
                                let mut buf = [0u8; 1024];
                                let n = match stream.read(&mut buf) {
                                    Ok(n) => n,
                                    Err(_) => return,
                                };
                                let req = String::from_utf8_lossy(&buf[..n]);
                                let key = req
                                    .lines()
                                    .find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-key:"))
                                    .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()));
                                let Some(key) = key else { return };
                                use sha1::{Digest, Sha1};
                                let mut h = Sha1::new();
                                h.update(key.as_bytes());
                                h.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
                                let accept = base64_table(&h.finalize());
                                let resp = format!(
                                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                                );
                                if stream.write_all(resp.as_bytes()).is_err() {
                                    return;
                                }
                                stream.set_nonblocking(true).ok();
                                count2.fetch_add(1, Ordering::Relaxed);
                                let stream = Arc::new(Mutex::new(stream));
                                clients.lock().unwrap().push(stream.clone());

                                loop {
                                    if stop2.load(Ordering::Relaxed) {
                                        break;
                                    }
                                    std::thread::sleep(Duration::from_millis(5));
                                    let mut frames: Vec<(u8, Vec<u8>)> = Vec::new();
                                    let mut gone = false;
                                    {
                                        let mut w = stream.lock().unwrap();
                                        loop {
                                            match read_frame(&mut w) {
                                                Ok(Some((opcode, payload))) => {
                                                    frames.push((opcode, payload))
                                                }
                                                Ok(None) => {}
                                                Err(e)
                                                    if e.kind() == std::io::ErrorKind::WouldBlock =>
                                                {
                                                    break; // no more data right now
                                                }
                                                Err(_) => {
                                                    gone = true;
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    if gone {
                                        break;
                                    }
                                    if !frames.is_empty() {
                                        let cs = clients.lock().unwrap();
                                        for c in cs.iter() {
                                            if Arc::ptr_eq(c, &stream) {
                                                continue;
                                            }
                                            let mut w = c.lock().unwrap();
                                            for (opcode, payload) in frames.iter() {
                                                if let Err(e) = write_frame(&mut w, *opcode, payload) {
                                                    let _ = e; // drop on dead socket
                                                }
                                            }
                                        }
                                    }
                                }
                                count2.fetch_sub(1, Ordering::Relaxed);
                                clients.lock().unwrap().retain(|c| !Arc::ptr_eq(c, &stream));
                            });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => break,
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            });
        Ok(Hub { stop, connected, port })
    }

    pub fn connected(&self) -> usize {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Standalone hub process (`--wshub`): starts and runs forever.
pub fn run_hub(port: u16) -> anyhow::Result<()> {
    let hub = Hub::start(port)?;
    println!("lanvc ws hub listening on 0.0.0.0:{} (tcp — broadcast relay)", hub.port);
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn base64_table(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}
