//! Client (tablet) engine: discovery, pairing, connection, decoding and
//! automatic reconnection. Publishes [`ClientSnapshot`]s for the UI.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use secondscrn_diagnostics::LogBuffer;
use secondscrn_protocol::{
    ALPN, ClientMessage, ClientStats, CodecCapability, DisplayRequest, HostMessage, InputEvent, Intent, Orientation,
    PROTOCOL_VERSION, PenAction, PointerAction, PointerButton, QualityPreset, RejectReason, StreamConfig, TouchContact,
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
use crate::channel::{ControlChannel, PEER_TIMEOUT, PING_INTERVAL, STEP_TIMEOUT};
use crate::platform::{ClientPlatform, DecodeOutcome, DecoderStats, PlatformError, UiScale};
use crate::settings::{self, ClientSettings, KnownHost};
use crate::snapshot::{ClientMetrics, ClientPhase, ClientSnapshot, DiscoveredHost, HostSummary, LinkHealth};

/// How long the user has to type the PIN on the PC.
const PAIRING_TIMEOUT: Duration = Duration::from_secs(130);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
/// Frames queued for the decoder. Frames arrive in bursts (e.g. after a
/// retransmission) and dropping one breaks the reference chain, which costs a
/// keyframe: at tablet resolution a burst large enough to cause the next
/// loss. So every frame is decoded and only rendering skips stale pictures
/// (see the platform decoder); this bound is only a safety net (~1 s).
const DECODE_QUEUE: usize = 60;
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
            SessionError::HostIdentityChanged | SessionError::PairingFailed | SessionError::InvalidInput(_) => true,
            _ => false,
        }
    }

    fn user_message(&self) -> String {
        match self {
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
                    inner.set_phase(ClientPhase::Failed { message: e.user_message(), needs_pairing });
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
    let host_info = expect(&mut ch, STEP_TIMEOUT, "host hello", |m| match m {
        HostMessage::Hello { host, protocol_version } if protocol_version == PROTOCOL_VERSION => Some(Ok(host)),
        HostMessage::Hello { .. } => Some(Err(SessionError::Rejected(RejectReason::IncompatibleVersion))),
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
    let mut last_rtp: Option<u64> = None;
    // Render rate from frame deltas between (2 s apart) stats ticks.
    let mut last_render: (u64, Instant) = (0, Instant::now());
    let mut last_bytes: Option<(u64, Instant)> = None;
    let mut metrics = ClientMetrics::default();
    let mut ping_sent: Option<(u64, Instant)> = None;
    let mut rtt_avg = secondscrn_diagnostics::Ewma::new(0.3);
    // Raw (pre-retransmission) loss over few packets is noisy; smooth it.
    let mut loss_avg = secondscrn_diagnostics::Ewma::new(0.2);

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
                        }
                    }
                    HostMessage::StreamChanged { stream: s } => {
                        tracing::info!(width = s.width, height = s.height, fps = s.max_fps, "stream reconfigured");
                        // The decoder only restarts for a new format; a new
                        // frame rate arrives in-band with the next keyframe.
                        if (s.codec, s.width, s.height) != (stream.codec, stream.width, stream.height) {
                            worker.reconfigure(s.clone());
                        }
                        stream = s;
                        lock(&inner.state).stream = Some(stream.clone());
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
                    }
                    TransportEvent::Connected { .. } => {
                        *streamed = true;
                        inner.set_phase(ClientPhase::Streaming);
                    }
                    TransportEvent::Interrupted => inner.set_phase(ClientPhase::Interrupted),
                    TransportEvent::Resumed => {
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
                        }
                        if let Some(l) = s.loss {
                            loss_avg.add(l);
                            metrics.loss = Some(loss_avg.get());
                        }
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
                media.request_keyframe();
            }
            _ = ping.tick() => {
                if last_rx.elapsed() > PEER_TIMEOUT {
                    break Err(SessionError::Timeout("the PC stopped responding"));
                }
                if worker.take_keyframe_request() {
                    media.request_keyframe();
                    ch.send(&ClientMessage::RequestKeyframe).await?;
                }
                nonce += 1;
                ping_sent = Some((nonce, Instant::now()));
                ch.send(&ClientMessage::Ping { nonce }).await?;
                let ds = worker.stats();
                let now = Instant::now();
                let dt = now.duration_since(last_render.1).as_secs_f32();
                if dt > 0.0 {
                    metrics.render_fps = ds.frames_rendered.saturating_sub(last_render.0) as f32 / dt;
                }
                last_render = (ds.frames_rendered, now);
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
    Reconfigure(StreamConfig),
}

/// Runs the platform decoder on a dedicated thread.
struct DecodeWorker {
    tx: Option<std::sync::mpsc::SyncSender<WorkerMsg>>,
    /// Frames waiting for the decoder, and the most since the last report.
    queued: Arc<AtomicUsize>,
    queued_max: Arc<AtomicUsize>,
    need_keyframe: Arc<AtomicBool>,
    stats: Arc<Mutex<DecoderStats>>,
}

impl DecodeWorker {
    fn spawn(
        platform: Arc<dyn ClientPlatform>,
        config: StreamConfig,
        errors: mpsc::UnboundedSender<PlatformError>,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<WorkerMsg>(DECODE_QUEUE);
        let need_keyframe = Arc::new(AtomicBool::new(true));
        let stats = Arc::new(Mutex::new(DecoderStats::default()));
        let (nk, st) = (need_keyframe.clone(), stats.clone());
        let queued = Arc::new(AtomicUsize::new(0));
        let q = queued.clone();
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
                while let Ok(msg) = rx.recv() {
                    match msg {
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
                            nk.store(true, Ordering::Relaxed);
                        }
                        WorkerMsg::Frame(f) => {
                            let behind = q.fetch_sub(1, Ordering::Relaxed) - 1;
                            // Never let latency build up: if frames waited too
                            // long, skip to the next keyframe instead.
                            if behind > 0 && !f.keyframe && f.arrival.elapsed() > MAX_DECODE_WAIT {
                                if !waiting_for_key {
                                    tracing::warn!(
                                        behind,
                                        waited_ms = f.arrival.elapsed().as_millis() as u64,
                                        "decoder fell behind, resyncing"
                                    );
                                }
                                waiting_for_key = true;
                                nk.store(true, Ordering::Relaxed);
                                continue;
                            }
                            // After a gap, decoding would reference missing
                            // frames and smear text; hold the last good
                            // picture until the requested keyframe arrives.
                            if !f.contiguous && !f.keyframe {
                                waiting_for_key = true;
                            }
                            if waiting_for_key && !f.keyframe {
                                nk.store(true, Ordering::Relaxed);
                                continue;
                            }
                            match decoder.decode(&f) {
                                Ok(DecodeOutcome::Ok) => waiting_for_key = false,
                                Ok(DecodeOutcome::NeedKeyframe) => {
                                    waiting_for_key = true;
                                    nk.store(true, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    let _ = errors.send(e);
                                    return;
                                }
                            }
                            *lock(&st) = decoder.stats();
                        }
                    }
                }
            })
            .expect("spawn decode thread");
        DecodeWorker { tx: Some(tx), queued, queued_max: Arc::new(AtomicUsize::new(0)), need_keyframe, stats }
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

    fn take_queue_max(&self) -> u32 {
        self.queued_max.swap(self.queued.load(Ordering::Relaxed), Ordering::Relaxed) as u32
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
