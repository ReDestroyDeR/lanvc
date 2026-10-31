//! The GUI. Two tabs:
//!   ROOM  — what friends see: who is here, mic/speakers, volume
//!   HOST  — everything about running the meeting point (hub + share info)
//! Controls reflect their state; nothing may overflow: all widths derive
//! from `ui.available_width()`.

use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use crate::config;
use crate::net::Ui;
use crate::ws;

const GREEN: Color32 = Color32::from_rgb(35, 134, 54);
const GREEN_BRIGHT: Color32 = Color32::from_rgb(64, 224, 120);
const GRAY: Color32 = Color32::from_rgb(90, 94, 100);
const DIM: Color32 = Color32::from_rgb(70, 74, 80);
const BLUE: Color32 = Color32::from_rgb(120, 170, 255);
const RED: Color32 = Color32::from_rgb(210, 90, 80);

const HUB_PORT: u16 = 8080;

pub fn run(ui: Ui) -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([460.0, 620.0]),
        ..Default::default()
    };
    eframe::run_native(
        "LAN Voice",
        opts,
        Box::new(move |_cc| {
            Box::new(App {
                ui,
                saved: None,
                last_save: Instant::now(),
                tab: 0,
                hub: None,
                hub_err: String::new(),
            })
        }),
    )
}

struct App {
    ui: Ui,
    saved: Option<config::Config>,
    last_save: Instant,
    tab: usize, // 0 = room, 1 = host
    hub: Option<ws::Hub>,
    hub_err: String,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(120));
        egui::CentralPanel::default().show(ctx, |ui| {
            let mut s = self.ui.lock().unwrap();

            // ---- config autosave (debounced 1/s) -------------------------
            let now_cfg = config::Config {
                name: s.name.clone(),
                echo_cancel: s.aec_enabled,
                volume: s.volume,
                mic_threshold: s.mic_threshold,
                server: s.server.clone(),
                room_key: s.room_key.clone(),
                hosting: s.hosting,
            };
            if self.saved.as_ref() != Some(&now_cfg)
                && self.last_save.elapsed() > Duration::from_secs(1)
            {
                config::save(&now_cfg);
                self.saved = Some(now_cfg);
                self.last_save = Instant::now();
            }

            // ---- tab bar -------------------------------------------------
            ui.add_space(4.0);
            let tw = (ui.available_width() - 8.0) / 2.0;
            ui.horizontal(|ui| {
                if tab_btn(ui, self.tab == 0, "ROOM", tw).clicked() {
                    self.tab = 0;
                }
                if tab_btn(ui, self.tab == 1, "HOST", tw).clicked() {
                    self.tab = 1;
                }
            });
            ui.add_space(8.0);

            if self.tab == 1 {
                Self::host_tab(ui, &mut s, &self.hub, &self.hub_err);
            } else {
                Self::room_tab(ui, &mut s);
            }

            // ---- hosting: hub lifecycle follows the toggle ----------------
            if s.hosting && self.hub.is_none() {
                match ws::Hub::start(HUB_PORT) {
                    Ok(h) => {
                        self.hub = Some(h);
                        self.hub_err = String::new();
                    }
                    Err(e) => {
                        s.hosting = false;
                        self.hub_err = format!("{e:#}");
                    }
                }
            } else if !s.hosting && self.hub.is_some() {
                if let Some(h) = self.hub.take() {
                    h.stop();
                }
            }
        });
    }
}

impl App {
    fn room_tab(ui: &mut egui::Ui, s: &mut crate::net::UiState) {
        // header: title + occupancy
        ui.horizontal(|ui| {
            ui.heading(RichText::new("LAN Voice").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.0, GREEN_BRIGHT);
                ui.label(
                    RichText::new(format!("{} in room", s.peers.len() + 1)).weak().size(13.0),
                );
            });
        });
        ui.add_space(8.0);

        // room: peer list
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.add_space(2.0);
            peer_row(ui, &s.name, "you", !s.mic_muted && s.self_speaking);
            for p in &s.peers {
                let tag = if p.path == 1 {
                    "relay"
                } else if p.path == 2 {
                    "ws"
                } else {
                    "lan"
                };
                peer_row(ui, &p.name, &format!("{tag} · {}", p.addr), p.speaking);
            }
            if s.peers.is_empty() {
                ui.label(
                    RichText::new("nobody else here yet — waiting for friends…").weak().size(13.0),
                );
            }
        });
        ui.add_space(10.0);

        // transport
        let bw = (ui.available_width() - 8.0) / 2.0;
        ui.horizontal(|ui| {
            if toggle(ui, !s.mic_muted, "MIC", "MUTED", bw).clicked() {
                s.mic_muted = !s.mic_muted;
            }
            if toggle(ui, !s.spk_muted, "SPEAKERS", "SPK MUTED", bw).clicked() {
                s.spk_muted = !s.spk_muted;
            }
        });
        ui.add_space(8.0);

        // levels
        egui::Grid::new("levels")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .min_col_width(90.0)
            .show(ui, |ui| {
                ui.label("Volume");
                ui.add_sized(
                    [ui.available_width(), 18.0],
                    egui::Slider::new(&mut s.volume, 0.0..=1.0)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                );
                ui.end_row();
                ui.label(RichText::new("Mic gate").size(14.0));
                ui.add_sized(
                    [ui.available_width(), 18.0],
                    egui::Slider::new(&mut s.mic_threshold, 0..=100).custom_formatter(|v, _| {
                        if v <= 0.5 {
                            "always on".into()
                        } else {
                            format!("gate {v:.0}%")
                        }
                    }),
                );
                ui.end_row();
            });
        ui.add_space(4.0);

        // set-once settings, collapsed
        let open_by_default = s.server.is_empty();
        egui::CollapsingHeader::new(RichText::new("Connection & profile").size(15.0))
            .default_open(open_by_default)
            .show(ui, |ui| {
                egui::Grid::new("settings")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .min_col_width(110.0)
                    .show(ui, |ui| {
                        ui.label("Your name");
                        ui.add_sized(
                            [ui.available_width(), 20.0],
                            egui::TextEdit::singleline(&mut s.name),
                        );
                        ui.end_row();
                        ui.label("Echo cancel");
                        ui.checkbox(&mut s.aec_enabled, "open speakers in one room");
                        ui.end_row();
                        ui.label("Server");
                        ui.add_sized(
                            [ui.available_width(), 20.0],
                            egui::TextEdit::singleline(&mut s.server)
                                .hint_text("yourdomain.com:45454 or wss://…"),
                        );
                        ui.end_row();
                        ui.label("Room key");
                        ui.add_sized(
                            [ui.available_width(), 20.0],
                            egui::TextEdit::singleline(&mut s.room_key)
                                .password(true)
                                .hint_text("same on every device"),
                        );
                        ui.end_row();
                    });
            });

        // status, pinned to the bottom
        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.separator();
            ui.add_space(2.0);
            let port = s
                .self_addr
                .strip_prefix("0.0.0.0:")
                .unwrap_or(&s.self_addr)
                .to_string();
            let (dot, text) = if !s.error.is_empty() {
                (RED, s.error.clone())
            } else if s.hosting {
                (GREEN_BRIGHT, "hosting — friends join via your domain + room key".to_string())
            } else if !s.server.is_empty() {
                (BLUE, format!("via {}", s.server))
            } else {
                (DIM, format!("LAN mode — listening on :{port}"))
            };
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 3.0, dot);
                ui.label(RichText::new(text).weak().size(12.0));
            });
        });
    }

    fn host_tab(
        ui: &mut egui::Ui,
        s: &mut crate::net::UiState,
        hub: &Option<ws::Hub>,
        hub_err: &str,
    ) {
        ui.heading(RichText::new("Host a room").strong());
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "This PC becomes the meeting point: friends connect through your \
                 domain, and their NATs never matter.",
            )
            .weak()
            .size(13.0),
        );
        ui.add_space(10.0);

        let running = hub.is_some();
        let big = ui.available_width().min(280.0);
        if running {
            if toggle(ui, true, "HOSTING — CLICK TO STOP", "", big).clicked() {
                s.hosting = false;
            }
        } else if toggle(ui, false, "START HOSTING", "START HOSTING", big).clicked() {
            s.hosting = true;
        }
        ui.add_space(2.0);
        if !hub_err.is_empty() {
            ui.label(RichText::new(hub_err).color(RED).size(13.0));
        }
        ui.add_space(6.0);

        let (clients_n, port) = match hub {
            Some(h) => (h.connected(), h.port),
            None => (0, HUB_PORT),
        };
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new("Status").strong().size(14.0));
            ui.label(
                RichText::new(if running {
                    format!("room hub running on tcp :{port} — {clients_n} friend(s) connected")
                } else {
                    "room hub stopped".to_string()
                })
                .size(13.0),
            );
            ui.add_space(4.0);
            ui.label(RichText::new("Friends enter:").strong().size(14.0));
            let share = if crate::ws::is_ws_url(&s.server) {
                s.server.clone()
            } else {
                "wss://your-domain/  — set it under ROOM > Connection & profile".to_string()
            };
            ui.label(RichText::new(share).size(13.0));
            ui.label(
                RichText::new("…plus the room key (same on every device)").weak().size(12.0),
            );
        });
        ui.add_space(8.0);

        egui::CollapsingHeader::new(RichText::new("How hosting reaches the internet").size(14.0))
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    RichText::new(
                        "Friends' WebSocket sessions must reach tcp :8080 on this PC. \
                         Easiest: a cloudflared tunnel routing your domain to \
                         http://localhost:8080 (outbound only — no port forwarding, \
                         survives CGNAT). Alternatives: a router port-forward of \
                         UDP+TCP 45454/8080, or hosting on a VPS.",
                    )
                    .size(13.0),
                );
            });

        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.separator();
            ui.add_space(2.0);
            ui.label(
                RichText::new("keep this window open while hosting — the room lives here")
                    .weak()
                    .size(12.0),
            );
        });
    }
}

fn tab_btn(ui: &mut egui::Ui, active: bool, label: &str, width: f32) -> egui::Response {
    let (text, fill) = if active {
        (RichText::new(label).strong().color(Color32::WHITE), Color32::from_rgb(60, 64, 72))
    } else {
        (RichText::new(label).weak(), Color32::TRANSPARENT)
    };
    ui.add_sized(
        [width, 26.0],
        egui::Button::new(text).fill(fill).stroke(egui::Stroke::NONE),
    )
}

fn toggle(
    ui: &mut egui::Ui,
    on: bool,
    on_label: &str,
    off_label: &str,
    width: f32,
) -> egui::Response {
    let (label, fill) = if on { (on_label, GREEN) } else { (off_label, GRAY) };
    ui.add_sized(
        [width, 38.0],
        egui::Button::new(RichText::new(label).color(Color32::WHITE).strong())
            .fill(fill)
            .min_size(egui::vec2(width, 38.0)),
    )
}

fn peer_row(ui: &mut egui::Ui, name: &str, sub: &str, speaking: bool) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
        let (color, radius) = if speaking {
            (GREEN_BRIGHT, 6.0)
        } else {
            (DIM, 4.5)
        };
        ui.painter().circle_filled(rect.center(), radius, color);
        ui.label(RichText::new(name).size(15.0));
        if !sub.is_empty() {
            ui.label(RichText::new(sub).weak().size(12.0));
        }
    });
}
