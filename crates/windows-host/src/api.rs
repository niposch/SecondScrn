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
use secondscrn_session::settings::{DevicePreferences, EncoderGpu, HostSettings, Theme};
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
    Smooth,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum GpuKind {
    Integrated,
    Dedicated,
}

/// A GPU with hardware video encoders.
#[derive(Debug, Clone, uniffi::Record)]
pub struct GpuView {
    pub name: String,
    pub kind: GpuKind,
    /// Its encoders work in this process.
    pub usable: bool,
    /// The selected encoders run on it.
    pub active: bool,
    /// Why its encoders don't work, if they don't.
    pub problem: Option<String>,
}

/// The encoder GPU choice and whether it is in effect.
#[derive(Debug, Clone, uniffi::Record)]
pub struct EncoderGpuView {
    pub gpus: Vec<GpuView>,
    /// More than one kind of GPU: the choice matters.
    pub choice_available: bool,
    /// The chosen GPU becomes usable after SecondScrn restarts.
    pub restart_required: bool,
    /// The chosen GPU cannot encode (and a restart won't help).
    pub problem: Option<String>,
}

/// The PC and a tablet run different app versions (before 1.0 they must
/// match): the older side installs exactly the other's version.
#[derive(Debug, Clone, uniffi::Record)]
pub struct VersionMismatchView {
    pub device_name: String,
    pub tablet_version: String,
    pub host_version: String,
    /// The tablet is behind (otherwise this PC is).
    pub tablet_older: bool,
    /// Download of the tablet app matching this PC.
    pub tablet_app_url: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct BenchResultView {
    pub gpu: String,
    pub kind: GpuKind,
    pub encoder: String,
    pub paced_fps: f32,
    pub latency_ms: f32,
    pub latency_p95_ms: f32,
    pub max_fps: f32,
    pub error: Option<String>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct BenchmarkView {
    pub running: bool,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub results: Vec<BenchResultView>,
    pub error: Option<String>,
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
    /// The tablet's preset (its own, or the default).
    pub preset: Preset,
    /// Resolution or frame rate differ from the preset's.
    pub custom: bool,
    /// The network can't carry the chosen resolution and frame rate.
    pub network_limited: bool,
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
    /// `None`: automatic.
    pub encoder_gpu: Option<GpuKind>,
    /// Highest frame rate of any stream; `None`: no limit.
    pub fps_limit: Option<u32>,
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
    pub encoder_gpu: EncoderGpuView,
    pub benchmark: BenchmarkView,
    pub last_error: Option<String>,
    /// A tablet with another app version tried to connect.
    pub version_mismatch: Option<VersionMismatchView>,
    pub settings: SettingsView,
    pub data_dir: String,
    pub version: String,
}

fn preset_from(p: QualityPreset) -> Preset {
    match p {
        QualityPreset::Automatic => Preset::Automatic,
        QualityPreset::Sharp => Preset::Sharp,
        QualityPreset::Smooth => Preset::Smooth,
        QualityPreset::LowBandwidth => Preset::LowBandwidth,
    }
}

fn preset_to(p: Preset) -> QualityPreset {
    match p {
        Preset::Automatic => QualityPreset::Automatic,
        Preset::Sharp => QualityPreset::Sharp,
        Preset::Smooth => QualityPreset::Smooth,
        Preset::LowBandwidth => QualityPreset::LowBandwidth,
    }
}

fn gpu_from(g: EncoderGpu) -> GpuKind {
    match g {
        EncoderGpu::Integrated => GpuKind::Integrated,
        EncoderGpu::Dedicated => GpuKind::Dedicated,
    }
}

fn gpu_to(g: GpuKind) -> EncoderGpu {
    match g {
        GpuKind::Integrated => EncoderGpu::Integrated,
        GpuKind::Dedicated => EncoderGpu::Dedicated,
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
        encoder_gpu: s.encoder_gpu_choice.map(gpu_from),
        fps_limit: s.fps_limit,
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
    base.encoder_gpu_choice = v.encoder_gpu.map(gpu_to);
    base.fps_limit = v.fps_limit;
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

pub fn to_view(s: &HostSnapshot, data_dir: &str, extras: &Extras) -> HostState {
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
            preset: preset_from(x.preset),
            custom: x.custom,
            network_limited: x.network_limited,
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
        encoder_gpu: extras.encoder_gpu.clone(),
        benchmark: extras.benchmark.clone(),
        last_error: s.last_error.clone(),
        version_mismatch: s.version_mismatch.as_ref().map(|m| VersionMismatchView {
            device_name: m.device_name.clone(),
            tablet_version: m.tablet_version.clone(),
            host_version: m.host_version.clone(),
            tablet_older: secondscrn_protocol::compare_versions(&m.tablet_version, &m.host_version)
                == Some(std::cmp::Ordering::Less),
            tablet_app_url: secondscrn_protocol::android_apk_url(&m.host_version),
        }),
        settings: settings_from(&s.settings),
        data_dir: data_dir.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Host state that lives outside the session engine.
#[derive(Clone)]
pub struct Extras {
    pub encoder_gpu: EncoderGpuView,
    pub benchmark: BenchmarkView,
}

/// [`Extras`] plus a change counter the UI watcher polls.
struct SharedExtras {
    extras: Mutex<Extras>,
    generation: std::sync::atomic::AtomicU64,
}

impl SharedExtras {
    fn get(&self) -> Extras {
        self.extras.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn update(&self, f: impl FnOnce(&mut Extras)) {
        f(&mut self.extras.lock().unwrap_or_else(|e| e.into_inner()));
        self.generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The running executable (the app, or the CLI).
fn current_exe() -> Option<std::path::PathBuf> {
    std::env::current_exe().ok()
}

/// Makes Windows start SecondScrn on the chosen GPU from the next launch
/// (see [`crate::gpu`]): an encoder only starts in a process Windows runs on
/// its GPU. Automatic leaves the choice to Windows (and to its per-app
/// graphics setting); switching to it removes the preference set for an
/// earlier explicit choice.
fn apply_gpu_preference(choice: Option<EncoderGpu>, switching: bool) {
    let Some(exe) = current_exe() else { return };
    let wanted = match choice {
        Some(EncoderGpu::Dedicated) => Some(crate::gpu::HIGH_PERFORMANCE),
        Some(EncoderGpu::Integrated) => Some(crate::gpu::POWER_SAVING),
        None if switching => None,
        None => return,
    };
    if let Err(e) = crate::gpu::set_preference(&exe, wanted) {
        tracing::warn!(error = format!("{e:#}"), "cannot set the graphics preference");
    }
}

fn encoder_gpu_view(
    platform: &WindowsHostPlatform,
    choice: Option<EncoderGpu>,
    started_with: Option<u32>,
) -> EncoderGpuView {
    let active = platform.active_gpu();
    let gpus: Vec<GpuView> = platform
        .gpus()
        .into_iter()
        .map(|g| GpuView {
            active: active.as_ref().is_some_and(|(name, _)| *name == g.name),
            usable: !g.codecs.is_empty(),
            kind: gpu_from(g.kind),
            problem: g.problem,
            name: g.name,
        })
        .collect();
    let choice_available =
        gpus.iter().any(|g| g.kind == GpuKind::Integrated) && gpus.iter().any(|g| g.kind == GpuKind::Dedicated);
    // Windows applies the graphics preference at process start.
    let pending = current_exe().map(|e| crate::gpu::preference(&e)).unwrap_or(started_with) != started_with;
    let (restart_required, problem) = match choice {
        None => (choice_available && pending, None),
        Some(kind) => {
            let chosen: Vec<&GpuView> = gpus.iter().filter(|g| g.kind == gpu_from(kind)).collect();
            let in_use = active.as_ref().is_some_and(|(_, k)| *k == kind);
            let restart = choice_available && pending && !in_use;
            let blocked = !chosen.is_empty() && !chosen.iter().any(|g| g.usable);
            let problem = (blocked && !restart).then(|| chosen.iter().find_map(|g| g.problem.clone())).flatten();
            (restart, problem)
        }
    };
    EncoderGpuView { choice_available, restart_required, problem, gpus }
}

fn bench_view(r: crate::bench::BenchResult) -> BenchResultView {
    BenchResultView {
        gpu: r.gpu,
        kind: gpu_from(r.kind),
        encoder: r.encoder,
        paced_fps: r.paced_fps,
        latency_ms: r.latency_ms,
        latency_p95_ms: r.latency_p95_ms,
        max_fps: r.max_fps,
        error: r.error,
    }
}

/// Runs the benchmark in the helper executable next to the app, once per
/// kind of GPU with the matching graphics preference: an encoder only shows
/// its real speed in a process Windows runs on its GPU (see [`crate::bench`]),
/// and the app's own process is fixed to one GPU when it starts.
fn run_benchmark_helper(width: u32, height: u32, fps: u32) -> anyhow::Result<Vec<crate::bench::BenchResult>> {
    use anyhow::Context;
    let dir = current_exe().and_then(|e| e.parent().map(|p| p.to_path_buf())).context("no executable path")?;
    let helper = dir.join("secondscrn-host-cli.exe");
    anyhow::ensure!(helper.exists(), "the benchmark helper is missing ({})", helper.display());
    let mut results = Vec::new();
    let gpus = [
        // Power saving, not "let Windows decide": on desktops whose screens
        // are on the graphics card, the integrated GPU's encoder only starts
        // in a process Windows runs on the integrated GPU.
        (EncoderGpu::Integrated, "integrated", Some(crate::gpu::POWER_SAVING)),
        (EncoderGpu::Dedicated, "dedicated", Some(crate::gpu::HIGH_PERFORMANCE)),
    ];
    for (kind, arg, pref) in gpus {
        let size = format!("{width}x{height}");
        let fps = fps.to_string();
        let run = crate::gpu::set_preference(&helper, pref)
            .and_then(|()| run_helper(&helper, &["bench", &size, &fps, "2", "--gpu", arg, "--json"]));
        match run {
            Ok(report) => results.extend(report.results),
            // One kind failing must not hide the other's results.
            Err(e) => results.push(crate::bench::BenchResult {
                gpu: format!("{arg} GPU"),
                kind,
                encoder: String::new(),
                codec: secondscrn_protocol::Codec::H265,
                paced_fps: 0.0,
                latency_ms: 0.0,
                latency_p95_ms: 0.0,
                max_fps: 0.0,
                error: Some(format!("{e:#}")),
            }),
        }
    }
    Ok(results)
}

fn run_helper(helper: &std::path::Path, args: &[&str]) -> anyhow::Result<crate::bench::BenchReport> {
    use anyhow::Context;
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut child = std::process::Command::new(helper)
        .args(args)
        .env("RUST_LOG", "warn")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("cannot start the benchmark helper")?;
    let mut stdout = child.stdout.take().context("no output")?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut out);
        out
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            anyhow::bail!("the benchmark did not finish");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let out = reader.join().unwrap_or_default();
    anyhow::ensure!(status.success(), "the benchmark helper failed ({status})");
    serde_json::from_str(out.trim()).context("unexpected benchmark output")
}

#[derive(uniffi::Object)]
pub struct SecondScrnHost {
    engine: Arc<HostEngine>,
    platform: Arc<WindowsHostPlatform>,
    logs: LogBuffer,
    data_dir: String,
    extras: Arc<SharedExtras>,
    /// The graphics preference this process was started with.
    started_with: Option<u32>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    watcher: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl SecondScrnHost {
    fn refresh_encoder_gpu(&self, choice: Option<EncoderGpu>) {
        let view = encoder_gpu_view(&self.platform, choice, self.started_with);
        self.extras.update(|x| x.encoder_gpu = view);
    }
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
        let settings = secondscrn_session::settings::load::<HostSettings>(&data_dir.join("settings.json"));
        let logs = crate::logging::init(&data_dir.join("logs"), settings.verbose_logging);
        tracing::info!(version = env!("CARGO_PKG_VERSION"), "SecondScrn host starting");

        let started_with = current_exe().and_then(|e| crate::gpu::preference(&e));
        // Keep the preference in line with the setting (e.g. after it was
        // reset in Windows); a change applies from the next start.
        apply_gpu_preference(settings.encoder_gpu_choice, false);
        let platform = Arc::new(WindowsHostPlatform::new(DisplaySource::Virtual, settings.encoder_gpu_choice));
        let engine = Arc::new(
            HostEngine::start(
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
            .map_err(fail)?,
        );
        let extras = Arc::new(SharedExtras {
            extras: Mutex::new(Extras {
                encoder_gpu: encoder_gpu_view(&platform, settings.encoder_gpu_choice, started_with),
                benchmark: BenchmarkView {
                    running: false,
                    width: 0,
                    height: 0,
                    fps: 0,
                    results: Vec::new(),
                    error: None,
                },
            }),
            generation: Default::default(),
        });

        let dir_string = data_dir.display().to_string();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (mut rx, stop2, dir2, extras2) = (engine.subscribe(), stop.clone(), dir_string.clone(), extras.clone());
        let watcher = std::thread::Builder::new()
            .name("secondscrn-ui-state".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().expect("runtime");
                rt.block_on(async move {
                    let generation = || extras2.generation.load(std::sync::atomic::Ordering::Relaxed);
                    let mut seen = generation();
                    listener.on_state_changed(to_view(&rx.borrow(), &dir2, &extras2.get()));
                    while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                        match tokio::time::timeout(Duration::from_millis(500), rx.changed()).await {
                            Ok(Ok(())) => {
                                seen = generation();
                                let s = to_view(&rx.borrow_and_update(), &dir2, &extras2.get());
                                listener.on_state_changed(s);
                                tokio::time::sleep(Duration::from_millis(250)).await;
                            }
                            Ok(Err(_)) => return,
                            // Changes outside the engine (GPU, benchmark).
                            Err(_) if generation() != seen => {
                                seen = generation();
                                listener.on_state_changed(to_view(&rx.borrow(), &dir2, &extras2.get()));
                            }
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
            extras,
            started_with,
            stop,
            watcher: Mutex::new(Some(watcher)),
        }))
    }

    pub fn state(&self) -> HostState {
        to_view(&self.engine.snapshot(), &self.data_dir, &self.extras.get())
    }

    /// Measures every GPU's encoder at this size and frame rate (a few
    /// seconds; results arrive through the state listener).
    pub fn run_benchmark(&self, width: u32, height: u32, fps: u32) {
        let mut already = false;
        self.extras.update(|x| {
            already = x.benchmark.running;
            if !already {
                x.benchmark = BenchmarkView { running: true, width, height, fps, results: Vec::new(), error: None };
            }
        });
        if already {
            return;
        }
        let extras = self.extras.clone();
        std::thread::spawn(move || {
            let r = run_benchmark_helper(width, height, fps);
            extras.update(|x| {
                x.benchmark.running = false;
                match r {
                    Ok(results) => x.benchmark.results = results.into_iter().map(bench_view).collect(),
                    Err(e) => x.benchmark.error = Some(format!("{e:#}")),
                }
            });
        });
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

    /// Ends the session and has the tablet connect again, so settings for
    /// new connections apply.
    pub fn reconnect(&self) {
        self.engine.reconnect();
    }

    /// Sets the default preset (tablets without their own).
    pub fn set_preset(&self, preset: Preset) {
        self.engine.set_preset(preset_to(preset));
    }

    /// Sets the connected tablet's preset; choosing the current one again
    /// undoes resolution and frame-rate changes.
    pub fn set_session_preset(&self, preset: Preset) -> Result<(), HostError> {
        self.engine.set_session_preset(preset_to(preset)).map_err(fail)
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
        let (old_gpu, new_gpu) = (base.encoder_gpu_choice, settings.encoder_gpu.map(gpu_to));
        self.engine.update_settings(settings_to(settings, base));
        if old_gpu != new_gpu {
            apply_gpu_preference(new_gpu, true);
            self.refresh_encoder_gpu(new_gpu);
            // Takes effect right away when this process can already use that
            // GPU's encoder; otherwise the view asks for a restart.
            let (platform, engine, extras) = (self.platform.clone(), self.engine.clone(), self.extras.clone());
            let started_with = self.started_with;
            std::thread::spawn(move || {
                platform.set_encoder_gpu(new_gpu);
                engine.refresh_platform();
                let view = encoder_gpu_view(&platform, new_gpu, started_with);
                extras.update(|x| x.encoder_gpu = view);
            });
        }
    }

    /// Re-detects the driver and encoders (e.g. after installing the driver).
    pub fn refresh(&self) {
        let (platform, engine, extras) = (self.platform.clone(), self.engine.clone(), self.extras.clone());
        let started_with = self.started_with;
        std::thread::spawn(move || {
            platform.reprobe();
            engine.refresh_platform();
            let view = encoder_gpu_view(&platform, engine.snapshot().settings.encoder_gpu_choice, started_with);
            extras.update(|x| x.encoder_gpu = view);
        });
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
