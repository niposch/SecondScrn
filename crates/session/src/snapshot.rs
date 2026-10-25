//! Immutable state snapshots published to the native UIs.
//!
//! The UIs never poll internal state; they receive a fresh snapshot whenever
//! something changes (and about once per second while streaming).

use secondscrn_protocol::{ClientStats, CodecCapability, ResolutionOption, StreamConfig};
use serde::{Deserialize, Serialize};

use crate::platform::{DisplayInfo, DriverStatus, PipelineStats, UiScale};
use crate::settings::{ClientSettings, DevicePreferences, HostSettings, KnownHost};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Negotiating,
    Streaming,
    /// Network interrupted; waiting for the connection to resume.
    Interrupted,
}

/// Overall link quality, for the tray icon and status badges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LinkHealth {
    #[default]
    Unknown,
    Good,
    Fair,
    Poor,
}

impl LinkHealth {
    pub fn assess(rtt_ms: Option<f32>, loss: Option<f32>, fps: f32, target_fps: u32) -> Self {
        let (Some(rtt), Some(loss)) = (rtt_ms, loss) else {
            return LinkHealth::Unknown;
        };
        // Low fps alone is fine for a static desktop; only combine with loss.
        let starving = target_fps > 0 && fps > 0.0 && fps < target_fps as f32 * 0.5 && loss > 0.01;
        if loss > 0.05 || rtt > 150.0 || starving {
            LinkHealth::Poor
        } else if loss > 0.01 || rtt > 40.0 {
            LinkHealth::Fair
        } else {
            LinkHealth::Good
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostMetrics {
    pub pipeline: PipelineStats,
    pub bitrate_estimate_bps: Option<u64>,
    pub bytes_sent: u64,
    pub send_rate_bps: u64,
    pub rtt_ms: Option<f32>,
    pub loss: Option<f32>,
    pub nacks: u64,
    pub plis: u64,
    pub frames_dropped_network: u64,
    pub client: Option<ClientStats>,
    pub health: LinkHealth,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub device_name: String,
    pub device_model: String,
    pub fingerprint: String,
    pub fingerprint_short: String,
    pub remote_address: String,
    pub phase: SessionPhase,
    pub stream: Option<StreamConfig>,
    pub display: Option<DisplayInfo>,
    /// Resolutions offered for this tablet in its current orientation.
    pub resolutions: Vec<ResolutionOption>,
    /// Frame rates offered for this tablet.
    pub frame_rates: Vec<u32>,
    pub metrics: HostMetrics,
    pub connected_since: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingPrompt {
    pub id: u64,
    pub device_name: String,
    pub device_model: String,
    pub fingerprint_short: String,
    pub remote_address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedDevice {
    pub fingerprint: String,
    pub fingerprint_short: String,
    pub name: String,
    pub model: String,
    pub paired_at: u64,
    pub last_seen: Option<u64>,
    pub preferences: DevicePreferences,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostSnapshot {
    pub host_name: String,
    pub fingerprint_short: String,
    pub listening: bool,
    pub port: u16,
    pub addresses: Vec<String>,
    pub session: Option<SessionInfo>,
    pub pairing: Option<PairingPrompt>,
    pub paired_devices: Vec<PairedDevice>,
    pub driver: DriverStatus,
    pub encoders: Vec<CodecCapability>,
    pub last_error: Option<String>,
    pub settings: HostSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ClientPhase {
    Idle,
    Connecting,
    /// Show `pin` and ask the user to type it on the PC.
    Pairing {
        pin: String,
    },
    Negotiating,
    Streaming,
    /// Media interrupted, connection still up.
    Interrupted,
    Reconnecting {
        attempt: u32,
        retry_in_ms: u64,
    },
    Failed {
        message: String,
        needs_pairing: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSummary {
    pub name: String,
    pub fingerprint: String,
    pub fingerprint_short: String,
    pub address: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredHost {
    pub name: String,
    pub address: String,
    pub port: u16,
    pub fingerprint: String,
    pub paired: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientMetrics {
    pub render_fps: f32,
    pub decode_ms_avg: f32,
    pub frames_received: u64,
    pub frames_decoded: u64,
    pub frames_dropped: u64,
    pub receive_rate_bps: u64,
    pub rtt_ms: Option<f32>,
    pub loss: Option<f32>,
    pub health: LinkHealth,
    /// Something can't keep up with the current settings.
    pub warning: Option<PerformanceWarning>,
}

/// Why the stream isn't smooth, for a hint in the tablet's options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceWarning {
    /// The tablet's decoder falls behind at this resolution and frame rate.
    Decoder,
    /// Packets are lost or delayed: the network can't carry the stream.
    Network,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientSnapshot {
    pub phase: ClientPhase,
    pub host: Option<HostSummary>,
    pub stream: Option<StreamConfig>,
    pub metrics: ClientMetrics,
    pub discovered: Vec<DiscoveredHost>,
    pub known_hosts: Vec<KnownHost>,
    pub decoders: Vec<CodecCapability>,
    pub settings: ClientSettings,
    pub device_fingerprint_short: String,
    /// Resolutions the tablet can ask for, from its current panel size.
    pub resolutions: Vec<ResolutionOption>,
    pub frame_rates: Vec<u32>,
    /// Windows scale factor of the tablet's monitor, while streaming.
    pub ui_scale: Option<UiScale>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_assessment() {
        assert_eq!(LinkHealth::assess(None, Some(0.0), 60.0, 60), LinkHealth::Unknown);
        assert_eq!(LinkHealth::assess(Some(5.0), Some(0.0), 60.0, 60), LinkHealth::Good);
        assert_eq!(LinkHealth::assess(Some(60.0), Some(0.0), 60.0, 60), LinkHealth::Fair);
        assert_eq!(LinkHealth::assess(Some(5.0), Some(0.1), 60.0, 60), LinkHealth::Poor);
        // Static desktop: low fps without loss is healthy.
        assert_eq!(LinkHealth::assess(Some(5.0), Some(0.0), 2.0, 60), LinkHealth::Good);
        assert_eq!(LinkHealth::assess(Some(5.0), Some(0.02), 10.0, 60), LinkHealth::Poor);
    }
}
