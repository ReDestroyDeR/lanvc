# lanvc — LAN voice chat

Zero-config, single-room voice chat for your LAN. One `.exe`, no server,
no accounts: run it on every machine and everyone in the room shows up
automatically (UDP broadcast discovery, full mesh).

- **Codec:** Opus, 10 ms frames @ 48 kHz mono, ~32 kbit/s (libopus statically
  linked). Protocol v2 — v1 (Vorbis) exes do not interoperate; everyone
  should run the same build.
- **Transport:** raw UDP, port **45454** (change with `--port`)
- **Latency:** ~10 ms frames + ~17 ms codec lookahead + ~10 ms device
  buffers per hop; round trip across two machines lands around 40–70 ms.
  The network itself is < 1 ms on a LAN.

## Use

1. Copy `lanvc.exe` to each PC and run it.
2. On first launch Windows asks about the firewall — **allow on private
   networks** (this app only listens on the LAN).
3. Optionally edit your display name, then just talk.

Use headphones: there is no echo cancellation, open speakers will feed back.

Buttons: `MIC ON/MUTED`, `SPEAKERS ON/MUTED`, master volume. The green dot
next to a name means that person is talking.

## Mic threshold and noise suppression

**Mic threshold** (default 35%) is voice activation: your mic transmits only
while the speech probability (speexdsp VAD) is above the threshold, with a
0.4 s hangover so word tails survive. Slide it to 0 for always-on. The dot
next to your own name shows exactly when you are transmitting — use it to
set the threshold. Spectral noise suppression (-25 dB, always on) attenuates
steady room noise and keyboard clatter under your voice.

## Same room, open speakers

Two processors attack the "everyone's mic hears everyone" problem:

1. **Per-peer voice removal (always on).** Your machine already decodes each
   peer's clean voice; before you transmit, an adaptive filter per peer
   (speexdsp MDF) subtracts that reference from your own mic capture. Your
   uplink ends up carrying only *your* near-end voice, so every voice is
   heard once instead of arriving once per person in the room. Needs
   ~0.5 s to learn each voice after they start talking; foreign voices are
   attenuated strongly (tens of dB), with a faint roomy residue possible.
2. **Echo cancel (toggle, off by default).** Additionally subtracts what
   your own speakers just played from your mic — use it when everyone is on
   open speakers, leave off with headsets. It also stops the
   speakers→mic→uplink howl loop.

Both stack: #1 removes voices arriving through the air, #2 removes them
arriving through your speakers. What no mic processing can do is stop you
from *hearing* other people's speakers — lower their volume or use headsets
for that.

## Friends outside your LAN (three ways)

**1. WebSocket room via your Cloudflare domain (easiest for friends).**
Deploy the included hub to your domain (from `cloudflare/`):

    cd cloudflare && npx wrangler deploy

then in the dashboard bind it to `room.example.com` (Workers > your worker >
Settings > Domains & Routes > custom domain). Friends set
**Server** = `wss://room.example.com/` plus the room key. All audio relays
through the Cloudflare edge encrypted (the Worker only sees opaque blobs);
works through any NAT, CGNAT or firewall since it is plain WSS on 443.
A self-hostable variant of the same hub ships as `lanvc --wshub` (TCP).

**2. Host from the UI.** Click **HOST ROOM** and forward UDP 45454 to your
PC (port-forward + DDNS). Your instance is the meeting point: it answers
friends' HELLOs, introduces them and relays audio when NATs refuse direct
paths. UI tags each peer `[lan]`, `[p2p]` (punched) or `[relay]`.

**3. Dedicated box:** `lanvc --serve --key <roomkey>` on any UDP-reachable
machine — same client setup as (2).

The room key authenticates and encrypts everything (XChaCha20-Poly1305);
without it, LAN mode stays plaintext and keyless. Mixing is client-side:
everyone hears every voice exactly once, from LAN, P2P or relayed sources
alike.

Click **HOST ROOM** (or start with `--host`): your instance becomes the
meeting point — it answers friends' HELLOs, introduces them to each other
and relays audio if their NATs refuse direct paths. No second program.

For friends to reach you, this PC must accept inbound **UDP 45454**:

- forward UDP 45454 on your router to this PC (one-time), and
- point your domain's A record at your internet IP (dynamic-DNS client if
  it changes).

Friends then set **Server** = `yourdomain.com:45454`, the same **Room
key**, and everyone's UI tags each peer `[p2p]` (direct, punched) or
`[relay]` (through you).

For a dedicated always-on box you can instead run `lanvc --serve --key
<roomkey>` there (rendezvous + relay only, no voice) — same client setup.
A Cloudflare Worker cannot do this (no raw UDP); a tunnel would require
extra client software on every friend's PC.

The room key is required for internet use: it authenticates the room and
encrypts every packet (XChaCha20-Poly1305); the relay only ever sees
encrypted blobs. Keyless LAN mode stays plaintext and keyless.

A Cloudflare Worker cannot relay UDP — host `--serve` on a PC/VPS instead
(a free-tier tunnel does not carry raw UDP either without extra client
software on every friend's machine).

## Settings

Name, echo cancel, master volume, mic threshold, server, room key and the
hosting toggle persist in `%APPDATA%\lanvc\config.toml` (Windows) or
`~/.config/lanvc/config.toml` (Linux/macOS) — plaintext, written ~1 s
after any change.

## Console output

The exe is a GUI binary — no terminal pops up. Output is not lost:
launched from a terminal (`--selftest`, `--headless`) it prints there;
launched without one (double-click), stdout/stderr go to
`%TEMP%\lanvc.log` (truncated each run — panics included).

## CLI

    lanvc.exe                 # normal GUI mode
    lanvc.exe --port 45455    # different UDP port (e.g. two instances on one PC)
    lanvc.exe --selftest      # encode/decode round-trip check, no audio needed
    lanvc.exe --headless      # join the room with no GUI (for testing)

## Build (Windows x64, cross-compiled from Linux)

    rustup target add x86_64-pc-windows-gnu
    cargo build --release --target x86_64-pc-windows-gnu
    # -> target/x86_64-pc-windows-gnu/release/lanvc.exe

libogg 1.3.5 / libvorbis 1.3.7 are vendored under `vendor/` and compiled by
`build.rs` with the mingw-w64 cross compiler — nothing else is needed.

## Notes

- Discovery uses `255.255.255.255` broadcasts plus unicast; every instance
  also re-announces directly to known peers, so one-sided broadcast blocking
  usually still works. Multiple NICs/VPNs: broadcasts follow the default route.
- Everyone decodes and mixes every other peer locally; there is no server
  and nothing leaves the LAN. Packets are unauthenticated UDP — use it on
  networks you trust.
