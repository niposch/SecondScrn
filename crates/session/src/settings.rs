//! Persisted user settings. Rust is the single source of truth; the native
//! UIs read and write these through the binding API.

use std::collections::BTreeMap;
use std::path::Path;

use secondscrn_protocol::{Codec, DEFAULT_CONTROL_PORT, QualityPreset};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::SessionError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

/// Per-tablet preferences on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DevicePreferences {
    /// Overrides the global preset for this tablet.
    pub preset: Option<QualityPreset>,
    /// Allow this tablet to reconnect automatically without UI interaction.
    pub allow_auto_reconnect: bool,
    /// Stream resolution as a percentage of the panel; `None` = native.
    pub resolution_scale: Option<u32>,
    /// Frame rate for this tablet; `None` = the global `max_fps`.
    pub max_fps: Option<u32>,
}

impl DevicePreferences {
    /// The effective scale, snapped to an offered one.
    pub fn scale_percent(&self) -> u32 {
        snap_scale(self.resolution_scale.unwrap_or(100))
    }
}

/// The offered resolution scale closest to `percent`.
pub fn snap_scale(percent: u32) -> u32 {
    secondscrn_protocol::RESOLUTION_SCALES.into_iter().min_by_key(|s| s.abs_diff(percent)).unwrap_or(100)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostSettings {
    pub version: u32,
    /// Name shown to tablets. `None` = the computer name.
    pub host_name: Option<String>,
    pub control_port: u16,
    /// Announce the host via mDNS. Manual connection works regardless.
    pub discoverable: bool,
    pub preset: QualityPreset,
    /// Force a codec instead of automatic negotiation.
    pub preferred_codec: Option<Codec>,
    pub max_fps: u32,
    /// Upper bitrate bound in Mbit/s; 0 = automatic.
    pub max_bitrate_mbps: u32,
    /// Keep the virtual monitor attached this long after a tablet drops, so
    /// windows don't move during short Wi-Fi interruptions.
    pub display_linger_secs: u32,
    pub devices: BTreeMap<String, DevicePreferences>,
    // Application behaviour (applied by the Windows UI).
    pub start_with_windows: bool,
    pub start_minimized: bool,
    pub close_to_tray: bool,
    pub theme: Theme,
    pub check_for_updates: bool,
    pub verbose_logging: bool,
}

impl Default for HostSettings {
    fn default() -> Self {
        HostSettings {
            version: 1,
            host_name: None,
            control_port: DEFAULT_CONTROL_PORT,
            discoverable: true,
            preset: QualityPreset::Automatic,
            preferred_codec: None,
            max_fps: 60,
            max_bitrate_mbps: 0,
            display_linger_secs: 30,
            devices: BTreeMap::new(),
            start_with_windows: false,
            start_minimized: true,
            close_to_tray: true,
            theme: Theme::System,
            check_for_updates: true,
            verbose_logging: false,
        }
    }
}

impl HostSettings {
    pub fn device(&self, fingerprint_hex: &str) -> DevicePreferences {
        self.devices.get(fingerprint_hex).cloned().unwrap_or(DevicePreferences {
            preset: None,
            allow_auto_reconnect: true,
            resolution_scale: None,
            max_fps: None,
        })
    }

    /// Clamps values a hand-edited file could make unusable.
    pub fn sanitized(mut self) -> Self {
        self.max_fps = self.max_fps.clamp(10, 240);
        self.display_linger_secs = self.display_linger_secs.min(600);
        if self.control_port == 0 {
            self.control_port = DEFAULT_CONTROL_PORT;
        }
        self
    }
}

/// A host the tablet has paired with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownHost {
    /// Hex certificate fingerprint (pinned).
    pub fingerprint: String,
    pub name: String,
    pub last_address: String,
    pub port: u16,
    pub auto_reconnect: bool,
    /// Unix seconds.
    pub last_connected: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientSettings {
    pub version: u32,
    /// Name shown on the PC; `None` = device model.
    pub device_name: Option<String>,
    pub preset: QualityPreset,
    pub preferred_codec: Option<Codec>,
    pub known_hosts: Vec<KnownHost>,
    pub keep_screen_on: bool,
    /// Send touch and pen input to the PC.
    pub touch_input: bool,
    /// Translate touch into mouse clicks and scrolling instead of sending
    /// real touch (for apps that handle touch badly).
    pub touch_as_mouse: bool,
    /// Keep the current screen orientation while streaming.
    pub rotation_locked: bool,
}

impl Default for ClientSettings {
    fn default() -> Self {
        ClientSettings {
            version: 1,
            device_name: None,
            preset: QualityPreset::Automatic,
            preferred_codec: None,
            known_hosts: Vec::new(),
            keep_screen_on: true,
            touch_input: true,
            touch_as_mouse: false,
            rotation_locked: false,
        }
    }
}

impl ClientSettings {
    pub fn known_host(&self, fingerprint_hex: &str) -> Option<&KnownHost> {
        self.known_hosts.iter().find(|h| h.fingerprint == fingerprint_hex)
    }

    pub fn upsert_host(&mut self, host: KnownHost) {
        match self.known_hosts.iter_mut().find(|h| h.fingerprint == host.fingerprint) {
            Some(existing) => *existing = host,
            None => self.known_hosts.push(host),
        }
    }

    pub fn remove_host(&mut self, fingerprint_hex: &str) -> bool {
        let before = self.known_hosts.len();
        self.known_hosts.retain(|h| h.fingerprint != fingerprint_hex);
        before != self.known_hosts.len()
    }
}

/// Loads settings; a missing file yields defaults. A corrupt file is moved
/// aside (`.corrupt`) and defaults are used, so the app always starts.
pub fn load<T: DeserializeOwned + Default>(path: &Path) -> T {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "settings file is corrupt, using defaults");
                let _ = std::fs::rename(path, path.with_extension("corrupt"));
                T::default()
            }
        },
        Err(_) => T::default(),
    }
}

pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<(), SessionError> {
    let bytes = serde_json::to_vec_pretty(value).expect("settings serialise");
    secondscrn_security::write_atomic(path, &bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_settings_roundtrip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.json");
        let loaded: HostSettings = load(&path);
        assert_eq!(loaded, HostSettings::default());

        let mut s = HostSettings { preset: QualityPreset::Sharp, max_fps: 30, ..Default::default() };
        s.devices.insert(
            "ab".into(),
            DevicePreferences {
                preset: Some(QualityPreset::LowBandwidth),
                allow_auto_reconnect: false,
                resolution_scale: Some(75),
                max_fps: Some(120),
            },
        );
        save(&path, &s).unwrap();
        let back: HostSettings = load(&path);
        assert_eq!(back, s);
        assert_eq!(back.device("ab").preset, Some(QualityPreset::LowBandwidth));
        assert!(back.device("unknown").allow_auto_reconnect);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let s: HostSettings = serde_json::from_str(r#"{"max_fps": 30}"#).unwrap();
        assert_eq!(s.max_fps, 30);
        assert_eq!(s.control_port, DEFAULT_CONTROL_PORT);
        assert!(s.close_to_tray);
    }

    #[test]
    fn corrupt_settings_fall_back_and_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.json");
        std::fs::write(&path, b"{not json").unwrap();
        let s: HostSettings = load(&path);
        assert_eq!(s, HostSettings::default());
        assert!(path.with_extension("corrupt").exists());
    }

    #[test]
    fn sanitize_clamps() {
        let s = HostSettings { max_fps: 1000, control_port: 0, ..Default::default() }.sanitized();
        assert_eq!(s.max_fps, 240);
        assert_eq!(s.control_port, DEFAULT_CONTROL_PORT);
    }

    #[test]
    fn known_hosts_upsert_and_remove() {
        let mut c = ClientSettings::default();
        let h = KnownHost {
            fingerprint: "aa".into(),
            name: "PC".into(),
            last_address: "10.0.0.2".into(),
            port: 47800,
            auto_reconnect: true,
            last_connected: None,
        };
        c.upsert_host(h.clone());
        c.upsert_host(KnownHost { last_address: "10.0.0.3".into(), ..h });
        assert_eq!(c.known_hosts.len(), 1);
        assert_eq!(c.known_host("aa").unwrap().last_address, "10.0.0.3");
        assert!(c.remove_host("aa"));
        assert!(!c.remove_host("aa"));
    }
}
