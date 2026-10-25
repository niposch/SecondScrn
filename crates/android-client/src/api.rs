//! The API exposed to Kotlin through UniFFI.
//!
//! Kotlin sends commands and receives state snapshots (at most ~10 per
//! second). Video never crosses this boundary: it is decoded in Rust and
//! rendered straight into the `Surface` handed over via JNI.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use secondscrn_protocol::{
    DisplayRequest, Orientation, PenAction, PointerAction, PointerButton, QualityPreset, TouchContact,
};
use secondscrn_security::{SecretProtector, SecurityError};
use secondscrn_session::snapshot::{ClientPhase, ClientSnapshot};
use secondscrn_session::{ClientConfig, ClientEngine, ConnectTarget};
use secondscrn_transport::InterfacePolicy;

use crate::platform::AndroidPlatform;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ClientError {
    // Not `message`: that would clash with Throwable.message in Kotlin.
    #[error("{detail}")]
    Failed { detail: String },
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum StoreError {
    #[error("{detail}")]
    Failed { detail: String },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for StoreError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> Self {
        StoreError::Failed { detail: e.reason }
    }
}

/// Encrypts the device identity at rest. Implemented in Kotlin with an
/// Android Keystore AES-GCM key that never leaves secure hardware.
#[uniffi::export(with_foreign)]
pub trait SecretStore: Send + Sync {
    fn protect(&self, data: Vec<u8>) -> Result<Vec<u8>, StoreError>;
    fn unprotect(&self, data: Vec<u8>) -> Result<Vec<u8>, StoreError>;
}

#[uniffi::export(with_foreign)]
pub trait ClientListener: Send + Sync {
    fn on_state_changed(&self, state: ClientState);
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum Phase {
    Idle,
    Connecting,
    Pairing { pin: String },
    Negotiating,
    Streaming,
    Interrupted,
    Reconnecting { attempt: u32, retry_in_ms: u64 },
    Failed { message: String, needs_pairing: bool },
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum Preset {
    Automatic,
    Sharp,
    Balanced,
    LowBandwidth,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum PointerKind {
    Down,
    Move,
    Up,
    Cancel,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum PenKind {
    Hover,
    Down,
    Move,
    Up,
    Leave,
    Cancel,
}

/// One finger of a touch frame, in normalised (0..1) coordinates.
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct TouchPoint {
    pub id: u32,
    pub kind: PointerKind,
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ResolutionChoice {
    pub scale_percent: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum Health {
    Unknown,
    Good,
    Fair,
    Poor,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct HostItem {
    pub name: String,
    pub address: String,
    pub port: u16,
    pub fingerprint: String,
    pub fingerprint_short: String,
    pub paired: bool,
    pub discovered: bool,
    pub auto_reconnect: bool,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct StreamInfo {
    pub codec: String,
    pub encoder: String,
    pub width: u32,
    pub height: u32,
    pub max_fps: u32,
    /// Percentage of the panel resolution; 0 = a mode chosen on the PC.
    pub scale_percent: u32,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct Metrics {
    pub render_fps: f32,
    pub decode_ms: f32,
    pub bitrate_mbps: f32,
    pub rtt_ms: Option<f32>,
    pub loss_percent: Option<f32>,
    pub frames_dropped: u64,
    pub health: Health,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ClientState {
    pub phase: Phase,
    pub host_name: Option<String>,
    pub host_fingerprint_short: Option<String>,
    pub stream: Option<StreamInfo>,
    pub metrics: Metrics,
    pub hosts: Vec<HostItem>,
    pub decoders: Vec<String>,
    pub device_fingerprint_short: String,
    pub preset: Preset,
    pub touch_input: bool,
    pub touch_as_mouse: bool,
    pub rotation_locked: bool,
    /// Resolutions the PC can stream for this screen, largest first.
    pub resolutions: Vec<ResolutionChoice>,
    /// Frame rates this screen can show; `stream.max_fps` is the current one.
    pub frame_rates: Vec<u32>,
    /// Windows scale factor of this tablet's monitor (percent), while
    /// streaming, and the values Windows allows.
    pub ui_scale: Option<u32>,
    pub ui_scales: Vec<u32>,
    /// Snapdragon GSR upscaling: the setting, and whether this device can.
    pub super_resolution: bool,
    pub super_resolution_supported: bool,
}

fn preset_to(p: Preset) -> QualityPreset {
    match p {
        Preset::Automatic => QualityPreset::Automatic,
        Preset::Sharp => QualityPreset::Sharp,
        Preset::Balanced => QualityPreset::Balanced,
        Preset::LowBandwidth => QualityPreset::LowBandwidth,
    }
}

fn preset_from(p: QualityPreset) -> Preset {
    match p {
        QualityPreset::Automatic => Preset::Automatic,
        QualityPreset::Sharp => Preset::Sharp,
        QualityPreset::Balanced => Preset::Balanced,
        QualityPreset::LowBandwidth => Preset::LowBandwidth,
    }
}

fn health(h: secondscrn_session::snapshot::LinkHealth) -> Health {
    use secondscrn_session::snapshot::LinkHealth as L;
    match h {
        L::Unknown => Health::Unknown,
        L::Good => Health::Good,
        L::Fair => Health::Fair,
        L::Poor => Health::Poor,
    }
}

pub fn to_ffi(s: &ClientSnapshot) -> ClientState {
    let phase = match &s.phase {
        ClientPhase::Idle => Phase::Idle,
        ClientPhase::Connecting => Phase::Connecting,
        ClientPhase::Pairing { pin } => Phase::Pairing { pin: pin.clone() },
        ClientPhase::Negotiating => Phase::Negotiating,
        ClientPhase::Streaming => Phase::Streaming,
        ClientPhase::Interrupted => Phase::Interrupted,
        ClientPhase::Reconnecting { attempt, retry_in_ms } => {
            Phase::Reconnecting { attempt: *attempt, retry_in_ms: *retry_in_ms }
        }
        ClientPhase::Failed { message, needs_pairing } => {
            Phase::Failed { message: message.clone(), needs_pairing: *needs_pairing }
        }
    };
    // Known hosts first (most recent first), then discovered unknown ones.
    let mut hosts: Vec<HostItem> = {
        let mut known = s.known_hosts.clone();
        known.sort_by_key(|h| std::cmp::Reverse(h.last_connected));
        known
            .iter()
            .map(|k| {
                let seen = s.discovered.iter().find(|d| d.fingerprint == k.fingerprint);
                HostItem {
                    name: k.name.clone(),
                    // Prefer the address currently announced on the network.
                    address: seen.map(|d| d.address.clone()).unwrap_or_else(|| k.last_address.clone()),
                    port: seen.map(|d| d.port).unwrap_or(k.port),
                    fingerprint: k.fingerprint.clone(),
                    fingerprint_short: short(&k.fingerprint),
                    paired: true,
                    discovered: seen.is_some(),
                    auto_reconnect: k.auto_reconnect,
                }
            })
            .collect()
    };
    for d in s.discovered.iter().filter(|d| !d.paired) {
        hosts.push(HostItem {
            name: d.name.clone(),
            address: d.address.clone(),
            port: d.port,
            fingerprint: d.fingerprint.clone(),
            fingerprint_short: short(&d.fingerprint),
            paired: false,
            discovered: true,
            auto_reconnect: false,
        });
    }
    let m = &s.metrics;
    ClientState {
        phase,
        host_name: s.host.as_ref().map(|h| h.name.clone()),
        host_fingerprint_short: s.host.as_ref().map(|h| h.fingerprint_short.clone()),
        stream: s.stream.as_ref().map(|c| StreamInfo {
            codec: c.codec.to_string(),
            encoder: c.encoder.clone(),
            width: c.width,
            height: c.height,
            max_fps: c.max_fps,
            scale_percent: c.scale_percent,
        }),
        metrics: Metrics {
            render_fps: m.render_fps,
            decode_ms: m.decode_ms_avg,
            bitrate_mbps: m.receive_rate_bps as f32 / 1e6,
            rtt_ms: m.rtt_ms,
            loss_percent: m.loss.map(|l| l * 100.0),
            frames_dropped: m.frames_dropped,
            health: health(m.health),
        },
        hosts,
        decoders: s
            .decoders
            .iter()
            .map(|d| {
                format!(
                    "{} ({}{}, up to {}x{})",
                    d.codec,
                    d.implementation,
                    if d.hardware { "" } else { ", software" },
                    d.max_width,
                    d.max_height
                )
            })
            .collect(),
        device_fingerprint_short: s.device_fingerprint_short.clone(),
        preset: preset_from(s.settings.preset),
        touch_input: s.settings.touch_input,
        touch_as_mouse: s.settings.touch_as_mouse,
        rotation_locked: s.settings.rotation_locked,
        resolutions: s
            .resolutions
            .iter()
            .map(|o| ResolutionChoice { scale_percent: o.scale_percent, width: o.width, height: o.height })
            .collect(),
        frame_rates: s.frame_rates.clone(),
        ui_scale: s.ui_scale.as_ref().map(|u| u.current),
        ui_scales: s.ui_scale.as_ref().map(|u| u.options.clone()).unwrap_or_default(),
        super_resolution: s.settings.super_resolution,
        super_resolution_supported: crate::super_resolution_supported(),
    }
}

fn pointer_action(kind: PointerKind) -> PointerAction {
    match kind {
        PointerKind::Down => PointerAction::Down,
        PointerKind::Move => PointerAction::Move,
        PointerKind::Up => PointerAction::Up,
        PointerKind::Cancel => PointerAction::Cancel,
    }
}

fn short(hex: &str) -> String {
    secondscrn_security::Fingerprint::from_hex(hex).map(|f| f.short()).unwrap_or_default()
}

struct StoreProtector(Arc<dyn SecretStore>);

impl SecretProtector for StoreProtector {
    fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecurityError> {
        self.0.protect(plaintext.to_vec()).map_err(|e| SecurityError::Protector(e.to_string()))
    }
    fn unprotect(&self, ciphertext: &[u8]) -> Result<Vec<u8>, SecurityError> {
        self.0.unprotect(ciphertext.to_vec()).map_err(|e| SecurityError::Protector(e.to_string()))
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ClientInit {
    /// App-private directory (Context.filesDir).
    pub data_dir: String,
    pub device_name: String,
    pub device_model: String,
    pub app_version: String,
}

#[derive(uniffi::Object)]
pub struct SecondScrnClient {
    engine: ClientEngine,
    stop_listener: Arc<std::sync::atomic::AtomicBool>,
    listener_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

#[uniffi::export]
impl SecondScrnClient {
    #[uniffi::constructor]
    pub fn new(
        init: ClientInit,
        store: Arc<dyn SecretStore>,
        listener: Arc<dyn ClientListener>,
    ) -> Result<Arc<Self>, ClientError> {
        let logs = crate::logging::init();
        let platform = Arc::new(AndroidPlatform::new(init.device_name, init.device_model, init.app_version.clone()));
        let engine = ClientEngine::start(
            ClientConfig {
                data_dir: init.data_dir.into(),
                interfaces: InterfacePolicy { include_loopback: false, only: Vec::new() },
                protector: Arc::new(StoreProtector(store)),
                logs: Some(logs),
                app_version: init.app_version,
            },
            platform,
        )
        .map_err(|e| ClientError::Failed { detail: e.to_string() })?;

        crate::set_super_resolution(engine.snapshot().settings.super_resolution);
        // Probe the GPU off the UI path (creates a GL context once).
        std::thread::spawn(crate::super_resolution_supported);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut rx = engine.subscribe();
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("secondscrn-ui-state".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().expect("runtime");
                rt.block_on(async move {
                    listener.on_state_changed(to_ffi(&rx.borrow()));
                    while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                        match tokio::time::timeout(Duration::from_millis(500), rx.changed()).await {
                            Ok(Ok(())) => {
                                let s = to_ffi(&rx.borrow_and_update());
                                listener.on_state_changed(s);
                                // At most ~10 UI updates per second.
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                            Ok(Err(_)) => return,
                            Err(_) => {}
                        }
                    }
                });
            })
            .map_err(|e| ClientError::Failed { detail: e.to_string() })?;
        Ok(Arc::new(SecondScrnClient { engine, stop_listener: stop, listener_thread: Mutex::new(Some(thread)) }))
    }

    pub fn state(&self) -> ClientState {
        to_ffi(&self.engine.snapshot())
    }

    /// Connects to a host. Pass the fingerprint of a paired host to reconnect
    /// without a PIN; leave it empty to pair.
    pub fn connect(&self, address: String, port: u16, fingerprint: Option<String>) {
        let fingerprint = fingerprint.filter(|f| !f.is_empty());
        self.engine.connect(ConnectTarget { address: address.trim().to_string(), port, fingerprint });
    }

    pub fn connect_last(&self) -> bool {
        self.engine.connect_last()
    }

    pub fn disconnect(&self) {
        self.engine.disconnect();
    }

    pub fn forget_host(&self, fingerprint: String) {
        self.engine.forget_host(&fingerprint);
    }

    pub fn set_host_auto_reconnect(&self, fingerprint: String, enabled: bool) {
        self.engine.set_host_auto_reconnect(&fingerprint, enabled);
    }

    /// Size of the video surface in physical pixels.
    pub fn set_display(&self, width: u32, height: u32, refresh_hz: u32, dpi: u32) {
        let orientation = if height > width { Orientation::Portrait } else { Orientation::Landscape };
        self.engine.set_display(DisplayRequest { width, height, refresh_hz, dpi, orientation });
    }

    /// Normalised (0..1) pointer position on the streamed display.
    pub fn pointer(&self, kind: PointerKind, x: f32, y: f32, secondary: bool) {
        let action = pointer_action(kind);
        let button = if secondary { PointerButton::Secondary } else { PointerButton::Primary };
        self.engine.send_pointer(action, button, x, y);
    }

    /// A touch frame: every finger currently on the screen.
    pub fn touch(&self, points: Vec<TouchPoint>) {
        let contacts = points
            .into_iter()
            .map(|p| TouchContact { id: p.id, action: pointer_action(p.kind), x: p.x, y: p.y })
            .collect();
        self.engine.send_touch(contacts);
    }

    /// Stylus input. Tilt in degrees (-90..90, positive = right / towards
    /// the user), pressure 0..1.
    #[allow(clippy::too_many_arguments)]
    pub fn pen(
        &self,
        kind: PenKind,
        x: f32,
        y: f32,
        pressure: f32,
        tilt_x: f32,
        tilt_y: f32,
        barrel: bool,
        eraser: bool,
    ) {
        let action = match kind {
            PenKind::Hover => PenAction::Hover,
            PenKind::Down => PenAction::Down,
            PenKind::Move => PenAction::Move,
            PenKind::Up => PenAction::Up,
            PenKind::Leave => PenAction::Leave,
            PenKind::Cancel => PenAction::Cancel,
        };
        self.engine.send_pen(action, x, y, pressure, tilt_x, tilt_y, barrel, eraser);
    }

    /// Asks the PC for another stream resolution (percentage of the panel).
    pub fn set_resolution(&self, scale_percent: u32) {
        self.engine.set_resolution(scale_percent);
    }

    /// Asks the PC to stream at most `fps` frames per second.
    pub fn set_frame_rate(&self, fps: u32) {
        self.engine.set_frame_rate(fps);
    }

    /// Sets the Windows scale factor of this tablet's monitor (percent).
    pub fn set_ui_scale(&self, percent: u32) {
        self.engine.set_ui_scale(percent);
    }

    /// Snapdragon GSR upscaling of streams below the panel resolution. Takes
    /// effect when the video surface is next set up (the UI resizes it).
    pub fn set_super_resolution(&self, enabled: bool) {
        crate::set_super_resolution(enabled);
        let mut s = self.engine.snapshot().settings;
        s.super_resolution = enabled;
        self.engine.update_settings(s);
    }

    pub fn set_touch_as_mouse(&self, enabled: bool) {
        let mut s = self.engine.snapshot().settings;
        s.touch_as_mouse = enabled;
        self.engine.update_settings(s);
    }

    pub fn set_rotation_locked(&self, locked: bool) {
        let mut s = self.engine.snapshot().settings;
        s.rotation_locked = locked;
        self.engine.update_settings(s);
    }

    pub fn scroll(&self, x: f32, y: f32, delta_x: f32, delta_y: f32) {
        self.engine.send_scroll(x, y, delta_x, delta_y);
    }

    pub fn request_keyframe(&self) {
        self.engine.request_keyframe();
    }

    pub fn set_preset(&self, preset: Preset) {
        self.engine.set_preset(preset_to(preset));
    }

    pub fn set_touch_input(&self, enabled: bool) {
        let mut s = self.engine.snapshot().settings;
        s.touch_input = enabled;
        self.engine.update_settings(s);
    }

    pub fn set_device_name(&self, name: String) {
        let mut s = self.engine.snapshot().settings;
        s.device_name = Some(name).filter(|n| !n.trim().is_empty());
        self.engine.update_settings(s);
    }

    pub fn start_discovery(&self) {
        self.engine.start_discovery();
    }

    pub fn stop_discovery(&self) {
        self.engine.stop_discovery();
    }

    pub fn export_diagnostics(&self) -> String {
        self.engine.export_diagnostics()
    }

    pub fn shutdown(&self) {
        self.stop_listener.store(true, std::sync::atomic::Ordering::Relaxed);
        self.engine.shutdown();
        if let Some(t) = self.listener_thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = t.join();
        }
    }
}
