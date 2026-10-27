use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    net::SocketAddr,
    path::{Path, PathBuf},
};

const MINI1: &str = include_str!("../profiles/mini1.json");
const MINI2: &str = include_str!("../profiles/mini2.json");
const GENERIC: &str = include_str!("../profiles/generic.json");

/// Hostnames (and their subdomains) that the DPI bypass applies to by default.
pub const DEFAULT_DOMAINS: &[&str] = &["soundcloud.com", "sndcdn.com", "soundcloud.cloud"];

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct Audio {
    pub device: String,
    pub mixer_device: String,
    pub mixer_control: String,
    pub mixer_index: u32,
    pub mixer_channel: String,
}
impl Default for Audio {
    fn default() -> Self {
        Self {
            device: "default".into(),
            mixer_device: "default".into(),
            mixer_control: "Master".into(),
            mixer_index: 0,
            mixer_channel: "front_left".into(),
        }
    }
}
impl Audio {
    pub fn channel(&self) -> alsa::mixer::SelemChannelId {
        use alsa::mixer::SelemChannelId;
        match self.mixer_channel.as_str() {
            "front_right" => SelemChannelId::FrontRight,
            "mono" => SelemChannelId::mono(),
            _ => SelemChannelId::FrontLeft,
        }
    }
}
#[derive(Clone, Deserialize)]
pub struct Voice {
    pub address: SocketAddr,
}
#[derive(Clone, Deserialize)]
pub struct Profile {
    pub id: String,
    #[serde(default)]
    pub audio: Audio,
    #[serde(default)]
    pub voice: Option<Voice>,
}

pub fn profile(selection: &str) -> io::Result<Profile> {
    let text = match selection {
        "mini1" => MINI1,
        "mini2" => MINI2,
        "generic" => GENERIC,
        // Mini 2 has the LED ring this sysfs node comes from.
        "auto" if Path::new("/sys/class/leds").exists() && has_mini2_leds() => MINI2,
        "auto" if Path::new("/system/vendor/quasar").exists() => MINI1,
        "auto" => GENERIC,
        other => {
            return serde_json::from_str(&fs::read_to_string(other)?)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
    };
    serde_json::from_str(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
fn has_mini2_leds() -> bool {
    fs::read_dir("/sys/class/leds")
        .map(|dir| {
            dir.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("mini2:"))
        })
        .unwrap_or(false)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Bypass {
    /// `auto`, `off`, or the name of one strategy: a Flowseal preset (`alt11`, `general`,
    /// `simple-fake`, ... see `soundiya strategies`) or one of the built-in tricks (`split`,
    /// `tlsrec`, `both`, `disorder`, `oob`).
    #[serde(alias = "mode")]
    pub strategy: String,
    /// Resolve blocked hosts over DNS-over-HTTPS instead of the poisoned system resolver.
    pub doh: bool,
    /// Extra domains to treat as blocked, besides the SoundCloud ones.
    pub domains: Vec<String>,
    /// Optional `host:port` of an HTTP CONNECT proxy to tunnel through (for IP-level blocks).
    pub upstream_proxy: Option<String>,
}
impl Default for Bypass {
    fn default() -> Self {
        Self {
            strategy: "auto".into(),
            doh: true,
            domains: Vec::new(),
            upstream_proxy: None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub profile: String,
    pub voice: bool,
    /// Let the top (Alice) button pause and resume the music while it is loaded.
    pub buttons: bool,
    pub bypass: Bypass,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            profile: "auto".into(),
            voice: true,
            buttons: true,
            bypass: Bypass::default(),
        }
    }
}

pub fn load(state_dir: &Path) -> io::Result<Config> {
    match fs::read_to_string(state_dir.join("config.json")) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("config.json is invalid: {e}"),
            )
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e),
    }
}

/// Stores the chosen strategy in `config.json`, leaving every other setting as it was.
pub fn save_strategy(state_dir: &Path, strategy: &str) -> io::Result<()> {
    let path = state_dir.join("config.json");
    let mut config: serde_json::Value = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("config.json is invalid: {e}")))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e),
    };
    let bypass = config
        .as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "config.json is not an object"))?
        .entry("bypass")
        .or_insert_with(|| serde_json::json!({}));
    let bypass = bypass
        .as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "config.json: bypass is not an object"))?;
    // The old name for the same setting must not shadow the new one.
    bypass.remove("mode");
    bypass.insert("strategy".into(), strategy.into());
    fs::create_dir_all(state_dir)?;
    write_private(&path, &serde_json::to_string_pretty(&config).map_err(io::Error::other)?)
}

pub fn write_private(path: &PathBuf, data: &str) -> io::Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let temp = path.with_extension("new");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(data.as_bytes())?;
    file.sync_all()?;
    fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn embedded_profiles_parse() {
        for id in ["mini1", "mini2", "generic"] {
            assert_eq!(profile(id).unwrap().id, id);
        }
        assert!(profile("mini2").unwrap().voice.is_some());
        assert!(profile("generic").unwrap().voice.is_none());
    }
    #[test]
    fn partial_config_uses_defaults() {
        let config: Config = serde_json::from_str(r#"{"bypass":{"strategy":"alt11"}}"#).unwrap();
        assert_eq!(config.bypass.strategy, "alt11");
        assert!(config.bypass.doh && config.voice);
        assert_eq!(Config::default().bypass.strategy, "auto");
        // The older spelling of the setting still works.
        let old: Config = serde_json::from_str(r#"{"bypass":{"mode":"tlsrec"}}"#).unwrap();
        assert_eq!(old.bypass.strategy, "tlsrec");
    }
    #[test]
    fn saving_a_strategy_keeps_the_other_settings() {
        let dir = std::env::temp_dir().join(format!("soundiya-config-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("config.json"),
            r#"{"voice":false,"bypass":{"mode":"split","doh":false,"domains":["a.test"]}}"#,
        )
        .unwrap();
        save_strategy(&dir, "alt9").unwrap();
        let config = load(&dir).unwrap();
        assert_eq!(config.bypass.strategy, "alt9");
        assert!(!config.voice && !config.bypass.doh);
        assert_eq!(config.bypass.domains, ["a.test"]);
        let text = fs::read_to_string(dir.join("config.json")).unwrap();
        assert!(!text.contains("\"mode\""), "{text}");
        // And it creates the file when there is none.
        let fresh = dir.join("fresh");
        save_strategy(&fresh, "general").unwrap();
        assert_eq!(load(&fresh).unwrap().bypass.strategy, "general");
        fs::remove_dir_all(&dir).unwrap();
    }
}
