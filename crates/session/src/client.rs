//! Client (tablet) engine: discovery, pairing, connection, decoding and
//! automatic reconnection. Publishes [`ClientSnapshot`]s for the UI.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use secondscrn_diagnostics::LogBuffer;
use secondscrn_protocol::{
    ALPN, ClientMessage, ClientStats, CodecCapability, DisplayRequest, FrameTiming, HostMessage, InputEvent, Intent,
    Orientation, PROTOCOL_VERSION, PenAction, PointerAction, PointerButton, QualityPreset, RejectReason, StreamConfig,
    TouchContact,
};
use secondscrn_security::pairing::{ClientPairing, generate_pin};
use secondscrn_security::tls::{PAIRING_EXPORTER_LABEL, client_config, peer_fingerprint, server_name};
use secondscrn_security::trust::now_unix;
use secondscrn_security::{Fingerprint, Identity, SecretProtector};
use secondscrn_transport::{InterfacePolicy, ReceivedFrame, ReceiverConfig, TransportEvent, accept_offer};
use tokio::net::TcpStream;
use tokio::runtime::{Handle, Runtime};
use tokio::sync::{mpsc, watch};
use tokio_rustls::TlsConnector;

use crate::SessionError;
use crate::backoff::Backoff;
use crate::channel::{ControlChannel, PEER_TIMEOUT, PING_INTERVAL, STATS_INTERVAL, STEP_TIMEOUT};
use crate::platform::{ClientPlatform, DecodeOutcome, DecoderStats, PlatformError, UiScale};
use crate::settings::{self, ClientSettings, KnownHost};
use crate::snapshot::{
    ClientMetrics, ClientPhase, ClientSnapshot, DiscoveredHost, HostSummary, LinkHealth, PerformanceWarning,
};

/// How long the user has to type the PIN on the PC.
const PAIRING_TIMEOUT: Duration = Duration::from_secs(130);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
/// How often received frames are reported to the PC (latency feedback).
const FEEDBACK_INTERVAL: Duration = Duration::from_millis(50);
/// A request for a recovery or keyframe is repeated after this long.
const KEYFRAME_REQUEST_REPEAT: Duration = Duration::from_millis(300);
/// The tablet's render rate is measured over about this long.
const RENDER_RATE_WINDOW: Duration = Duration::from_millis(500);
/// Frames queued for the decoder. Frames arrive in bursts (e.g. after a
/// retransmission) and dropping one breaks the reference chain, which costs a
/// keyframe: at tablet resolution a burst large enough to cause the next
/// loss. So every frame is decoded and only rendering skips stale pictures
/// (see the platform decoder); this bound is only a safety net (~1 s).
const DECODE_QUEUE: usize = 60;
/// Frames waiting for the decoder at which the PC is asked to slow down:
/// about 50 ms worth (Wi-Fi delivers a few frames at once now and then;
/// that alone must not count), at least 3.
fn slowdown_queue(fps: u32) -> usize {
    ((fps as usize).div_ceil(20)).max(3)
}
/// No performance warnings this long after the stream (re)starts.
const WARNING_GRACE: Duration = Duration::from_secs(10);
/// A performance warning stays this long after its cause was last seen.
const WARNING_HOLD: Duration = Duration::from_secs(15);

/// Only fresh, sustained evidence outside stream start-up can raise a warning.
#[derive(Default)]
struct WarningTracker {
    started: Option<Instant>,
    congested_reports: u32,
    last_report: Option<Instant>,
    behind: std::collections::VecDeque<Instant>,
    warning: Option<(PerformanceWarning, Instant)>,
}

impl WarningTracker {
    fn reset(&mut self, now: Instant) {
        *self = Self { started: Some(now), ..Self::default() };
    }

    fn settled(&self, now: Instant) -> bool {
        self.started.is_some_and(|t| now.duration_since(t) >= WARNING_GRACE)
    }

    fn network_report(&mut self, now: Instant, loss: Option<f32>, rtt_ms: Option<f32>) {
        let bad = loss.is_some_and(|l| l > 0.05) || rtt_ms.is_some_and(|r| r > 150.0);
        if bad && self.last_report.is_some_and(|t| now.duration_since(t) < PING_INTERVAL) {
            return;
        }
        self.last_report = Some(now);
        self.congested_reports = if bad && self.settled(now) { self.congested_reports.saturating_add(1) } else { 0 };
        if self.congested_reports >= 3 {
            self.warning = Some((PerformanceWarning::Network, now));
        }
    }

    fn decoder_behind(&mut self, now: Instant) {
        if !self.settled(now) {
            return;
        }
        self.behind.retain(|t| now.duration_since(*t) <= Duration::from_secs(10));
        self.behind.push_back(now);
        if self.behind.len() >= 2 {
            self.warning = Some((PerformanceWarning::Decoder, now));
        }
    }

    /// The PC reports whether the network carries the chosen quality.
    fn network_limited(&mut self, now: Instant, limited: bool) {
        if limited {
            self.warning = Some((PerformanceWarning::Bandwidth, now));
        } else if self.warning.is_some_and(|(w, _)| w == PerformanceWarning::Bandwidth) {
            self.warning = None;
        }
    }

    fn current(&mut self, now: Instant) -> Option<PerformanceWarning> {
        if self.warning.is_some_and(|(_, t)| now.duration_since(t) >= WARNING_HOLD) {
            self.warning = None;
        }
        self.warning.map(|(w, _)| w)
    }
}
/// A frame that waited this long for the decoder (with more behind it) is
/// skipped along with the rest until the next keyframe: bounded latency.
const MAX_DECODE_WAIT: Duration = Duration::from_millis(250);

pub struct ClientConfig {
    pub data_dir: PathBuf,
    pub interfaces: InterfacePolicy,
    pub protector: Arc<dyn SecretProtector>,
    pub logs: Option<LogBuffer>,
    pub app_version: String,
}

/// Where to connect. With a `fingerprint` of a known host the connection is
/// pinned; without one, a PIN pairing starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectTarget {
    pub address: String,
    pub port: u16,
    pub fingerprint: Option<String>,
}

#[derive(Default)]
struct ClientState {
    phase: Option<ClientPhase>,
    host: Option<HostSummary>,
    stream: Option<StreamConfig>,
    metrics: ClientMetrics,
    ui_scale: Option<UiScale>,
}

struct Connection {
    cancel: watch::Sender<bool>,
}

struct ClientInner {
    platform: Arc<dyn ClientPlatform>,
    identity: Identity,
    data_dir: PathBuf,
    interfaces: InterfacePolicy,
    logs: Option<LogBuffer>,
    app_version: String,
    settings: Mutex<ClientSettings>,
    state: Mutex<ClientState>,
    decoders: Mutex<Vec<CodecCapability>>,
    snapshot_tx: watch::Sender<ClientSnapshot>,
    handle: Handle,
    connection: Mutex<Option<Connection>>,
    session_tx: Mutex<Option<mpsc::UnboundedSender<ClientMessage>>>,
    display: Mutex<Option<DisplayRequest>>,
    input_seq: AtomicU64,
    discovery: Mutex<Option<Arc<AtomicBool>>>,
    discovered: Mutex<Vec<(secondscrn_discovery::HostAdvert, Instant)>>,
}

pub struct ClientEngine {
    inner: Arc<ClientInner>,
    runtime: Mutex<Option<Runtime>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ClientEngine {
    pub fn start(config: ClientConfig, platform: Arc<dyn ClientPlatform>) -> Result<Self, SessionError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("secondscrn-client")
            .enable_all()
            .build()?;
        let engine = Self::start_on(runtime.handle().clone(), config, platform)?;
        *lock(&engine.runtime) = Some(runtime);
        Ok(engine)
    }

    pub fn start_on(
        handle: Handle,
        config: ClientConfig,
        platform: Arc<dyn ClientPlatform>,
    ) -> Result<Self, SessionError> {
        std::fs::create_dir_all(&config.data_dir)?;
        let device = platform.device_info();
        let identity =
            Identity::load_or_create(&config.data_dir.join("identity.bin"), &device.name, config.protector.as_ref())?;
        let settings: ClientSettings = settings::load(&config.data_dir.join("settings.json"));
        let (snapshot_tx, _) = watch::channel(ClientSnapshot {
            phase: ClientPhase::Idle,
            host: None,
            stream: None,
            metrics: ClientMetrics::default(),
            discovered: Vec::new(),
            known_hosts: settings.known_hosts.clone(),
            decoders: Vec::new(),
            settings: settings.clone(),
            device_fingerprint_short: identity.fingerprint().short(),
            resolutions: Vec::new(),
            frame_rates: Vec::new(),
            ui_scale: None,
        });
        let inner = Arc::new(ClientInner {
            platform: platform.clone(),
            identity,
            data_dir: config.data_dir,
            interfaces: config.interfaces,
            logs: config.logs,
            app_version: config.app_version,
            settings: Mutex::new(settings),
            state: Mutex::new(ClientState::default()),
            decoders: Mutex::new(Vec::new()),
            snapshot_tx,
            handle: handle.clone(),
            connection: Mutex::new(None),
            session_tx: Mutex::new(None),
            display: Mutex::new(None),
            input_seq: AtomicU64::new(1),
            discovery: Mutex::new(None),
            discovered: Mutex::new(Vec::new()),
        });
        // Probing decoders instantiates codecs; do it off the caller's thread.
        let probe = inner.clone();
        handle.spawn_blocking(move || {
            let caps = probe.platform.decoders();
            tracing::info!(decoders = ?caps.iter().map(|c| format!("{}:{}", c.codec, c.implementation)).collect::<Vec<_>>(), "decoder capabilities");
            *lock(&probe.decoders) = caps;
            probe.publish();
        });
        inner.publish();
        Ok(ClientEngine { inner, runtime: Mutex::new(None) })
    }

    pub fn snapshot(&self) -> ClientSnapshot {
        self.inner.snapshot_tx.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<ClientSnapshot> {
        self.inner.snapshot_tx.subscribe()
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.inner.identity.fingerprint()
    }

    /// Sets the size of the video surface; sent to the host on connect and on
    /// every change while streaming (rotation, multi-window).
    pub fn set_display(&self, display: DisplayRequest) {
        let changed = lock(&self.inner.display).replace(display) != Some(display);
        if changed {
            self.inner.send_to_session(ClientMessage::DisplayChanged { display });
            self.inner.publish();
        }
    }

    /// Connects (or pairs, for an unknown host). Replaces any current connection.
    pub fn connect(&self, target: ConnectTarget) {
        self.disconnect_inner(false);
        let (cancel, cancel_rx) = watch::channel(false);
        *lock(&self.inner.connection) = Some(Connection { cancel });
        let inner = self.inner.clone();
        self.inner.handle.spawn(connection_loop(inner, target, cancel_rx));
    }

    /// Connects to the most recently used host that allows auto-reconnect.
    pub fn connect_last(&self) -> bool {
        let host = {
            let s = lock(&self.inner.settings);
            s.known_hosts.iter().filter(|h| h.auto_reconnect).max_by_key(|h| h.last_connected).cloned()
        };
        match host {
            Some(h) => {
                self.connect(ConnectTarget { address: h.last_address, port: h.port, fingerprint: Some(h.fingerprint) });
                true
            }
            None => false,
        }
    }

    pub fn disconnect(&self) {
        self.disconnect_inner(true);
    }

    fn disconnect_inner(&self, publish_idle: bool) {
        self.inner.send_to_session(ClientMessage::Goodbye);
        if let Some(c) = lock(&self.inner.connection).take() {
            let _ = c.cancel.send(true);
        }
        if publish_idle {
            self.inner.set_phase(ClientPhase::Idle);
        }
    }

    pub fn forget_host(&self, fingerprint_hex: &str) -> bool {
        let removed = lock(&self.inner.settings).remove_host(fingerprint_hex);
        self.inner.save_settings();
        self.inner.publish();
        removed
    }

    pub fn send_pointer(&self, action: PointerAction, button: PointerButton, x: f32, y: f32) {
        let seq = self.inner.input_seq.fetch_add(1, Ordering::Relaxed);
        let event = InputEvent::Pointer { seq, action, button, x: x.clamp(0.0, 1.0), y: y.clamp(0.0, 1.0) };
        self.inner.send_to_session(ClientMessage::Input { event });
    }

    pub fn send_scroll(&self, x: f32, y: f32, delta_x: f32, delta_y: f32) {
        let seq = self.inner.input_seq.fetch_add(1, Ordering::Relaxed);
        let event = InputEvent::Scroll { seq, x: x.clamp(0.0, 1.0), y: y.clamp(0.0, 1.0), delta_x, delta_y };
        self.inner.send_to_session(ClientMessage::Input { event });
    }

    /// A touch frame with every current contact (see [`InputEvent::Touch`]).
    pub fn send_touch(&self, mut contacts: Vec<TouchContact>) {
        for c in &mut contacts {
            c.x = c.x.clamp(0.0, 1.0);
            c.y = c.y.clamp(0.0, 1.0);
        }
        let seq = self.inner.input_seq.fetch_add(1, Ordering::Relaxed);
        self.inner.send_to_session(ClientMessage::Input { event: InputEvent::Touch { seq, contacts } });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn send_pen(
        &self,
        action: PenAction,
        x: f32,
        y: f32,
        pressure: f32,
        tilt_x: f32,
        tilt_y: f32,
        barrel: bool,
        eraser: bool,
    ) {
        let seq = self.inner.input_seq.fetch_add(1, Ordering::Relaxed);
        let event = InputEvent::Pen {
            seq,
            action,
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
            pressure: pressure.clamp(0.0, 1.0),
            tilt_x: tilt_x.clamp(-90.0, 90.0),
            tilt_y: tilt_y.clamp(-90.0, 90.0),
            barrel,
            eraser,
        };
        self.inner.send_to_session(ClientMessage::Input { event });
    }

    /// Asks the PC to stream at `scale_percent` of the panel resolution. The
    /// PC remembers the choice for this tablet.
    pub fn set_resolution(&self, scale_percent: u32) {
        self.inner.send_to_session(ClientMessage::SetResolution { scale_percent });
    }

    /// Asks the PC to stream at most `fps` frames per second (remembered by
    /// the PC for this tablet).
    pub fn set_frame_rate(&self, fps: u32) {
        self.inner.send_to_session(ClientMessage::SetFrameRate { fps });
    }

    /// Sets the Windows scale factor of this tablet's monitor (percent).
    pub fn set_ui_scale(&self, percent: u32) {
        self.inner.send_to_session(ClientMessage::SetUiScale { percent });
    }

    /// Picks the video codec (`None` = automatic). Applies to the running
    /// session by reconnecting: the codec is fixed per WebRTC session.
    pub fn set_preferred_codec(&self, codec: Option<secondscrn_protocol::Codec>) {
        let changed = {
            let mut s = lock(&self.inner.settings);
            let changed = s.preferred_codec != codec;
            s.preferred_codec = codec;
            changed
        };
        self.inner.save_settings();
        self.inner.publish();
        let host = lock(&self.inner.state).host.clone();
        if changed && let Some(h) = host {
            let streaming = lock(&self.inner.state).stream.is_some();
            if streaming {
                tracing::info!(?codec, "reconnecting with another codec");
                self.connect(ConnectTarget { address: h.address, port: h.port, fingerprint: Some(h.fingerprint) });
            }
        }
    }

    /// Call after the video surface was recreated.
    pub fn request_keyframe(&self) {
        self.inner.send_to_session(ClientMessage::RequestKeyframe);
    }

    pub fn set_preset(&self, preset: QualityPreset) {
        lock(&self.inner.settings).preset = preset;
        self.inner.save_settings();
        self.inner.send_to_session(ClientMessage::SetPreset { preset });
        self.inner.publish();
    }

    pub fn update_settings(&self, new: ClientSettings) {
        *lock(&self.inner.settings) = new;
        self.inner.save_settings();
        self.inner.publish();
    }

    pub fn set_host_auto_reconnect(&self, fingerprint_hex: &str, enabled: bool) {
        {
            let mut s = lock(&self.inner.settings);
            if let Some(h) = s.known_hosts.iter_mut().find(|h| h.fingerprint == fingerprint_hex) {
                h.auto_reconnect = enabled;
            }
        }
        self.inner.save_settings();
        self.inner.publish();
    }

    /// Starts mDNS browsing. The caller (Android) must hold a multicast lock.
    pub fn start_discovery(&self) {
        let mut d = lock(&self.inner.discovery);
        if d.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        *d = Some(stop.clone());
        let inner = self.inner.clone();
        std::thread::Builder::new()
            .name("secondscrn-discovery".into())
            .spawn(move || {
                let browser = match secondscrn_discovery::Browser::start() {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(error = %e, "discovery unavailable");
                        return;
                    }
                };
                while !stop.load(Ordering::Relaxed) {
                    let ev = browser.recv_timeout(Duration::from_millis(500));
                    let mut list = lock(&inner.discovered);
                    let now = Instant::now();
                    match ev {
                        Some(secondscrn_discovery::BrowseEvent::Found(ad)) => {
                            list.retain(|(a, _)| a.instance != ad.instance);
                            list.push((ad, now));
                        }
                        Some(secondscrn_discovery::BrowseEvent::Lost { instance }) => {
                            list.retain(|(a, _)| a.instance != instance)
                        }
                        None => {}
                    }
                    // Forget hosts not re-announced for a while.
                    list.retain(|(_, seen)| now.duration_since(*seen) < Duration::from_secs(300));
                    drop(list);
                    inner.publish();
                }
                lock(&inner.discovered).clear();
                inner.publish();
            })
            .expect("spawn discovery thread");
    }

    pub fn stop_discovery(&self) {
        if let Some(stop) = lock(&self.inner.discovery).take() {
            stop.store(true, Ordering::Relaxed);
        }
    }

    pub fn export_diagnostics(&self) -> String {
        let snap = self.snapshot();
        let logs = self.inner.logs.as_ref().map(|l| l.recent(2000)).unwrap_or_default();
        secondscrn_diagnostics::export_report(
            vec![
                ("app", serde_json::json!({ "role": "client", "version": self.inner.app_version })),
                ("device", serde_json::to_value(self.inner.platform.device_info()).unwrap_or_default()),
                ("state", serde_json::to_value(&snap).unwrap_or_default()),
            ],
            &logs,
        )
    }

    pub fn shutdown(&self) {
        self.stop_discovery();
        self.disconnect_inner(false);
        if let Some(rt) = lock(&self.runtime).take() {
            std::thread::sleep(Duration::from_millis(200));
            rt.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

impl Drop for ClientEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl ClientInner {
    fn send_to_session(&self, msg: ClientMessage) {
        if let Some(tx) = lock(&self.session_tx).as_ref() {
            let _ = tx.send(msg);
        }
    }

    fn save_settings(&self) {
        let s = lock(&self.settings).clone();
        if let Err(e) = settings::save(&self.data_dir.join("settings.json"), &s) {
            tracing::error!(error = %e, "failed to save settings");
        }
    }

    fn set_phase(&self, phase: ClientPhase) {
        {
            let mut st = lock(&self.state);
            if matches!(phase, ClientPhase::Idle | ClientPhase::Failed { .. }) {
                st.stream = None;
                st.ui_scale = None;
                st.metrics = ClientMetrics::default();
            }
            st.phase = Some(phase);
        }
        self.publish();
    }

    fn publish(&self) {
        let settings = lock(&self.settings).clone();
        let discovered = lock(&self.discovered)
            .iter()
            .filter_map(|(ad, _)| {
                Some(DiscoveredHost {
                    name: ad.name.clone(),
                    address: ad.addresses.first()?.to_string(),
                    port: ad.port,
                    fingerprint: ad.fingerprint.clone(),
                    paired: settings.known_host(&ad.fingerprint).is_some(),
                })
            })
            .collect();
        let display = *lock(&self.display);
        let st = lock(&self.state);
        let snap = ClientSnapshot {
            phase: st.phase.clone().unwrap_or(ClientPhase::Idle),
            host: st.host.clone(),
            stream: st.stream.clone(),
            metrics: st.metrics.clone(),
            discovered,
            known_hosts: settings.known_hosts.clone(),
            decoders: lock(&self.decoders).clone(),
            settings,
            device_fingerprint_short: self.identity.fingerprint().short(),
            resolutions: display.map(|d| d.resolution_options()).unwrap_or_default(),
            frame_rates: display.map(|d| d.frame_rate_options()).unwrap_or_default(),
            ui_scale: st.ui_scale.clone(),
        };
        drop(st);
        self.snapshot_tx.send_if_modified(|old| {
            if *old != snap {
                *old = snap;
                true
            } else {
                false
            }
        });
    }
}

enum End {
    /// The PC ended the session (user action there); don't reconnect.
    HostClosed,
}

impl SessionError {
    /// Errors where retrying cannot help without user action.
    fn is_fatal(&self) -> bool {
        match self {
            SessionError::Rejected(r) => !matches!(r, RejectReason::InternalError | RejectReason::DisplayUnavailable),
            SessionError::HostIdentityChanged
            | SessionError::PairingFailed
            | SessionError::InvalidInput(_)
            | SessionError::VersionMismatch { .. } => true,
            _ => false,
        }
    }

    fn user_message(&self) -> String {
        match self {
            SessionError::VersionMismatch { host, tablet } => {
                match secondscrn_protocol::compare_versions(tablet, host) {
                    Some(std::cmp::Ordering::Less) => format!(
                        "The PC runs SecondScrn {host}, this tablet {tablet}. Install SecondScrn {host} on this tablet."
                    ),
                    Some(std::cmp::Ordering::Greater) => format!(
                        "The PC runs SecondScrn {host}, this tablet {tablet}. Install SecondScrn {tablet} on the PC \
                         (Settings → Check now)."
                    ),
                    _ => {
                        format!("The PC runs SecondScrn {host}, this tablet {tablet}. Both must run the same version.")
                    }
                }
            }
            SessionError::Rejected(RejectReason::IncompatibleVersion) => "The PC runs another SecondScrn version. \
                Update both apps to the same version."
                .into(),
            SessionError::Rejected(RejectReason::NotPaired) => {
                "This tablet is no longer paired with the PC. Pair again to connect.".into()
            }
            SessionError::Rejected(r) => {
                let mut s = r.to_string();
                if let Some(f) = s.get_mut(0..1) {
                    f.make_ascii_uppercase();
                }
                s + "."
            }
            SessionError::HostIdentityChanged => "The PC's identity changed. If SecondScrn was reinstalled on the PC, \
                forget this PC and pair again. Otherwise someone may be impersonating it."
                .into(),
            SessionError::PairingFailed => "The code entered on the PC did not match. Try pairing again.".into(),
            SessionError::Timeout(what) => format!("Timed out ({what})."),
            SessionError::Io(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                "The PC refused the connection. Is SecondScrn running on it?".into()
            }
            other => other.to_string(),
        }
    }
}

async fn connection_loop(inner: Arc<ClientInner>, target: ConnectTarget, mut cancel: watch::Receiver<bool>) {
    let mut backoff = Backoff::default();
    let mut target = target;
    loop {
        let mut streamed = false;
        let outcome = tokio::select! {
            r = run_connection(&inner, &mut target, &mut streamed) => Some(r),
            _ = cancel.wait_for(|c| *c) => None,
        };
        *lock(&inner.session_tx) = None;
        let Some(outcome) = outcome else {
            return; // cancelled by the user
        };
        match outcome {
            Ok(End::HostClosed) => {
                inner.set_phase(ClientPhase::Failed {
                    message: "The PC ended the session.".into(),
                    needs_pairing: false,
                    update_to: None,
                });
                return;
            }
            Err(e) => {
                tracing::info!(error = %e, "connection ended");
                let paired = target.fingerprint.is_some();
                if e.is_fatal() || !paired {
                    let needs_pairing = matches!(
                        e,
                        SessionError::Rejected(RejectReason::NotPaired) | SessionError::HostIdentityChanged
                    );
                    // This app is the older one: offer exactly the PC's version.
                    let update_to = match &e {
                        SessionError::VersionMismatch { host, tablet }
                            if secondscrn_protocol::compare_versions(tablet, host)
                                == Some(std::cmp::Ordering::Less) =>
                        {
                            Some(host.clone())
                        }
                        _ => None,
                    };
                    inner.set_phase(ClientPhase::Failed { message: e.user_message(), needs_pairing, update_to });
                    return;
                }
                if streamed {
                    backoff.reset();
                }
                let delay = backoff.next_delay();
                inner.set_phase(ClientPhase::Reconnecting {
                    attempt: backoff.attempt(),
                    retry_in_ms: delay.as_millis() as u64,
                });
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel.wait_for(|c| *c) => return,
                }
            }
        }
    }
}

type Tls = tokio_rustls::client::TlsStream<TcpStream>;
type ClientChannel = ControlChannel<Tls, HostMessage>;

async fn expect<F, T>(
    ch: &mut ClientChannel,
    timeout: Duration,
    what: &'static str,
    mut f: F,
) -> Result<T, SessionError>
where
    F: FnMut(HostMessage) -> Option<T>,
{
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match ch.recv_timeout(remaining, what).await? {
            HostMessage::Rejected { reason } => return Err(SessionError::Rejected(reason)),
            HostMessage::Ping { nonce } => ch.send(&ClientMessage::Pong { nonce }).await?,
            HostMessage::Goodbye { .. } => return Err(SessionError::ConnectionClosed),
            other => {
                if let Some(v) = f(other) {
                    return Ok(v);
                }
            }
        }
    }
}

async fn run_connection(
    inner: &Arc<ClientInner>,
    target: &mut ConnectTarget,
    streamed: &mut bool,
) -> Result<End, SessionError> {
    inner.set_phase(ClientPhase::Connecting);
    let addr: SocketAddr =
        tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::lookup_host((target.address.as_str(), target.port)))
            .await
            .map_err(|_| SessionError::Timeout("resolving the PC's address"))??
            .next()
            .ok_or(SessionError::InvalidInput("address"))?;
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| SessionError::Timeout("connecting to the PC"))??;
    let _ = tcp.set_nodelay(true);

    let known: Option<KnownHost> =
        target.fingerprint.as_deref().and_then(|fp| lock(&inner.settings).known_host(fp).cloned());
    let pinned = known.as_ref().and_then(|k| Fingerprint::from_hex(&k.fingerprint));
    if let Some(k) = &known {
        // Shown in errors such as "identity changed" before the handshake completes.
        lock(&inner.state).host = Some(HostSummary {
            name: k.name.clone(),
            fingerprint: k.fingerprint.clone(),
            fingerprint_short: pinned.map(|f| f.short()).unwrap_or_default(),
            address: target.address.clone(),
            port: target.port,
        });
    }
    let connector = TlsConnector::from(Arc::new(client_config(&inner.identity, pinned, ALPN)?));
    let tls = match tokio::time::timeout(STEP_TIMEOUT, connector.connect(server_name(), tcp)).await {
        Err(_) => return Err(SessionError::Timeout("TLS handshake")),
        Ok(Err(e)) => {
            let is_pin_failure = e
                .get_ref()
                .and_then(|i| i.downcast_ref::<rustls::Error>())
                .is_some_and(|r| matches!(r, rustls::Error::InvalidCertificate(_)));
            return Err(if is_pin_failure && pinned.is_some() { SessionError::HostIdentityChanged } else { e.into() });
        }
        Ok(Ok(t)) => t,
    };
    let (host_fp, exporter) = {
        let (_, conn) = tls.get_ref();
        let fp =
            peer_fingerprint(conn.peer_certificates()).ok_or(SessionError::Protocol("missing host certificate"))?;
        let mut ekm = [0u8; 32];
        conn.export_keying_material(&mut ekm, PAIRING_EXPORTER_LABEL, None)
            .map_err(|_| SessionError::Protocol("TLS exporter unavailable"))?;
        (fp, ekm)
    };
    let mut ch: ClientChannel = ControlChannel::new(tls);
    let device = {
        let mut d = inner.platform.device_info();
        if let Some(n) = lock(&inner.settings).device_name.clone().filter(|n| !n.trim().is_empty()) {
            d.name = n;
        }
        d.app_version = inner.app_version.clone();
        d
    };
    let intent = if pinned.is_some() { Intent::Connect } else { Intent::Pair };
    ch.send(&ClientMessage::Hello { protocol_version: PROTOCOL_VERSION, device, intent }).await?;
    let ours = inner.app_version.clone();
    let host_info = expect(&mut ch, STEP_TIMEOUT, "host hello", |m| match m {
        HostMessage::Hello { host, protocol_version }
            if secondscrn_protocol::versions_compatible(
                (&ours, PROTOCOL_VERSION),
                (&host.app_version, protocol_version),
            ) =>
        {
            Some(Ok(host))
        }
        HostMessage::Hello { host, .. } => {
            Some(Err(SessionError::VersionMismatch { host: host.app_version, tablet: ours.clone() }))
        }
        _ => None,
    })
    .await??;

    if intent == Intent::Pair {
        expect(&mut ch, STEP_TIMEOUT, "pairing", |m| matches!(m, HostMessage::PairPending).then_some(())).await?;
        let pin = generate_pin();
        let (pairing, msg) = ClientPairing::start(&pin, host_fp, inner.identity.fingerprint());
        {
            let mut st = lock(&inner.state);
            st.host = Some(HostSummary {
                name: host_info.name.clone(),
                fingerprint: host_fp.to_hex(),
                fingerprint_short: host_fp.short(),
                address: target.address.clone(),
                port: target.port,
            });
        }
        inner.set_phase(ClientPhase::Pairing { pin });
        ch.send(&ClientMessage::PairStart { spake: msg }).await?;
        let (spake, mac) = expect(&mut ch, PAIRING_TIMEOUT, "PIN entry on the PC", |m| match m {
            HostMessage::PairChallenge { spake, mac } => Some((spake, mac)),
            _ => None,
        })
        .await?;
        let confirm = pairing.finish(&spake, &mac, &exporter).map_err(|_| SessionError::PairingFailed)?;
        ch.send(&ClientMessage::PairConfirm { mac: confirm }).await?;
        expect(&mut ch, STEP_TIMEOUT, "pairing confirmation", |m| matches!(m, HostMessage::PairComplete).then_some(()))
            .await?;
        tracing::info!(host = %host_info.name, fingerprint = %host_fp, "paired with host");
        target.fingerprint = Some(host_fp.to_hex());
    }

    // Remember the host and its current address.
    {
        let mut s = lock(&inner.settings);
        let auto = s.known_host(&host_fp.to_hex()).map(|h| h.auto_reconnect).unwrap_or(true);
        s.upsert_host(KnownHost {
            fingerprint: host_fp.to_hex(),
            name: host_info.name.clone(),
            last_address: target.address.clone(),
            port: target.port,
            auto_reconnect: auto,
            last_connected: Some(now_unix()),
        });
    }
    inner.save_settings();
    {
        let mut st = lock(&inner.state);
        st.host = Some(HostSummary {
            name: host_info.name.clone(),
            fingerprint: host_fp.to_hex(),
            fingerprint_short: host_fp.short(),
            address: target.address.clone(),
            port: target.port,
        });
    }
    inner.set_phase(ClientPhase::Negotiating);

    let display = lock(&inner.display).unwrap_or(DisplayRequest {
        width: 1920,
        height: 1200,
        refresh_hz: 60,
        dpi: 240,
        orientation: Orientation::Landscape,
    });
    // Decoder probing runs at startup; wait briefly if it hasn't finished.
    let mut decoders = lock(&inner.decoders).clone();
    for _ in 0..50 {
        if !decoders.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        decoders = lock(&inner.decoders).clone();
    }
    let (preset, preferred_codec) = {
        let s = lock(&inner.settings);
        (s.preset, s.preferred_codec)
    };
    ch.send(&ClientMessage::SessionRequest { display, decoders, preset, preferred_codec }).await?;
    let mut stream = expect(&mut ch, Duration::from_secs(30), "session setup", |m| match m {
        HostMessage::SessionAccepted { stream } => Some(stream),
        _ => None,
    })
    .await?;
    let offer = expect(&mut ch, STEP_TIMEOUT, "SDP offer", |m| match m {
        HostMessage::SdpOffer { sdp } => Some(sdp),
        _ => None,
    })
    .await?;
    let (mut media, answer) = accept_offer(
        ReceiverConfig { codec: stream.codec, interfaces: inner.interfaces.clone(), host_hint: Some(addr.ip()) },
        &offer,
    )
    .await?;
    ch.send(&ClientMessage::SdpAnswer { sdp: answer }).await?;
    lock(&inner.state).stream = Some(stream.clone());
    inner.publish();

    let (session_tx, mut session_rx) = mpsc::unbounded_channel();
    *lock(&inner.session_tx) = Some(session_tx);
    let (worker_err_tx, mut worker_errors) = mpsc::unbounded_channel();
    let mut worker = DecodeWorker::spawn(inner.platform.clone(), stream.clone(), worker_err_tx.clone());

    let mut ping = tokio::time::interval(PING_INTERVAL);
    let mut last_rx = Instant::now();
    let mut nonce = 0u64;
    let mut frames_received = 0u64;
    let mut late_frames = 0u64;
    let mut odd_frames = 0u64;
    let mut behind_seen = 0u64;
    let mut last_slowdown: Option<Instant> = None;
    let mut warnings = WarningTracker::default();
    let mut last_rtp: Option<u64> = None;
    // Rendered-frame counts of recent stats ticks, for the render rate.
    let mut renders: VecDeque<(u64, Instant)> = VecDeque::new();
    let mut stats_tick = tokio::time::interval(STATS_INTERVAL);
    let mut last_bytes: Option<(u64, Instant)> = None;
    let mut metrics = ClientMetrics::default();
    let mut ping_sent: Option<(u64, Instant)> = None;
    let mut rtt_avg = secondscrn_diagnostics::Ewma::new(0.3);
    let mut rtt_observed: Option<Instant> = None;
    // Raw (pre-retransmission) loss over few packets is noisy; smooth it.
    let mut loss_avg = secondscrn_diagnostics::Ewma::new(0.2);
    // Latency feedback: frames received since the last report.
    let clock = Instant::now();
    let mut timings: Vec<FrameTiming> = Vec::new();
    let mut feedback_tick = tokio::time::interval(FEEDBACK_INTERVAL);
    let mut last_keyframe_request: Option<Instant> = None;

    let result = loop {
        tokio::select! {
            msg = ch.rx.recv() => {
                let msg = match msg {
                    None => break Err(SessionError::ConnectionClosed),
                    Some(Err(e)) => break Err(e.into()),
                    Some(Ok(m)) => m,
                };
                last_rx = Instant::now();
                match msg {
                    HostMessage::Ping { nonce } => ch.send(&ClientMessage::Pong { nonce }).await?,
                    // A WebRTC receiver doesn't measure RTT; the control
                    // channel's ping round trip is a good proxy on a LAN.
                    HostMessage::Pong { nonce } => {
                        if let Some((_, t)) = ping_sent.take().filter(|(n, _)| *n == nonce) {
                            rtt_avg.add(t.elapsed().as_secs_f32() * 1000.0);
                            metrics.rtt_ms = Some(rtt_avg.get());
                            rtt_observed = Some(Instant::now());
                        }
                    }
                    HostMessage::StreamChanged { stream: s } => {
                        tracing::info!(width = s.width, height = s.height, fps = s.max_fps, "stream reconfigured");
                        warnings.reset(Instant::now());
                        metrics.warning = None;
                        lock(&inner.state).metrics.warning = None;
                        behind_seen = worker.times_behind();
                        // New format or frame rate: the decoder is set up for
                        // both (the rate decides its low-latency mode).
                        if (s.codec, s.width, s.height, s.max_fps)
                            != (stream.codec, stream.width, stream.height, stream.max_fps)
                        {
                            worker.reconfigure(s.clone());
                        }
                        stream = s;
                        lock(&inner.state).stream = Some(stream.clone());
                        inner.publish();
                    }
                    HostMessage::Recovery { rtp_time } => worker.recovery(rtp_time),
                    HostMessage::NetworkLimited { limited } => {
                        warnings.network_limited(Instant::now(), limited);
                        metrics.warning = warnings.current(Instant::now());
                        lock(&inner.state).metrics.warning = metrics.warning;
                        inner.publish();
                    }
                    HostMessage::UiScale { current, options } => {
                        lock(&inner.state).ui_scale = Some(UiScale { current, options });
                        inner.publish();
                    }
                    HostMessage::Goodbye { reconnect: false } => break Ok(End::HostClosed),
                    HostMessage::Goodbye { reconnect: true } => break Err(SessionError::ConnectionClosed),
                    HostMessage::Rejected { reason } => break Err(SessionError::Rejected(reason)),
                    _ => {}
                }
            }
            ev = media.events.recv() => {
                let Some(ev) = ev else { break Err(SessionError::Transport("media transport stopped".into())) };
                match ev {
                    TransportEvent::Frame(f) => {
                        frames_received += 1;
                        timings.push(FrameTiming {
                            rtp_time: f.rtp_time,
                            arrival_us: f.arrival.saturating_duration_since(clock).as_micros() as u64,
                            bytes: f.data.len().min(u32::MAX as usize) as u32,
                        });
                        // Diagnostics: frames must arrive once, in order.
                        if let Some(prev) = last_rtp && f.rtp_time <= prev {
                            odd_frames += 1;
                            if odd_frames.is_power_of_two() {
                                tracing::warn!(odd_frames, rtp = f.rtp_time, prev, len = f.data.len(), key = f.keyframe, "frame with repeated or older timestamp");
                            }
                        }
                        last_rtp = Some(f.rtp_time);
                        if !worker.push(f) {
                            late_frames += 1;
                            media.request_keyframe();
                        }
                        // Frames are queuing up: ask the PC to send fewer
                        // before a backlog forces a resync (a freeze).
                        if worker.queued() >= slowdown_queue(stream.max_fps)
                            && last_slowdown.is_none_or(|t: Instant| t.elapsed() > Duration::from_millis(100))
                        {
                            last_slowdown = Some(Instant::now());
                            ch.send(&ClientMessage::SlowDown).await?;
                        }
                    }
                    TransportEvent::Connected { .. } => {
                        warnings.reset(Instant::now());
                        metrics.warning = None;
                        lock(&inner.state).metrics.warning = None;
                        *streamed = true;
                        inner.set_phase(ClientPhase::Streaming);
                    }
                    TransportEvent::Interrupted => {
                        warnings = WarningTracker::default();
                        metrics.warning = None;
                        lock(&inner.state).metrics.warning = None;
                        inner.set_phase(ClientPhase::Interrupted);
                    }
                    TransportEvent::Resumed => {
                        warnings.reset(Instant::now());
                        metrics.warning = None;
                        lock(&inner.state).metrics.warning = None;
                        behind_seen = worker.times_behind();
                        media.request_keyframe();
                        inner.set_phase(ClientPhase::Streaming);
                    }
                    TransportEvent::Stats(s) => {
                        let now = Instant::now();
                        if let Some((b, t)) = last_bytes {
                            let dt = now.duration_since(t).as_secs_f64();
                            if dt > 0.1 {
                                metrics.receive_rate_bps = ((s.bytes_received.saturating_sub(b)) as f64 * 8.0 / dt) as u64;
                            }
                        }
                        last_bytes = Some((s.bytes_received, now));
                        if let Some(rtt) = s.rtt {
                            metrics.rtt_ms = Some(rtt.as_secs_f32() * 1000.0);
                            rtt_observed = Some(now);
                        }
                        if let Some(l) = s.loss {
                            loss_avg.add(l);
                            metrics.loss = Some(loss_avg.get());
                        }
                        // Count transport reports, never repeated readings of
                        // an old metric on the control-channel timer.
                        let fresh_rtt = metrics.rtt_ms.filter(|_| {
                            rtt_observed.is_some_and(|t| now.duration_since(t) <= PING_INTERVAL * 2)
                        });
                        warnings.network_report(now, s.loss, fresh_rtt);
                    }
                    TransportEvent::Closed { reason } => break Err(SessionError::Transport(reason)),
                    TransportEvent::BitrateEstimate(_) | TransportEvent::KeyframeRequested => {}
                }
            }
            Some(msg) = session_rx.recv() => {
                let bye = matches!(msg, ClientMessage::Goodbye);
                ch.send(&msg).await?;
                if bye {
                    return Ok(End::HostClosed);
                }
            }
            Some(err) = worker_errors.recv() => {
                tracing::warn!(error = %err, "decoder failed, restarting");
                worker = DecodeWorker::spawn(inner.platform.clone(), stream.clone(), worker_err_tx.clone());
                warnings.reset(Instant::now());
                metrics.warning = None;
                lock(&inner.state).metrics.warning = None;
                inner.publish();
                behind_seen = 0;
                media.request_keyframe();
            }
            _ = ping.tick() => {
                if last_rx.elapsed() > PEER_TIMEOUT {
                    break Err(SessionError::Timeout("the PC stopped responding"));
                }
                // Something can't keep up: warn (with a recommendation in the
                // UI) instead of changing the user's settings.
                let now = Instant::now();
                let behind = worker.times_behind();
                if behind > behind_seen {
                    warnings.decoder_behind(now);
                    behind_seen = behind;
                }
                metrics.warning = warnings.current(now);
                nonce += 1;
                ping_sent = Some((nonce, Instant::now()));
                ch.send(&ClientMessage::Ping { nonce }).await?;
            }
            _ = feedback_tick.tick(), if !timings.is_empty() || worker.wants_keyframe() => {
                // The decoder lost its references: tell the PC right away (it
                // answers with a recovery frame or a keyframe), and again only
                // if that doesn't arrive.
                if last_keyframe_request.is_none_or(|t: Instant| t.elapsed() >= KEYFRAME_REQUEST_REPEAT)
                    && worker.take_keyframe_request()
                {
                    last_keyframe_request = Some(Instant::now());
                    media.request_keyframe();
                    ch.send(&ClientMessage::RequestKeyframe).await?;
                }
                if !timings.is_empty() {
                    let frames = std::mem::take(&mut timings);
                    ch.send(&ClientMessage::FrameTimings { frames, intact: worker.intact() }).await?;
                }
            }
            _ = stats_tick.tick() => {
                let ds = worker.stats();
                let now = Instant::now();
                renders.push_back((ds.frames_rendered, now));
                // Rate over the oldest sample within the window: current
                // within a fraction of a second, not a ping interval.
                while renders.len() > 2 && now.duration_since(renders[1].1) >= RENDER_RATE_WINDOW {
                    renders.pop_front();
                }
                if let Some(&(frames, t)) = renders.front() {
                    let dt = now.duration_since(t).as_secs_f32();
                    if dt > 0.0 {
                        metrics.render_fps = ds.frames_rendered.saturating_sub(frames) as f32 / dt;
                    }
                }
                metrics.decode_ms_avg = ds.decode_ms_avg;
                metrics.frames_received = frames_received;
                metrics.frames_decoded = ds.frames_decoded;
                metrics.frames_dropped = ds.frames_dropped + late_frames;
                metrics.health = LinkHealth::assess(metrics.rtt_ms, metrics.loss, metrics.render_fps, stream.max_fps);
                ch.send(&ClientMessage::Stats { stats: ClientStats {
                    frames_received,
                    frames_decoded: ds.frames_decoded,
                    frames_rendered: ds.frames_rendered,
                    frames_dropped: ds.frames_dropped,
                    decode_time_ms_avg: ds.decode_ms_avg,
                    render_fps: metrics.render_fps,
                    late_frames,
                    decode_queue_max: worker.take_queue_max(),
                    stutters: ds.stutters,
                }}).await?;
                lock(&inner.state).metrics = metrics.clone();
                inner.publish();
            }
        }
    };
    drop(worker);
    result
}

enum WorkerMsg {
    Frame(ReceivedFrame),
    /// The frame with this RTP time decodes after a loss (it predicts only
    /// from a reference picture this decoder has).
    Recovery(u64),
    Reconfigure(StreamConfig),
}

/// Runs the platform decoder on a dedicated thread.
struct DecodeWorker {
    tx: Option<std::sync::mpsc::SyncSender<WorkerMsg>>,
    /// Frames waiting for the decoder, and the most since the last report.
    queued: Arc<AtomicUsize>,
    queued_max: Arc<AtomicUsize>,
    /// Times the decoder fell behind and had to skip to a keyframe.
    behind: Arc<AtomicU64>,
    need_keyframe: Arc<AtomicBool>,
    stats: Arc<Mutex<DecoderStats>>,
    /// First and latest RTP time decoded since the last loss.
    intact: Arc<Mutex<Option<(u64, u64)>>>,
}

/// Frames kept while waiting for a recovery frame (it may arrive before the
/// PC's announcement does), at most this many and this old.
const HELD_FRAMES: usize = 64;
const HELD_MAX_AGE: Duration = Duration::from_millis(500);

impl DecodeWorker {
    fn spawn(
        platform: Arc<dyn ClientPlatform>,
        config: StreamConfig,
        errors: mpsc::UnboundedSender<PlatformError>,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<WorkerMsg>(DECODE_QUEUE);
        let need_keyframe = Arc::new(AtomicBool::new(true));
        let stats = Arc::new(Mutex::new(DecoderStats::default()));
        let intact = Arc::new(Mutex::new(None));
        let (nk, st, run) = (need_keyframe.clone(), stats.clone(), intact.clone());
        let queued = Arc::new(AtomicUsize::new(0));
        let q = queued.clone();
        let behind_count = Arc::new(AtomicU64::new(0));
        let bc = behind_count.clone();
        std::thread::Builder::new()
            .name("secondscrn-decode".into())
            .spawn(move || {
                let mut config = config;
                let mut decoder = match platform.create_decoder(&config) {
                    Ok(d) => d,
                    Err(e) => {
                        let _ = errors.send(e);
                        return;
                    }
                };
                let mut waiting_for_key = true;
                // While waiting: frames after the loss, and the announced
                // recovery frame.
                let mut held: VecDeque<ReceivedFrame> = VecDeque::new();
                let mut recovery: Option<u64> = None;
                // Decodes `f`; false if the decoder must stop.
                let decode = |f: &ReceivedFrame,
                              waiting_for_key: &mut bool,
                              decoder: &mut Box<dyn crate::platform::VideoDecoder>| {
                    let starts_run = *waiting_for_key;
                    match decoder.decode(f) {
                        Ok(DecodeOutcome::Ok) => {
                            if starts_run {
                                // Requests made while waiting are answered.
                                nk.store(false, Ordering::Relaxed);
                            }
                            *waiting_for_key = false;
                            let mut r = lock(&run);
                            *r = Some((
                                if starts_run { f.rtp_time } else { r.map_or(f.rtp_time, |(a, _)| a) },
                                f.rtp_time,
                            ));
                        }
                        Ok(DecodeOutcome::NeedKeyframe) => {
                            *waiting_for_key = true;
                            *lock(&run) = None;
                            nk.store(true, Ordering::Relaxed);
                        }
                        Err(e) => {
                            let _ = errors.send(e);
                            return false;
                        }
                    }
                    *lock(&st) = decoder.stats();
                    true
                };
                while let Ok(msg) = rx.recv() {
                    match msg {
                        WorkerMsg::Recovery(rtp) => {
                            if !waiting_for_key {
                                continue;
                            }
                            held.retain(|f| f.rtp_time >= rtp);
                            tracing::debug!(
                                rtp,
                                held = held.len(),
                                arrived = held.front().is_some_and(|f| f.rtp_time == rtp),
                                "recovery announced"
                            );
                            if held.front().is_some_and(|f| f.rtp_time == rtp) {
                                // It arrived first: decode it and what followed.
                                for f in std::mem::take(&mut held) {
                                    if !decode(&f, &mut waiting_for_key, &mut decoder) {
                                        return;
                                    }
                                }
                            } else {
                                recovery = Some(rtp);
                            }
                        }
                        WorkerMsg::Reconfigure(c) => {
                            config = c;
                            drop(decoder);
                            decoder = match platform.create_decoder(&config) {
                                Ok(d) => d,
                                Err(e) => {
                                    let _ = errors.send(e);
                                    return;
                                }
                            };
                            waiting_for_key = true;
                            *lock(&run) = None;
                            held.clear();
                            recovery = None;
                            nk.store(true, Ordering::Relaxed);
                        }
                        WorkerMsg::Frame(f) => {
                            let behind = q.fetch_sub(1, Ordering::Relaxed) - 1;
                            // Never let latency build up: if frames waited too
                            // long, skip to the next keyframe instead.
                            if behind > 0 && !f.keyframe && f.arrival.elapsed() > MAX_DECODE_WAIT {
                                if !waiting_for_key {
                                    bc.fetch_add(1, Ordering::Relaxed);
                                    tracing::warn!(
                                        behind,
                                        waited_ms = f.arrival.elapsed().as_millis() as u64,
                                        "decoder fell behind, resyncing"
                                    );
                                }
                                waiting_for_key = true;
                                *lock(&run) = None;
                                nk.store(true, Ordering::Relaxed);
                                continue;
                            }
                            // After a gap, decoding would reference missing
                            // frames and smear text; hold the last good
                            // picture until a keyframe or a recovery frame.
                            if !f.contiguous && !f.keyframe {
                                waiting_for_key = true;
                                *lock(&run) = None;
                            }
                            if f.keyframe {
                                held.clear();
                                recovery = None;
                            } else if waiting_for_key {
                                if recovery == Some(f.rtp_time) {
                                    tracing::debug!(rtp = f.rtp_time, "recovery frame arrived");
                                    recovery = None;
                                    held.clear();
                                } else {
                                    nk.store(true, Ordering::Relaxed);
                                    held.retain(|h| h.arrival.elapsed() < HELD_MAX_AGE);
                                    if held.len() >= HELD_FRAMES {
                                        held.pop_front();
                                    }
                                    held.push_back(f);
                                    continue;
                                }
                            }
                            if !decode(&f, &mut waiting_for_key, &mut decoder) {
                                return;
                            }
                        }
                    }
                }
            })
            .expect("spawn decode thread");
        DecodeWorker {
            tx: Some(tx),
            queued,
            queued_max: Arc::new(AtomicUsize::new(0)),
            behind: behind_count,
            need_keyframe,
            stats,
            intact,
        }
    }

    fn recovery(&self, rtp: u64) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(WorkerMsg::Recovery(rtp));
        }
    }

    /// First and latest RTP time decoded since the last loss.
    fn intact(&self) -> Option<(u64, u64)> {
        *lock(&self.intact)
    }

    /// Returns false if the frame was dropped because the decoder is behind.
    fn push(&self, frame: ReceivedFrame) -> bool {
        let Some(tx) = &self.tx else { return false };
        let depth = self.queued.fetch_add(1, Ordering::Relaxed) + 1;
        self.queued_max.fetch_max(depth, Ordering::Relaxed);
        match tx.try_send(WorkerMsg::Frame(frame)) {
            Ok(()) => true,
            Err(_) => {
                self.queued.fetch_sub(1, Ordering::Relaxed);
                // Dropped frame breaks the reference chain; resync on a keyframe.
                self.need_keyframe.store(true, Ordering::Relaxed);
                false
            }
        }
    }

    fn reconfigure(&self, config: StreamConfig) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(WorkerMsg::Reconfigure(config));
        }
    }

    fn queued(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    fn times_behind(&self) -> u64 {
        self.behind.load(Ordering::Relaxed)
    }

    fn take_queue_max(&self) -> u32 {
        self.queued_max.swap(self.queued.load(Ordering::Relaxed), Ordering::Relaxed) as u32
    }

    fn wants_keyframe(&self) -> bool {
        self.need_keyframe.load(Ordering::Relaxed)
    }

    fn take_keyframe_request(&self) -> bool {
        self.need_keyframe.swap(false, Ordering::Relaxed)
    }

    fn stats(&self) -> DecoderStats {
        *lock(&self.stats)
    }
}

impl Drop for DecodeWorker {
    fn drop(&mut self) {
        // Closing the channel ends the thread, which drops the decoder.
        self.tx.take();
    }
}

#[cfg(test)]
mod warning_tests {
    use super::*;

    #[test]
    fn warnings_require_sustained_evidence_after_grace() {
        let start = Instant::now();
        let at = |s| start + Duration::from_secs(s);
        let mut w = WarningTracker::default();
        w.reset(start);
        for s in 0..10 {
            w.network_report(at(s), Some(0.5), Some(300.0));
            w.decoder_behind(at(s));
            assert_eq!(w.current(at(s)), None);
        }
        // Healthy reports break the streak; a single spike is insufficient.
        w.network_report(at(10), Some(0.1), None);
        w.network_report(at(12), Some(0.0), Some(20.0));
        w.network_report(at(14), Some(0.1), None);
        w.network_report(at(16), Some(0.1), None);
        assert_eq!(w.current(at(16)), None);
        w.network_report(at(18), Some(0.1), None);
        assert_eq!(w.current(at(18)), Some(PerformanceWarning::Network));
        // Reading an old result never extends its lifetime.
        assert_eq!(w.current(at(33)), None);
        w.decoder_behind(at(34));
        assert_eq!(w.current(at(34)), None);
        w.decoder_behind(at(36));
        assert_eq!(w.current(at(36)), Some(PerformanceWarning::Decoder));
        // Reconfiguration and resume both use reset: clear all history.
        w.reset(at(37));
        assert_eq!(w.current(at(37)), None);
        w.decoder_behind(at(38));
        w.network_report(at(38), Some(0.5), None);
        assert_eq!(w.current(at(48)), None);
    }
}
