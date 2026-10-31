//! Headless rendezvous + relay server (`lanvc --serve`).
//!
//! Friends register via S_HELLO and get each other's public ip:port so they
//! can hole-punch; S_RELAY audio is forwarded between them. With a
//! passphrase set, every datagram must arrive inside a valid room-key
//! envelope — the server relays opaque blobs it cannot read.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use anyhow::Context as _;

use crate::crypt::KeyCache;
use crate::net::{parse, Rendezvous};

pub fn run(port: u16, passphrase: String) -> anyhow::Result<()> {
    let sock = UdpSocket::bind(("0.0.0.0", port))
        .with_context(|| format!("server: cannot bind UDP port {port}"))?;
    sock.set_read_timeout(Some(Duration::from_millis(500))).ok();

    let mut kc = KeyCache::default();
    let cipher = kc.refresh(&passphrase).cloned();
    let mut rz = Rendezvous::default();
    let mut buf = vec![0u8; 65536];
    let mut next_print = Instant::now();

    println!(
        "lanvc server listening on 0.0.0.0:{port} ({})",
        if cipher.is_some() { "room key required" } else { "OPEN — set --key to require the room key" }
    );

    loop {
        if let Ok((n, src)) = sock.recv_from(&mut buf) {
            let plain = match &cipher {
                Some(c) => KeyCache::open(c, &buf[..n]),
                None => Some(buf[..n].to_vec()),
            };
            if let Some(plain) = plain {
                if let Some(rx) = parse(&plain) {
                    match rx.ty {
                        crate::net::T_S_HELLO => {
                            rz.hello(&sock, &mut kc, cipher.as_ref(), rx.session, rx.body, src, None);
                        }
                        crate::net::T_S_RELAY => {
                            rz.relay(&sock, &mut kc, cipher.as_ref(), rx.session, rx.body, None);
                        }
                        _ => {}
                    }
                }
            }

            rz.retain();
            if Instant::now() >= next_print {
                next_print = Instant::now() + Duration::from_secs(10);
                println!("[server] {} peer(s) in roster", rz.peer_count());
            }
        }
    }
}
