//! `%APPDATA%\stereo-pair\config.toml` (speakers + streaming settings) and
//! `%APPDATA%\stereo-pair\credentials.toml` (per-speaker HAP pairing keys).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::dsp::{ChannelSource, DspSettings};
use crate::hap::{Credentials, HapCredentials};

pub const DEFAULT_CLIENT_NAME: &str = "Stereo Pair";
/// X-Apple-Device-ID the spike paired the Home View with (patched pyatv hap.py).
pub const SPIKE_APPLE_DEVICE_ID: &str = "0x569EF725E5A4";

pub fn app_dir() -> Result<PathBuf> {
    let base = std::env::var_os("APPDATA").ok_or_else(|| anyhow!("APPDATA is not set"))?;
    Ok(PathBuf::from(base).join("stereo-pair"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(app_dir()?.join("config.toml"))
}

pub fn credentials_path() -> Result<PathBuf> {
    Ok(app_dir()?.join("credentials.toml"))
}

fn default_latency_ms() -> u32 {
    crate::raop::timeline::DEFAULT_LATENCY_FRAMES * 1000 / crate::raop::timeline::SAMPLE_RATE
}
fn default_true() -> bool {
    true
}
fn default_client_name() -> String {
    DEFAULT_CLIENT_NAME.to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// Send-to-play latency shared by all speakers (ms). pyatv uses 1500; 300 is a
    /// lower setting to experiment with. Clamped to 100..2000 (latencyMin/Max we
    /// announce in SETUP).
    #[serde(default = "default_latency_ms")]
    pub latency_ms: u32,
    /// Fixed AirPlay device volume in dB (-30..0, -144 = mute) sent once after RECORD.
    /// Leave unset to keep each speaker's current volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_volume_db: Option<f32>,
    /// Apply the Windows master volume/mute as a digital gain.
    #[serde(default = "default_true")]
    pub follow_windows_volume: bool,
    /// UDP port of the NTP timing server (0 = any free port).
    #[serde(default)]
    pub timing_port: u16,
    /// Name shown on the speakers when pairing.
    #[serde(default = "default_client_name")]
    pub client_name: String,
    /// Tray app: start streaming as soon as it launches.
    #[serde(default = "default_true")]
    pub auto_start: bool,
    #[serde(default, rename = "speaker")]
    pub speakers: Vec<SpeakerConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SpeakerConfig {
    /// Display/mDNS name, e.g. "Living Room".
    pub name: String,
    /// IPv4 address; if omitted the name is resolved over mDNS at start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    /// AirPlay port if not the usual 7000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// left | right | mono | stereo
    pub role: ChannelSource,
    #[serde(default)]
    pub gain_db: f32,
    /// 0..500 ms, sample accurate.
    #[serde(default)]
    pub delay_ms: f32,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl SpeakerConfig {
    pub fn new(name: &str, ip: &str, role: ChannelSource) -> Self {
        Self { name: name.into(), ip: Some(ip.into()), port: None, role, gain_db: 0.0, delay_ms: 0.0, enabled: true }
    }

    pub fn dsp(&self) -> DspSettings {
        DspSettings { source: self.role, gain_db: self.gain_db, delay_ms: self.delay_ms }
    }

    /// Stable identity of the endpoint ("ip:port", or "mdns:<name>" without an ip).
    pub fn key(&self) -> String {
        match &self.ip {
            Some(ip) => format!("{ip}:{}", self.port.unwrap_or(crate::airplay::session::AIRPLAY_PORT)),
            None => format!("mdns:{}", self.name),
        }
    }
}

/// Write via a temp file + rename so a crash mid-write never leaves a truncated file.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    std::fs::create_dir_all(path.parent().ok_or_else(|| anyhow!("{} has no parent", path.display()))?)?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

impl Default for Config {
    fn default() -> Self {
        Self {
            latency_ms: default_latency_ms(),
            device_volume_db: None,
            follow_windows_volume: true,
            timing_port: 0,
            client_name: default_client_name(),
            auto_start: true,
            speakers: vec![
                SpeakerConfig::new("Living Room", "192.168.1.50", ChannelSource::Left),
                SpeakerConfig::new("Living Room (2)", "192.168.1.51", ChannelSource::Right),
            ],
        }
    }
}

const CONFIG_HEADER: &str = "\
# stereo-pair configuration
# latency_ms            shared send->play latency for every speaker (pyatv: 1500; try 300)
# device_volume_db      optional fixed AirPlay volume (-30..0 dB) set once per session
# follow_windows_volume apply Windows master volume + mute as digital gain
# auto_start            tray app: start streaming on launch
# [[speaker]]           role = left | right | mono | stereo, gain_db, delay_ms (0..500)
";

impl Config {
    /// Load the config, writing the default one first if it does not exist.
    pub fn load_or_create() -> Result<(Self, PathBuf, bool)> {
        let path = config_path()?;
        let (cfg, created) = Self::load_or_create_at(&path)?;
        Ok((cfg, path, created))
    }

    pub fn load_or_create_at(path: &Path) -> Result<(Self, bool)> {
        if !path.exists() {
            let cfg = Config::default();
            cfg.save_to(path)?;
            return Ok((cfg, true));
        }
        Ok((Self::load_from(path)?, false))
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        write_atomic(path, &format!("{CONFIG_HEADER}\n{}", toml::to_string_pretty(self)?))
    }

    pub fn latency_frames(&self) -> u32 {
        use crate::raop::timeline::{LATENCY_MAX_FRAMES, LATENCY_MIN_FRAMES};
        ((self.latency_ms as u64 * 44100 / 1000) as u32).clamp(LATENCY_MIN_FRAMES, LATENCY_MAX_FRAMES)
    }
}

// ---------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct CredentialStore {
    #[serde(default, rename = "speaker")]
    pub speakers: Vec<StoredCredentials>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StoredCredentials {
    pub ip: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// "hap" (stored long-term keys) or "transient"
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ltpk: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ltsk: String,
    /// Device HAP identifier (hex of its bytes; for the Home View the `psi` UUID text).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device_id_hex: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_id_hex: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub apple_device_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_name: String,
}

impl StoredCredentials {
    pub fn from_hap(ip: &str, name: &str, c: &HapCredentials) -> Self {
        Self {
            ip: ip.to_string(),
            name: name.to_string(),
            kind: "hap".into(),
            ltpk: hex::encode(&c.ltpk),
            ltsk: hex::encode(&c.ltsk),
            device_id_hex: hex::encode(&c.atv_id),
            client_id_hex: hex::encode(&c.client_id),
            apple_device_id: c.apple_device_id.clone(),
            client_name: c.client_name.clone(),
        }
    }

    pub fn to_credentials(&self) -> Result<Credentials> {
        match self.kind.as_str() {
            "transient" => Ok(Credentials::Transient),
            "hap" => {
                let c = HapCredentials {
                    ltpk: hex::decode(&self.ltpk).context("ltpk")?,
                    ltsk: hex::decode(&self.ltsk).context("ltsk")?,
                    atv_id: hex::decode(&self.device_id_hex).context("device_id_hex")?,
                    client_id: hex::decode(&self.client_id_hex).context("client_id_hex")?,
                    apple_device_id: if self.apple_device_id.is_empty() {
                        SPIKE_APPLE_DEVICE_ID.into()
                    } else {
                        self.apple_device_id.clone()
                    },
                    client_name: if self.client_name.is_empty() {
                        DEFAULT_CLIENT_NAME.into()
                    } else {
                        self.client_name.clone()
                    },
                };
                if c.ltpk.len() != 32 || c.ltsk.len() != 32 || c.atv_id.is_empty() || c.client_id.is_empty() {
                    bail!("credentials for {} are incomplete", self.ip);
                }
                Ok(Credentials::Hap(c))
            }
            other => bail!("unknown credential kind {other:?} for {}", self.ip),
        }
    }
}

impl CredentialStore {
    pub fn load() -> Result<Self> {
        let path = credentials_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)?;
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = credentials_path()?;
        let header = "# stereo-pair HAP credentials. ltsk is a private key - keep this file private.\n\n";
        write_atomic(&path, &format!("{header}{}", toml::to_string_pretty(self)?))?;
        Ok(path)
    }

    pub fn find(&self, ip: &str) -> Option<&StoredCredentials> {
        self.speakers.iter().find(|s| s.ip == ip)
    }

    /// Credentials for a speaker; transient pairing if nothing is stored.
    pub fn credentials_for(&self, ip: &str) -> Result<Credentials> {
        match self.find(ip) {
            Some(s) => s.to_credentials(),
            None => Ok(Credentials::Transient),
        }
    }

    pub fn upsert(&mut self, entry: StoredCredentials) {
        self.speakers.retain(|s| s.ip != entry.ip);
        self.speakers.push(entry);
    }
}

/// Parse pyatv's `ltpk:ltsk:atv_id:client_id` (all hex) credential string.
pub fn parse_pyatv_credentials(text: &str, apple_device_id: &str, client_name: &str) -> Result<HapCredentials> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.len() != 4 {
        bail!("expected 4 colon-separated hex fields (ltpk:ltsk:atv_id:client_id), got {}", parts.len());
    }
    let c = HapCredentials {
        ltpk: hex::decode(parts[0]).context("ltpk")?,
        ltsk: hex::decode(parts[1]).context("ltsk")?,
        atv_id: hex::decode(parts[2]).context("atv_id")?,
        client_id: hex::decode(parts[3]).context("client_id")?,
        apple_device_id: apple_device_id.to_string(),
        client_name: client_name.to_string(),
    };
    if c.ltpk.len() != 32 || c.ltsk.len() != 32 {
        bail!("ltpk/ltsk must be 32 bytes each");
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pyatv_credentials_round_trip_through_store() {
        let text = format!(
            "{}:{}:{}:{}\n",
            "11".repeat(32),
            "22".repeat(32),
            hex::encode("A1B2C3D4-5E6F-4A7B-8C9D-0E1F2A3B4C5D"),
            hex::encode("C1D2E3F4-0000-1111-2222-333344445555")
        );
        let c = parse_pyatv_credentials(&text, SPIKE_APPLE_DEVICE_ID, DEFAULT_CLIENT_NAME).unwrap();
        assert_eq!(c.atv_id, b"A1B2C3D4-5E6F-4A7B-8C9D-0E1F2A3B4C5D");
        let mut store = CredentialStore::default();
        store.upsert(StoredCredentials::from_hap("192.168.1.51", "Living Room (2)", &c));
        let text = toml::to_string_pretty(&store).unwrap();
        let back: CredentialStore = toml::from_str(&text).unwrap();
        assert_eq!(back.credentials_for("192.168.1.51").unwrap(), Credentials::Hap(c));
        assert_eq!(back.credentials_for("192.168.1.50").unwrap(), Credentials::Transient);
        assert!(parse_pyatv_credentials("aa:bb", "x", "y").is_err());
    }

    #[test]
    fn default_config_round_trips() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&format!("{CONFIG_HEADER}\n{text}")).unwrap();
        assert_eq!(back, cfg);
        assert_eq!(cfg.latency_frames(), 66150);
        let low = Config { latency_ms: 300, ..Config::default() };
        assert_eq!(low.latency_frames(), 13230);
        // clamped to LATENCY_MIN_FRAMES (100 ms floor, lowered from pyatv's 11025)
        let tiny = Config { latency_ms: 10, ..Config::default() };
        assert_eq!(tiny.latency_frames(), 4410);
        let minimal: Config = toml::from_str("[[speaker]]\nname = \"x\"\nrole = \"left\"\n").unwrap();
        assert_eq!(minimal.latency_ms, 1500);
        assert!(minimal.speakers[0].enabled);
    }

    /// A milestone-1 config.toml (no auto_start / port fields) still loads, gets the
    /// new defaults, and saving it adds nothing that changes its meaning.
    #[test]
    fn milestone1_config_is_forward_compatible() {
        let m1 = format!(
            "{CONFIG_HEADER}\nlatency_ms = 100\nfollow_windows_volume = true\ntiming_port = 0\nclient_name = \"Stereo Pair\"\n\n\
             [[speaker]]\nname = \"Living Room\"\nip = \"192.168.1.50\"\nrole = \"left\"\ngain_db = 0.0\ndelay_ms = 0.0\nenabled = true\n\n\
             [[speaker]]\nname = \"Living Room (2)\"\nip = \"192.168.1.51\"\nrole = \"right\"\ngain_db = 0.0\ndelay_ms = 0.0\nenabled = true\n"
        );
        let cfg: Config = toml::from_str(&m1).unwrap();
        assert_eq!(cfg.latency_ms, 100);
        assert!(cfg.auto_start);
        assert_eq!(cfg.speakers[1].port, None);
        assert_eq!(cfg.speakers[1].key(), "192.168.1.51:7000");
        assert_eq!(cfg.speakers, Config::default().speakers);
        let saved = toml::to_string_pretty(&cfg).unwrap();
        assert!(!saved.contains("\nport"), "unset port is not written: {saved}");
        assert!(saved.contains("auto_start = true"));
    }

    #[test]
    fn save_to_and_load_from_round_trip_atomically() {
        let dir = crate::testvec::temp_dir("cfg");
        let path = dir.join("nested").join("config.toml");
        let (created, was_new) = Config::load_or_create_at(&path).unwrap();
        assert!(was_new);
        assert_eq!(created, Config::default());
        let mut cfg = created.clone();
        cfg.latency_ms = 250;
        cfg.auto_start = false;
        cfg.speakers[0].gain_db = -3.5;
        cfg.speakers[1].delay_ms = 12.3;
        cfg.speakers[1].port = Some(7001);
        cfg.save_to(&path).unwrap();
        assert!(!path.with_extension("toml.tmp").exists());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# stereo-pair configuration"));
        let (back, was_new) = Config::load_or_create_at(&path).unwrap();
        assert!(!was_new);
        assert_eq!(back, cfg);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
