//! Persisted settings: a tiny hand-rolled key=value file in the user's
//! config dir (`config_dir/lanvc/config.toml`). Plaintext on purpose —
//! it lives on the user's own machine; note the room key is stored as-is.

use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub name: String,
    pub echo_cancel: bool,
    pub volume: f32,
    pub mic_threshold: u8,
    pub server: String,
    pub room_key: String,
    pub hosting: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: String::new(),
            echo_cancel: false,
            volume: 1.0,
            mic_threshold: 35,
            server: String::new(),
            room_key: String::new(),
            hosting: false,
        }
    }
}

fn path() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var("APPDATA").ok().map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config"));
    base.map(|d| {
        let dir = d.join("lanvc");
        let _ = fs::create_dir_all(&dir);
        dir.join("config.toml")
    })
}

pub fn load() -> Config {
    let mut c = Config::default();
    let Some(path) = path() else { return c };
    let Ok(text) = fs::read_to_string(path) else { return c };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim().trim_matches('"'));
        match k {
            "name" => c.name = v.to_string(),
            "echo_cancel" => c.echo_cancel = v == "true",
            "volume" => c.volume = v.parse().unwrap_or(c.volume),
            "mic_threshold" => c.mic_threshold = v.parse().unwrap_or(c.mic_threshold),
            "server" => c.server = v.to_string(),
            "room_key" => c.room_key = v.to_string(),
            "hosting" => c.hosting = v == "true",
            _ => {}
        }
    }
    c
}

pub fn save(c: &Config) {
    let Some(path) = path() else { return };
    let body = format!(
        "name = \"{}\"\necho_cancel = {}\nvolume = {}\nmic_threshold = {}\nserver = \"{}\"\nroom_key = \"{}\"\nhosting = {}\n",
        c.name.replace('\\', "\\\\").replace('"', "\\\""),
        c.echo_cancel,
        c.volume,
        c.mic_threshold,
        c.server.replace('\\', "\\\\").replace('"', "\\\""),
        c.room_key.replace('\\', "\\\\").replace('"', "\\\""),
        c.hosting,
    );
    let _ = fs::write(path, body);
}
