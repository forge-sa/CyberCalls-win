use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Shown above your own bubbles.
    pub my_name: String,
    pub model: PathBuf,
    pub use_gpu: bool,
    pub threads: i32,
    /// Languages spoken on calls; each phrase is matched to one of these.
    pub languages: Vec<String>,
    /// Executable name of the Telegram client, e.g. "AyuGram.exe".
    pub app_exe: String,
    pub overlay: OverlayConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OverlayConfig {
    /// Horizontal center of the bubble column, as a fraction of screen width.
    pub center_x: f32,
    /// Top of the bubble column, as a fraction of screen height.
    pub top: f32,
    /// Bottom limit of the column, as a fraction of screen height.
    pub bottom: f32,
    pub column_width: f32,
    /// Side of the CALL box in pixels; everything else scales with it.
    pub box_size: f32,
    /// Seconds a finished line stays before fading out.
    pub linger_secs: f32,
    /// Typewriter speed.
    pub chars_per_sec: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            my_name: "frantix".into(),
            model: PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/models/ggml-small-q5_1.bin")),
            use_gpu: true,
            threads: 8,
            languages: vec!["en".into(), "ru".into()],
            app_exe: "AyuGram.exe".into(),
            overlay: OverlayConfig::default(),
        }
    }
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            center_x: 0.5,
            top: 0.10,
            bottom: 0.92,
            column_width: 900.0,
            box_size: 112.0,
            linger_secs: 14.0,
            chars_per_sec: 55.0,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        dirs::config_dir().unwrap_or_else(|| ".".into()).join("cybercalls/config.toml")
    }

    /// Loads `%APPDATA%\cybercalls\config.toml`; missing keys fall back to defaults.
    pub fn load() -> Result<Self> {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}
