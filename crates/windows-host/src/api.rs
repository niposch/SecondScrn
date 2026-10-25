//! The API exposed to the C#/WinUI application through UniFFI
//! (bindings generated with uniffi-bindgen-cs).
//!
//! The UI sends commands and receives [`HostState`] snapshots through
//! [`HostListener`] (at most ~4 per second). No video data crosses this
//! boundary; the streaming backend runs independently of window visibility.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use secondscrn_diagnostics::LogBuffer;
use secondscrn_protocol::{Codec, QualityPreset};
use secondscrn_session::platform::DriverStatus;
use secondscrn_session::settings::{DevicePreferences, HostSettings, Theme};
use secondscrn_session::snapshot::{HostSnapshot, LinkHealth, SessionPhase};
use secondscrn_session::{HostConfig, HostEngine};
use secondscrn_transport::InterfacePolicy;

use crate::platform::{DisplaySource, WindowsHostPlatform};

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum HostError {
    #[error("{detail}")]
    Failed { detail: String },
}

fn fail(e: impl std::fmt::Display) -> HostError {
    HostError::Failed { detail: e.to_string() }
}

#[uniffi::export(callback_interface)]
pub trait HostListener: Send + Sync {
    fn on_state_changed(&self, state: HostState);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Preset {
    Automatic,
    Sharp,
    Balanced,
    LowBandwidth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum AppTheme {
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Health {
    Unknown,
    Good,
    Fair,
    Poor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Phase {
    Negotiating,
    Streaming,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DriverState {
    Ready,
    NotInstalled,
    Unavailable,
    NotApplicable,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct DriverView {
    pub state: DriverState,
    pub detail: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct EncoderView {
    pub codec: VideoCodec,
    pub name: String,
    pub max_width: u32,
    pub max_height: u32,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct MetricsView {
    pub capture_fps: f32,
    pub encode_fps: f32,
    pub encode_ms: f32,
    pub target_mbps: f32,
    pub target_fps: u32,
    pub send_mbps: f32,
    pub estimate_mbps: Option<f32>,
    pub rtt_ms: Option<f32>,
    pub loss_percent: Option<f32>,
    pub client_fps: Option<f32>,
    pub client_decode_ms: Option<f32>,
    pub frames_dropped: u64,
    pub keyframes: u64,
    pub nacks: u64,
    pub plis: u64,
    pub health: Health,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionView {
    pub device_name: String,
    pub device_model: String,
    pub fingerprint: String,
    pub fingerprint_short: String,
    pub remote_address: String,
    pub phase: Phase,
    pub codec: Option<VideoCodec>,
    pub encoder: String,
    pub width: u32,
    pub height: u32,
    pub max_fps: u32,
    pub display_id: String,
    pub virtual_display: bool,
    pub connected_since: u64,
    pub metrics: MetricsView,
    /// Current size as a percentage of the tablet panel; 0 = a mode chosen
    /// in the Windows display settings.
    pub scale_percent: u32,
    pub resolutions: Vec<ResolutionView>,
    /// Frame rates offered for this tablet; `max_fps` is the current one.
    pub frame_rates: Vec<u32>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ResolutionView {
    pub scale_percent: u32,
    pub width: u32,
    pub height: u32,
    /// e.g. "2400 × 1602 (75 %)".
    pub label: String,
}

pub fn resolution_label(o: &secondscrn_protocol::ResolutionOption) -> String {
    if o.scale_percent == 100 {
        format!("{} × {} (native)", o.width, o.height)
    } else {
        format!("{} × {} ({} %)", o.width, o.height, o.scale_percent)
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct PairingView {
    pub id: u64,
    pub device_name: String,
    pub device_model: String,
    pub fingerprint_short: String,
    pub remote_address: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct DeviceView {
    pub fingerprint: String,
    pub fingerprint_short: String,
    pub name: String,
    pub model: String,
    pub paired_at: u64,
    pub last_seen: Option<u64>,
    pub preset: Option<Preset>,
    pub auto_reconnect: bool,
    pub connected: bool,
    /// Remembered resolution, percent of the panel.
    pub resolution_scale: u32,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SettingsView {
    pub host_name: Option<String>,
    pub control_port: u16,
    pub discoverable: bool,
    pub preset: Preset,
    pub preferred_codec: Option<VideoCodec>,
    pub max_fps: u32,
    pub max_bitrate_mbps: u32,
    pub display_linger_secs: u32,
    pub start_with_windows: bool,
    pub start_minimized: bool,
    pub close_to_tray: bool,
    pub theme: AppTheme,
    pub check_for_updates: bool,
    pub verbose_logging: bool,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct HostState {
    pub host_name: String,
    pub fingerprint_short: String,
    pub listening: bool,
    pub port: u16,
    pub addresses: Vec<String>,
    pub session: Option<SessionView>,
    pub pairing: Option<PairingView>,
    pub devices: Vec<DeviceView>,
    pub driver: DriverView,
    pub encoders: Vec<EncoderView>,
    pub last_error: Option<String>,
    pub settings: SettingsView,
    pub data_dir: String,
    pub version: String,
}

fn preset_from(p: QualityPreset) -> Preset {
    match p {
        QualityPreset::Automatic => Preset::Automatic,
        QualityPreset::Sharp => Preset::Sharp,
        QualityPreset::Balanced => Preset::Balanced,
        QualityPreset::LowBandwidth => Preset::LowBandwidth,
    }
}

fn preset_to(p: Preset) -> QualityPreset {
    match p {
        Preset::Automatic => QualityPreset::Automatic,
        Preset::Sharp => QualityPreset::Sharp,
        Preset::Balanced => QualityPreset::Balanced,
        Preset::LowBandwidth => QualityPreset::LowBandwidth,
    }
}

fn codec_from(c: Codec) -> VideoCodec {
    match c {
        Codec::H264 => VideoCodec::H264,
        Codec::H265 => VideoCodec::H265,
        Codec::Av1 => VideoCodec::Av1,
    }
}

fn codec_to(c: VideoCodec) -> Codec {
    match c {
        VideoCodec::H264 => Codec::H264,
        VideoCodec::H265 => Codec::H265,
        VideoCodec::Av1 => Codec::Av1,
    }
}

fn settings_from(s: &HostSettings) -> SettingsView {
    SettingsView {
        host_name: s.host_name.clone(),
        control_port: s.control_port,
        discoverable: s.discoverable,
        preset: preset_from(s.preset),
        preferred_codec: s.preferred_codec.map(codec_from),
        max_fps: s.max_fps,
        max_bitrate_mbps: s.max_bitrate_mbps,
        display_linger_secs: s.display_linger_secs,
        start_with_windows: s.start_with_windows,
        start_minimized: s.start_minimized,
        close_to_tray: s.close_to_tray,
        theme: match s.theme {
            Theme::System => AppTheme::System,
            Theme::Light => AppTheme::Light,
            Theme::Dark => AppTheme::Dark,
        },
        check_for_updates: s.check_for_updates,
        verbose_logging: s.verbose_logging,
    }
}

fn settings_to(v: SettingsView, mut base: HostSettings) -> HostSettings {
    base.host_name = v.host_name.filter(|n| !n.trim().is_empty());
    base.control_port = v.control_port;
    base.discoverable = v.discoverable;
    base.preset = preset_to(v.preset);
    base.preferred_codec = v.preferred_codec.map(codec_to);
    base.max_fps = v.max_fps;
    base.max_bitrate_mbps = v.max_bitrate_mbps;
    base.display_linger_secs = v.display_linger_secs;
    base.start_with_windows = v.start_with_windows;
    base.start_minimized = v.start_minimized;
    base.close_to_tray = v.close_to_tray;
    base.theme = match v.theme {
        AppTheme::System => Theme::System,
        AppTheme::Light => Theme::Light,
        AppTheme::Dark => Theme::Dark,
    };
    base.check_for_updates = v.check_for_updates;
    base.verbose_logging = v.verbose_logging;
    base
}

fn health(h: LinkHealth) -> Health {
    match h {
        LinkHealth::Unknown => Health::Unknown,
        LinkHealth::Good => Health::Good,
        LinkHealth::Fair => Health::Fair,
        LinkHealth::Poor => Health::Poor,
    }
}

pub fn to_view(s: &HostSnapshot, data_dir: &str) -> HostState {
    let session = s.session.as_ref().map(|x| {
        let m = &x.metrics;
        let stream = x.stream.as_ref();
        let display = x.display.as_ref();
        SessionView {
            device_name: x.device_name.clone(),
            device_model: x.device_model.clone(),
            fingerprint: x.fingerprint.clone(),
            fingerprint_short: x.fingerprint_short.clone(),
            remote_address: x.remote_address.clone(),
            phase: match x.phase {
                SessionPhase::Negotiating => Phase::Negotiating,
                SessionPhase::Streaming => Phase::Streaming,
                SessionPhase::Interrupted => Phase::Interrupted,
            },
            codec: stream.map(|c| codec_from(c.codec)),
            encoder: stream.map(|c| c.encoder.clone()).unwrap_or_default(),
            width: display.map(|d| d.width).or(stream.map(|c| c.width)).unwrap_or(0),
            height: display.map(|d| d.height).or(stream.map(|c| c.height)).unwrap_or(0),
            max_fps: stream.map(|c| c.max_fps).unwrap_or(0),
            display_id: display.map(|d| d.id.clone()).unwrap_or_default(),
            virtual_display: display.is_some_and(|d| d.is_virtual),
            connected_since: x.connected_since,
            scale_percent: stream.map(|c| c.scale_percent).unwrap_or(100),
            resolutions: x
                .resolutions
                .iter()
                .map(|o| ResolutionView {
                    scale_percent: o.scale_percent,
                    width: o.width,
                    height: o.height,
                    label: resolution_label(o),
                })
                .collect(),
            frame_rates: x.frame_rates.clone(),
            metrics: MetricsView {
                capture_fps: m.pipeline.capture_fps,
                encode_fps: m.pipeline.encode_fps,
                encode_ms: m.pipeline.encode_ms_avg,
                target_mbps: m.pipeline.bitrate_target_bps as f32 / 1e6,
                target_fps: m.pipeline.fps_target,
                send_mbps: m.send_rate_bps as f32 / 1e6,
                estimate_mbps: m.bitrate_estimate_bps.map(|b| b as f32 / 1e6),
                rtt_ms: m.rtt_ms,
                loss_percent: m.loss.map(|l| l * 100.0),
                client_fps: m.client.map(|c| c.render_fps),
                client_decode_ms: m.client.map(|c| c.decode_time_ms_avg),
                frames_dropped: m.frames_dropped_network
                    + m.pipeline.frames_skipped
                    + m.client.map(|c| c.frames_dropped + c.late_frames).unwrap_or(0),
                keyframes: m.pipeline.keyframes,
                nacks: m.nacks,
                plis: m.plis,
                health: health(m.health),
            },
        }
    });
    let connected_fp = s.session.as_ref().map(|x| x.fingerprint.clone());
    HostState {
        host_name: s.host_name.clone(),
        fingerprint_short: s.fingerprint_short.clone(),
        listening: s.listening,
        port: s.port,
        addresses: s.addresses.clone(),
        session,
        pairing: s.pairing.as_ref().map(|p| PairingView {
            id: p.id,
            device_name: p.device_name.clone(),
            device_model: p.device_model.clone(),
            fingerprint_short: p.fingerprint_short.clone(),
            remote_address: p.remote_address.clone(),
        }),
        devices: s
            .paired_devices
            .iter()
            .map(|d| DeviceView {
                fingerprint: d.fingerprint.clone(),
                fingerprint_short: d.fingerprint_short.clone(),
                name: d.name.clone(),
                model: d.model.clone(),
                paired_at: d.paired_at,
                last_seen: d.last_seen,
                preset: d.preferences.preset.map(preset_from),
                auto_reconnect: d.preferences.allow_auto_reconnect,
                connected: connected_fp.as_deref() == Some(d.fingerprint.as_str()),
                resolution_scale: d.preferences.scale_percent(),
            })
            .collect(),
        driver: match &s.driver {
            DriverStatus::Ready { detail } => DriverView { state: DriverState::Ready, detail: detail.clone() },
            DriverStatus::NotInstalled => DriverView {
                state: DriverState::NotInstalled,
                detail: "The Virtual Display Driver is not installed.".into(),
            },
            DriverStatus::Unavailable { reason } => {
                DriverView { state: DriverState::Unavailable, detail: reason.clone() }
            }
            DriverStatus::NotApplicable => DriverView {
                state: DriverState::NotApplicable,
                detail: "Mirroring an existing monitor (no virtual display).".into(),
            },
        },
        encoders: s
            .encoders
            .iter()
            .map(|e| EncoderView {
                codec: codec_from(e.codec),
                name: e.implementation.clone(),
                max_width: e.max_width,
                max_height: e.max_height,
            })
            .collect(),
        last_error: s.last_error.clone(),
        settings: settings_from(&s.settings),
        data_dir: data_dir.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

#[derive(uniffi::Object)]
pub struct SecondScrnHost {
    engine: HostEngine,
    platform: Arc<WindowsHostPlatform>,
    logs: LogBuffer,
    data_dir: String,
    stop: Arc<std::sync::atomic::AtomicBool>,
    watcher: Mutex<Option<std::thread::JoinHandle<()>>>,
}

#[uniffi::export]
impl SecondScrnHost {
    /// Starts the host service. `data_dir` empty = `%LOCALAPPDATA%\SecondScrn`.
    #[uniffi::constructor]
    pub fn start(data_dir: String, listener: Box<dyn HostListener>) -> Result<Arc<Self>, HostError> {
        crate::init_dpi_awareness();
        let data_dir =
            if data_dir.trim().is_empty() { crate::default_data_dir() } else { std::path::PathBuf::from(data_dir) };
        std::fs::create_dir_all(&data_dir).map_err(fail)?;
        let verbose =
            secondscrn_session::settings::load::<HostSettings>(&data_dir.join("settings.json")).verbose_logging;
        let logs = crate::logging::init(&data_dir.join("logs"), verbose);
        tracing::info!(version = env!("CARGO_PKG_VERSION"), "SecondScrn host starting");

        let platform = Arc::new(WindowsHostPlatform::new(DisplaySource::Virtual));
        let engine = HostEngine::start(
            HostConfig {
                data_dir: data_dir.clone(),
                default_host_name: crate::computer_name(),
                listen_addr: [0, 0, 0, 0].into(),
                interfaces: InterfacePolicy::default(),
                enable_mdns: true,
                protector: Arc::new(crate::dpapi::Dpapi),
                logs: Some(logs.clone()),
                app_version: env!("CARGO_PKG_VERSION").into(),
            },
            platform.clone(),
        )
        .map_err(fail)?;

        let dir_string = data_dir.display().to_string();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (mut rx, stop2, dir2) = (engine.subscribe(), stop.clone(), dir_string.clone());
        let watcher = std::thread::Builder::new()
            .name("secondscrn-ui-state".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().expect("runtime");
                rt.block_on(async move {
                    listener.on_state_changed(to_view(&rx.borrow(), &dir2));
                    while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                        match tokio::time::timeout(Duration::from_millis(500), rx.changed()).await {
                            Ok(Ok(())) => {
                                let s = to_view(&rx.borrow_and_update(), &dir2);
                                listener.on_state_changed(s);
                                tokio::time::sleep(Duration::from_millis(250)).await;
                            }
                            Ok(Err(_)) => return,
                            Err(_) => {}
                        }
                    }
                });
            })
            .map_err(fail)?;
        Ok(Arc::new(SecondScrnHost {
            engine,
            platform,
            logs,
            data_dir: dir_string,
            stop,
            watcher: Mutex::new(Some(watcher)),
        }))
    }

    pub fn state(&self) -> HostState {
        to_view(&self.engine.snapshot(), &self.data_dir)
    }

    pub fn submit_pairing_code(&self, id: u64, code: String) -> Result<(), HostError> {
        self.engine.submit_pairing_pin(id, &code).map_err(fail)
    }

    pub fn reject_pairing(&self, id: u64) {
        let _ = self.engine.reject_pairing(id);
    }

    pub fn disconnect(&self) {
        self.engine.disconnect();
    }

    pub fn set_preset(&self, preset: Preset) {
        self.engine.set_preset(preset_to(preset));
    }

    pub fn revoke_device(&self, fingerprint: String) -> Result<(), HostError> {
        self.engine.revoke_device(&fingerprint).map(|_| ()).map_err(fail)
    }

    pub fn set_device_preferences(
        &self,
        fingerprint: String,
        preset: Option<Preset>,
        auto_reconnect: bool,
    ) -> Result<(), HostError> {
        let current = self.engine.snapshot().settings.device(&fingerprint);
        self.engine
            .set_device_preferences(
                &fingerprint,
                DevicePreferences { preset: preset.map(preset_to), allow_auto_reconnect: auto_reconnect, ..current },
            )
            .map_err(fail)
    }

    /// Streams the connected tablet at this percentage of its panel
    /// resolution (one of the `resolutions` of the session).
    pub fn set_resolution(&self, scale_percent: u32) -> Result<(), HostError> {
        self.engine.set_resolution(scale_percent).map_err(fail)
    }

    /// Streams the connected tablet at up to `fps` frames per second (one
    /// of the `frame_rates` of the session).
    pub fn set_frame_rate(&self, fps: u32) -> Result<(), HostError> {
        self.engine.set_frame_rate(fps).map_err(fail)
    }

    pub fn update_settings(&self, settings: SettingsView) {
        let base = self.engine.snapshot().settings;
        self.engine.update_settings(settings_to(settings, base));
    }

    /// Re-detects the driver and encoders (e.g. after installing the driver).
    pub fn refresh(&self) {
        let p = self.platform.clone();
        std::thread::spawn(move || p.reprobe());
        self.engine.refresh_platform();
    }

    pub fn export_diagnostics(&self) -> String {
        self.engine.export_diagnostics()
    }

    pub fn recent_logs(&self, max: u32) -> Vec<String> {
        self.logs.recent(max as usize)
    }

    pub fn shutdown(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.engine.shutdown();
        self.platform.release_displays();
        if let Some(t) = self.watcher.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = t.join();
        }
        tracing::info!("SecondScrn host stopped");
    }
}
