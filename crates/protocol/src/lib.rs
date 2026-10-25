//! Wire protocol for the SecondScrn control channel.
//!
//! The control channel is a mutually authenticated TLS 1.3 stream carrying
//! length-prefixed JSON messages (see [`framing`]). It carries pairing,
//! session negotiation, WebRTC signaling (SDP), input events and low-rate
//! statistics. Video never travels over this channel; it uses WebRTC/SRTP.
//!
//! JSON keeps the protocol self-describing and easy to evolve: unknown fields
//! are ignored, and new enum variants are only sent to peers that announced a
//! sufficient [`PROTOCOL_VERSION`].

use serde::{Deserialize, Serialize};

pub mod framing;

/// Bumped on incompatible protocol changes.
pub const PROTOCOL_VERSION: u32 = 2;
/// Default TCP port of the host's control listener.
pub const DEFAULT_CONTROL_PORT: u16 = 47800;
/// mDNS / DNS-SD service type announced by hosts.
pub const MDNS_SERVICE_TYPE: &str = "_secondscrn._tcp.local.";
/// ALPN identifier used on the TLS control channel.
pub const ALPN: &[u8] = b"secondscrn/1";

/// Information a peer reveals about itself before authentication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// User-visible device name, e.g. "Galaxy Tab S9".
    pub name: String,
    /// Model/manufacturer string for diagnostics.
    pub model: String,
    /// Application version, e.g. "0.1.0".
    pub app_version: String,
}

/// Why the client opened the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    /// Already paired; start a streaming session.
    Connect,
    /// First-time pairing. The client shows a PIN that the host user enters.
    Pair,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    H264,
    H265,
    Av1,
}

impl Codec {
    pub const ALL: [Codec; 3] = [Codec::H264, Codec::H265, Codec::Av1];

    pub fn mime(self) -> &'static str {
        match self {
            Codec::H264 => "video/avc",
            Codec::H265 => "video/hevc",
            Codec::Av1 => "video/av01",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::H265 => "HEVC",
            Codec::Av1 => "AV1",
        }
    }
}

impl std::fmt::Display for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display_name())
    }
}

/// A decoder (client) or encoder (host) configuration that was *verified* to
/// work, not merely advertised by the platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecCapability {
    pub codec: Codec,
    /// Implementation name, e.g. "c2.qti.hevc.decoder" or "NVIDIA HEVC Encoder MFT".
    pub implementation: String,
    /// True when backed by dedicated hardware.
    pub hardware: bool,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
}

/// Colour description of the encoded stream. SDR only for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorInfo {
    pub primaries: ColorPrimaries,
    pub transfer: TransferCharacteristics,
    pub matrix: MatrixCoefficients,
    pub range: ColorRange,
}

impl ColorInfo {
    /// BT.709 primaries/matrix, sRGB-like transfer, limited range: what every
    /// hardware decoder handles and what Android composes correctly.
    pub const SDR_BT709_LIMITED: ColorInfo = ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristics::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorPrimaries {
    Bt709,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferCharacteristics {
    Bt709,
    Srgb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatrixCoefficients {
    Bt709,
    Bt601,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorRange {
    Limited,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    #[default]
    Landscape,
    Portrait,
}

/// What the client would like the virtual monitor to look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayRequest {
    /// Physical panel pixels in the requested orientation.
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    /// Panel density, used by the host to suggest a Windows scale factor.
    pub dpi: u32,
    pub orientation: Orientation,
}

/// Resolutions offered for a tablet, as percentages of its native panel.
/// Lower ones trade sharpness for bandwidth on weak networks; all keep the
/// panel's aspect ratio so the picture still fills the screen.
pub const RESOLUTION_SCALES: [u32; 5] = [100, 85, 75, 67, 50];

/// Frame rates offered (those up to the tablet's refresh rate).
pub const FRAME_RATES: [u32; 5] = [30, 60, 90, 120, 144];

impl DisplayRequest {
    /// This request at `percent` of its size (even dimensions, as encoders
    /// and NV12 need). Density scales along so Windows picks a similar UI size.
    pub fn scaled(&self, percent: u32) -> DisplayRequest {
        let p = percent.clamp(25, 100);
        let scale = |v: u32| ((v as u64 * p as u64 + 50) / 100) as u32 & !1;
        DisplayRequest {
            width: scale(self.width).max(2),
            height: scale(self.height).max(2),
            dpi: scale(self.dpi).max(72),
            ..*self
        }
    }

    /// The offered resolutions for this panel, largest first.
    pub fn resolution_options(&self) -> Vec<ResolutionOption> {
        RESOLUTION_SCALES
            .iter()
            .map(|&p| {
                let r = self.scaled(p);
                ResolutionOption { scale_percent: p, width: r.width, height: r.height }
            })
            .collect()
    }

    /// Frame rates the tablet can show: the offered ones up to its refresh
    /// rate, plus the refresh rate itself.
    pub fn frame_rate_options(&self) -> Vec<u32> {
        let mut rates: Vec<u32> = FRAME_RATES.into_iter().filter(|r| *r <= self.refresh_hz).collect();
        if self.refresh_hz >= 24 && !rates.contains(&self.refresh_hz) {
            rates.push(self.refresh_hz);
        }
        rates
    }

    /// The offered scale that produces `width`x`height`, or 0 for any other
    /// mode (e.g. one picked in the Windows display settings).
    pub fn scale_of(&self, width: u32, height: u32) -> u32 {
        self.resolution_options()
            .into_iter()
            .find(|o| o.width.abs_diff(width) <= 2 && o.height.abs_diff(height) <= 2)
            .map_or(0, |o| o.scale_percent)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionOption {
    pub scale_percent: u32,
    pub width: u32,
    pub height: u32,
}

/// User-selectable trade-off between sharpness and bandwidth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QualityPreset {
    /// Follow the bandwidth estimate within the configured limits.
    #[default]
    Automatic,
    /// High quality: about twice the bitrate ceiling and a high floor, at
    /// the cost of some latency (larger frames take longer to send).
    Sharp,
    /// Default for mixed workloads.
    Balanced,
    /// Weak Wi-Fi: lower bitrate ceiling and frame rate.
    LowBandwidth,
}

impl QualityPreset {
    pub const ALL: [QualityPreset; 4] =
        [QualityPreset::Automatic, QualityPreset::Sharp, QualityPreset::Balanced, QualityPreset::LowBandwidth];
}

/// Parameters of the video stream the host decided on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub max_fps: u32,
    pub color: ColorInfo,
    /// Initial target bitrate in bits per second.
    pub initial_bitrate_bps: u64,
    pub encoder: String,
    /// The stream size as a percentage of the tablet's panel (one of
    /// [`RESOLUTION_SCALES`]), or 0 for another mode chosen on the PC.
    #[serde(default = "full_scale")]
    pub scale_percent: u32,
}

fn full_scale() -> u32 {
    100
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerAction {
    Down,
    Move,
    Up,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerButton {
    Primary,
    Secondary,
    Middle,
}

/// One finger in a [`InputEvent::Touch`] frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TouchContact {
    /// Stable for the lifetime of the contact (Android pointer id).
    pub id: u32,
    pub action: PointerAction,
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PenAction {
    /// In range above the screen, not touching.
    Hover,
    Down,
    Move,
    Up,
    /// Left the detection range.
    Leave,
    Cancel,
}

/// Input from the tablet. Coordinates are normalised to the streamed display
/// (0.0..=1.0) so they stay valid across resolution changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputEvent {
    /// Mouse input (a mouse on the tablet, or touch in mouse mode).
    Pointer {
        /// Monotonic sequence number; the host drops out-of-order events.
        seq: u64,
        action: PointerAction,
        button: PointerButton,
        x: f32,
        y: f32,
    },
    Scroll {
        seq: u64,
        x: f32,
        y: f32,
        /// Scroll amount in wheel "notches"; positive is down / right.
        delta_x: f32,
        delta_y: f32,
    },
    /// A touch frame: every finger currently down (plus those lifting in
    /// this frame), injected as real touch so Windows handles gestures.
    /// Fingers missing from a frame are cancelled.
    Touch { seq: u64, contacts: Vec<TouchContact> },
    /// Stylus input with pressure and tilt.
    Pen {
        seq: u64,
        action: PenAction,
        x: f32,
        y: f32,
        /// 0.0..=1.0.
        pressure: f32,
        /// Degrees, -90..=90; positive tilts right / towards the user.
        tilt_x: f32,
        tilt_y: f32,
        /// Barrel button held (right-click in Windows).
        barrel: bool,
        eraser: bool,
    },
}

impl InputEvent {
    pub fn seq(&self) -> u64 {
        match self {
            InputEvent::Pointer { seq, .. }
            | InputEvent::Scroll { seq, .. }
            | InputEvent::Touch { seq, .. }
            | InputEvent::Pen { seq, .. } => *seq,
        }
    }
}

/// Periodic receiver-side statistics (about once per second).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct ClientStats {
    pub frames_received: u64,
    pub frames_decoded: u64,
    pub frames_rendered: u64,
    pub frames_dropped: u64,
    pub decode_time_ms_avg: f32,
    pub render_fps: f32,
    /// Frames that arrived later than the frame interval and were skipped.
    pub late_frames: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// Protocol versions are incompatible.
    IncompatibleVersion,
    /// The client is not (or no longer) paired. It must pair again.
    NotPaired,
    /// The host user declined the pairing request or entered a wrong PIN.
    PairingDeclined,
    /// Another pairing attempt is in progress or too many attempts were made.
    PairingBusy,
    /// Another tablet is already streaming.
    Busy,
    /// No usable codec in common.
    NoCommonCodec,
    /// The virtual display could not be created.
    DisplayUnavailable,
    /// A host-side error; details are in the host diagnostics.
    InternalError,
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RejectReason::IncompatibleVersion => "the host and tablet app versions are incompatible",
            RejectReason::NotPaired => "this tablet is not paired with the host",
            RejectReason::PairingDeclined => "pairing was declined or the code was wrong",
            RejectReason::PairingBusy => "the host is busy with another pairing request",
            RejectReason::Busy => "another tablet is already connected",
            RejectReason::NoCommonCodec => "no video codec is supported by both devices",
            RejectReason::DisplayUnavailable => "the virtual display could not be created on the host",
            RejectReason::InternalError => "the host encountered an internal error",
        })
    }
}

/// Messages sent from the tablet (client) to the PC (host).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        protocol_version: u32,
        device: DeviceInfo,
        intent: Intent,
    },
    /// SPAKE2 message (side B) for PIN pairing.
    PairStart {
        #[serde(with = "b64")]
        spake: Vec<u8>,
    },
    /// Key confirmation for the pairing.
    PairConfirm {
        #[serde(with = "b64")]
        mac: Vec<u8>,
    },
    SessionRequest {
        display: DisplayRequest,
        decoders: Vec<CodecCapability>,
        preset: QualityPreset,
        preferred_codec: Option<Codec>,
    },
    SdpAnswer {
        sdp: String,
    },
    Input {
        event: InputEvent,
    },
    RequestKeyframe,
    Stats {
        stats: ClientStats,
    },
    /// The tablet rotated or its usable area changed.
    DisplayChanged {
        display: DisplayRequest,
    },
    SetPreset {
        preset: QualityPreset,
    },
    /// Stream at this percentage of the panel resolution (see
    /// [`RESOLUTION_SCALES`]).
    SetResolution {
        scale_percent: u32,
    },
    /// Stream at most this many frames per second.
    SetFrameRate {
        fps: u32,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    Goodbye,
}

/// Messages sent from the PC (host) to the tablet (client).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    Hello {
        protocol_version: u32,
        host: DeviceInfo,
    },
    /// The host user is being asked to enter the PIN shown on the tablet.
    PairPending,
    /// SPAKE2 message (side A) plus the host's key confirmation.
    PairChallenge {
        #[serde(with = "b64")]
        spake: Vec<u8>,
        #[serde(with = "b64")]
        mac: Vec<u8>,
    },
    PairComplete,
    Rejected {
        reason: RejectReason,
    },
    SessionAccepted {
        stream: StreamConfig,
    },
    SdpOffer {
        sdp: String,
    },
    /// The stream parameters changed (e.g. after rotation). A new SDP
    /// exchange may follow.
    StreamChanged {
        stream: StreamConfig,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    /// The host ends the session. With `reconnect` the tablet should retry
    /// (host app restarting/updating); otherwise the user ended it.
    Goodbye {
        #[serde(default)]
        reconnect: bool,
    },
}

mod b64 {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = <&str>::deserialize(d)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_client(msg: ClientMessage) {
        let json = serde_json::to_string(&msg).unwrap();
        let back: ClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back, "{json}");
    }

    fn roundtrip_host(msg: HostMessage) {
        let json = serde_json::to_string(&msg).unwrap();
        let back: HostMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back, "{json}");
    }

    #[test]
    fn client_messages_roundtrip() {
        roundtrip_client(ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            device: DeviceInfo { name: "Tab".into(), model: "X".into(), app_version: "0.1.0".into() },
            intent: Intent::Pair,
        });
        roundtrip_client(ClientMessage::PairStart { spake: vec![0, 1, 2, 255] });
        roundtrip_client(ClientMessage::SessionRequest {
            display: DisplayRequest {
                width: 2560,
                height: 1600,
                refresh_hz: 60,
                dpi: 274,
                orientation: Orientation::Landscape,
            },
            decoders: vec![CodecCapability {
                codec: Codec::H265,
                implementation: "c2.qti.hevc.decoder".into(),
                hardware: true,
                max_width: 4096,
                max_height: 2160,
                max_fps: 60,
            }],
            preset: QualityPreset::Sharp,
            preferred_codec: None,
        });
        roundtrip_client(ClientMessage::Input {
            event: InputEvent::Pointer {
                seq: 7,
                action: PointerAction::Down,
                button: PointerButton::Primary,
                x: 0.25,
                y: 0.5,
            },
        });
        roundtrip_client(ClientMessage::Input {
            event: InputEvent::Touch {
                seq: 8,
                contacts: vec![
                    TouchContact { id: 0, action: PointerAction::Move, x: 0.1, y: 0.2 },
                    TouchContact { id: 1, action: PointerAction::Down, x: 0.3, y: 0.4 },
                ],
            },
        });
        roundtrip_client(ClientMessage::Input {
            event: InputEvent::Pen {
                seq: 9,
                action: PenAction::Hover,
                x: 0.5,
                y: 0.5,
                pressure: 0.0,
                tilt_x: -12.0,
                tilt_y: 30.0,
                barrel: false,
                eraser: true,
            },
        });
        roundtrip_client(ClientMessage::SetResolution { scale_percent: 50 });
        roundtrip_client(ClientMessage::SetFrameRate { fps: 90 });
        roundtrip_client(ClientMessage::Stats { stats: ClientStats::default() });
        roundtrip_client(ClientMessage::Goodbye);
    }

    #[test]
    fn host_messages_roundtrip() {
        roundtrip_host(HostMessage::PairChallenge { spake: vec![9; 33], mac: vec![1; 32] });
        roundtrip_host(HostMessage::Rejected { reason: RejectReason::NotPaired });
        roundtrip_host(HostMessage::SessionAccepted {
            stream: StreamConfig {
                codec: Codec::Av1,
                width: 1920,
                height: 1080,
                max_fps: 60,
                color: ColorInfo::SDR_BT709_LIMITED,
                initial_bitrate_bps: 20_000_000,
                encoder: "test".into(),
                scale_percent: 75,
            },
        });
    }

    #[test]
    fn wire_format_is_stable() {
        // Guards against accidental renames that would break older peers.
        let json = serde_json::to_string(&ClientMessage::Ping { nonce: 3 }).unwrap();
        assert_eq!(json, r#"{"type":"ping","nonce":3}"#);
        let json = serde_json::to_string(&HostMessage::Rejected { reason: RejectReason::Busy }).unwrap();
        assert_eq!(json, r#"{"type":"rejected","reason":"busy"}"#);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let msg: ClientMessage = serde_json::from_str(r#"{"type":"ping","nonce":1,"future_field":true}"#).unwrap();
        assert_eq!(msg, ClientMessage::Ping { nonce: 1 });
    }

    #[test]
    fn resolution_options_keep_aspect_and_even_sizes() {
        let panel = DisplayRequest {
            width: 3200,
            height: 2136,
            refresh_hz: 120,
            dpi: 345,
            orientation: Orientation::Landscape,
        };
        let opts = panel.resolution_options();
        assert_eq!(opts[0], ResolutionOption { scale_percent: 100, width: 3200, height: 2136 });
        assert_eq!(opts.last().unwrap(), &ResolutionOption { scale_percent: 50, width: 1600, height: 1068 });
        for o in &opts {
            assert!(o.width % 2 == 0 && o.height % 2 == 0, "{o:?}");
            let aspect = o.width as f32 / o.height as f32;
            assert!((aspect - 3200.0 / 2136.0).abs() < 0.01, "{o:?}");
        }
        assert_eq!(panel.scale_of(2400, 1602), 75);
        assert_eq!(panel.scale_of(1920, 1080), 0);
        assert_eq!(panel.scaled(50).refresh_hz, 120);
        assert_eq!(panel.frame_rate_options(), vec![30, 60, 90, 120]);
        let odd = DisplayRequest { refresh_hz: 165, ..panel };
        assert_eq!(odd.frame_rate_options(), vec![30, 60, 90, 120, 144, 165]);
    }

    #[test]
    fn stream_config_without_scale_defaults_to_native() {
        let json = r#"{"codec":"h264","width":1,"height":1,"max_fps":60,"color":{"primaries":"bt709","transfer":"bt709","matrix":"bt709","range":"limited"},"initial_bitrate_bps":1,"encoder":"x"}"#;
        let c: StreamConfig = serde_json::from_str(json).unwrap();
        assert_eq!(c.scale_percent, 100);
    }

    #[test]
    fn invalid_base64_is_rejected() {
        let res: Result<ClientMessage, _> = serde_json::from_str(r#"{"type":"pair_start","spake":"!!"}"#);
        assert!(res.is_err());
    }
}
