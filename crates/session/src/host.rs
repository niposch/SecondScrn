//! Host (PC) engine: accepts tablets, pairs them, negotiates and runs one
//! streaming session at a time, and publishes [`HostSnapshot`]s for the UI.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use secondscrn_diagnostics::LogBuffer;
use secondscrn_protocol::{
    ALPN, ClientMessage, CodecCapability, DeviceInfo, DisplayRequest, HostMessage, InputEvent, Intent,
    PROTOCOL_VERSION, QualityPreset, RejectReason, StreamConfig,
};
use secondscrn_security::pairing::{HostPairing, normalize_pin};
use secondscrn_security::tls::{PAIRING_EXPORTER_LABEL, peer_fingerprint, server_config};
use secondscrn_security::trust::{TrustStore, TrustedPeer, now_unix};
use secondscrn_security::{Fingerprint, Identity, SecretProtector};
use secondscrn_transport::{FrameSender, InterfacePolicy, PendingSender, SenderConfig, TransportEvent};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_rustls::TlsAcceptor;

use crate::SessionError;
use crate::adapt::{BitrateController, BitratePolicy};
use crate::channel::{ControlChannel, PEER_TIMEOUT, PING_INTERVAL, STEP_TIMEOUT};
use crate::negotiate::{NegotiationError, NegotiationInput, negotiate};
use crate::platform::{
    DisplayInfo, DisplayLease, DisplayPlan, DriverStatus, HostPlatform, PipelineSink, PlatformError, VideoPipeline,
};
use crate::settings::{self, DevicePreferences, HostSettings};
use crate::snapshot::{HostMetrics, HostSnapshot, LinkHealth, PairedDevice, PairingPrompt, SessionInfo, SessionPhase};

/// How long the PC user has to type the PIN.
const PAIRING_TIMEOUT: Duration = Duration::from_secs(120);
/// At most this many pairing attempts per [`PAIRING_WINDOW`].
const PAIRING_MAX_ATTEMPTS: usize = 5;
const PAIRING_WINDOW: Duration = Duration::from_secs(300);
/// Unauthenticated connections handled concurrently.
const MAX_PENDING_CONNECTIONS: usize = 8;
/// Pipeline restarts tolerated per session before giving up.
const MAX_PIPELINE_RESTARTS: u32 = 5;

pub struct HostConfig {
    pub data_dir: PathBuf,
    pub default_host_name: String,
    pub listen_addr: IpAddr,
    pub interfaces: InterfacePolicy,
    pub enable_mdns: bool,
    pub protector: Arc<dyn SecretProtector>,
    pub logs: Option<LogBuffer>,
    pub app_version: String,
}

enum PairDecision {
    Pin(String),
    Reject,
}

enum SessionCommand {
    Disconnect { reconnect: bool },
    SetPreset(QualityPreset),
    SetResolution(u32),
    SetFrameRate(u32),
}

/// Why the stream is about to change.
enum DisplayChange {
    /// Another frame rate: only the encoder restarts.
    FrameRate,
    /// The tablet rotated or the user picked another resolution: apply a new plan.
    Plan,
    /// The mode was changed outside SecondScrn (Windows display settings):
    /// follow it.
    External,
}

struct ActiveSession {
    id: u64,
    fingerprint: Fingerprint,
    info: SessionInfo,
    commands: mpsc::UnboundedSender<SessionCommand>,
    finished: watch::Receiver<bool>,
}

struct PendingPairing {
    prompt: PairingPrompt,
    decision: Option<oneshot::Sender<PairDecision>>,
}

#[derive(Default)]
struct HostState {
    listening: bool,
    port: u16,
    session: Option<ActiveSession>,
    pairing: Option<PendingPairing>,
    pairing_attempts: VecDeque<Instant>,
    last_error: Option<String>,
}

struct HostInner {
    platform: Arc<dyn HostPlatform>,
    identity: Identity,
    acceptor: TlsAcceptor,
    data_dir: PathBuf,
    default_host_name: String,
    listen_addr: IpAddr,
    interfaces: InterfacePolicy,
    enable_mdns: bool,
    logs: Option<LogBuffer>,
    app_version: String,
    trust: Mutex<TrustStore>,
    settings: Mutex<HostSettings>,
    state: Mutex<HostState>,
    driver: Mutex<DriverStatus>,
    encoders: Mutex<Vec<CodecCapability>>,
    snapshot_tx: watch::Sender<HostSnapshot>,
    handle: Handle,
    listener: Mutex<Option<tokio::task::JoinHandle<()>>>,
    announcer: Mutex<Option<secondscrn_discovery::Announcer>>,
    ids: AtomicU64,
    pending_connections: Arc<tokio::sync::Semaphore>,
}

/// The running host. Cheap to share; all methods are synchronous and safe to
/// call from any thread (including UI threads via FFI).
pub struct HostEngine {
    inner: Arc<HostInner>,
    runtime: Mutex<Option<Runtime>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl HostEngine {
    /// Starts the host on its own runtime.
    pub fn start(config: HostConfig, platform: Arc<dyn HostPlatform>) -> Result<Self, SessionError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("secondscrn-host")
            .enable_all()
            .build()?;
        let engine = Self::start_on(runtime.handle().clone(), config, platform)?;
        *lock(&engine.runtime) = Some(runtime);
        Ok(engine)
    }

    /// Starts the host on an existing runtime (tests, embedding).
    pub fn start_on(handle: Handle, config: HostConfig, platform: Arc<dyn HostPlatform>) -> Result<Self, SessionError> {
        std::fs::create_dir_all(&config.data_dir)?;
        let identity = Identity::load_or_create(
            &config.data_dir.join("identity.bin"),
            &config.default_host_name,
            config.protector.as_ref(),
        )?;
        let trust = TrustStore::load(&config.data_dir.join("paired-devices.json"))?;
        let settings: HostSettings = settings::load::<HostSettings>(&config.data_dir.join("settings.json")).sanitized();
        let acceptor = TlsAcceptor::from(Arc::new(server_config(&identity, ALPN)?));

        let encoders = platform.encoders();
        let driver = platform.driver_status();
        let (snapshot_tx, _) = watch::channel(placeholder_snapshot(&settings));
        let inner = Arc::new(HostInner {
            platform,
            identity,
            acceptor,
            data_dir: config.data_dir,
            default_host_name: config.default_host_name,
            listen_addr: config.listen_addr,
            interfaces: config.interfaces,
            enable_mdns: config.enable_mdns,
            logs: config.logs,
            app_version: config.app_version,
            trust: Mutex::new(trust),
            settings: Mutex::new(settings),
            state: Mutex::new(HostState::default()),
            driver: Mutex::new(driver),
            encoders: Mutex::new(encoders),
            snapshot_tx,
            handle: handle.clone(),
            listener: Mutex::new(None),
            announcer: Mutex::new(None),
            ids: AtomicU64::new(1),
            pending_connections: Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_CONNECTIONS)),
        });
        tracing::info!(fingerprint = %inner.identity.fingerprint(), "host identity loaded");
        inner.restart_listener();
        handle.spawn(housekeeping(Arc::downgrade(&inner)));
        inner.publish();
        Ok(HostEngine { inner, runtime: Mutex::new(None) })
    }

    pub fn snapshot(&self) -> HostSnapshot {
        self.inner.snapshot_tx.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<HostSnapshot> {
        self.inner.snapshot_tx.subscribe()
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.inner.identity.fingerprint()
    }

    /// The PC user typed the PIN shown on the tablet.
    pub fn submit_pairing_pin(&self, id: u64, pin: &str) -> Result<(), SessionError> {
        let pin = normalize_pin(pin).ok_or(SessionError::InvalidInput("the code must have 6 digits"))?;
        self.decide_pairing(id, PairDecision::Pin(pin))
    }

    pub fn reject_pairing(&self, id: u64) -> Result<(), SessionError> {
        self.decide_pairing(id, PairDecision::Reject)
    }

    fn decide_pairing(&self, id: u64, decision: PairDecision) -> Result<(), SessionError> {
        let mut st = lock(&self.inner.state);
        let p = st.pairing.as_mut().filter(|p| p.prompt.id == id).ok_or(SessionError::NotFound("pairing request"))?;
        let tx = p.decision.take().ok_or(SessionError::NotFound("pairing request"))?;
        drop(st);
        let _ = tx.send(decision);
        Ok(())
    }

    /// Revokes a tablet: removes its pairing and ends its session.
    pub fn revoke_device(&self, fingerprint_hex: &str) -> Result<bool, SessionError> {
        let fp = Fingerprint::from_hex(fingerprint_hex).ok_or(SessionError::InvalidInput("fingerprint"))?;
        let removed = lock(&self.inner.trust).remove(&fp)?;
        {
            let st = lock(&self.inner.state);
            if let Some(s) = st.session.as_ref().filter(|s| s.fingerprint == fp) {
                let _ = s.commands.send(SessionCommand::Disconnect { reconnect: false });
            }
        }
        lock(&self.inner.settings).devices.remove(fingerprint_hex);
        self.inner.save_settings();
        tracing::info!(device = %fp, "device revoked");
        self.inner.publish();
        Ok(removed)
    }

    pub fn set_device_preferences(&self, fingerprint_hex: &str, prefs: DevicePreferences) -> Result<(), SessionError> {
        let fp = Fingerprint::from_hex(fingerprint_hex).ok_or(SessionError::InvalidInput("fingerprint"))?;
        if !lock(&self.inner.trust).is_trusted(&fp) {
            return Err(SessionError::NotFound("paired device"));
        }
        let (effective, scale_changed) = {
            let mut s = lock(&self.inner.settings);
            let old = s.device(fingerprint_hex).scale_percent();
            s.devices.insert(fingerprint_hex.to_string(), prefs.clone());
            (prefs.preset.unwrap_or(s.preset), old != prefs.scale_percent())
        };
        self.inner.save_settings();
        let st = lock(&self.inner.state);
        if let Some(s) = st.session.as_ref().filter(|s| s.fingerprint == fp) {
            let _ = s.commands.send(SessionCommand::SetPreset(effective));
            if scale_changed {
                let _ = s.commands.send(SessionCommand::SetResolution(prefs.scale_percent()));
            }
        }
        drop(st);
        self.inner.publish();
        Ok(())
    }

    /// Streams the connected tablet at `scale_percent` of its panel resolution
    /// (snapped to an offered scale) and remembers the choice for that tablet.
    pub fn set_resolution(&self, scale_percent: u32) -> Result<(), SessionError> {
        let scale = settings::snap_scale(scale_percent);
        let st = lock(&self.inner.state);
        let s = st.session.as_ref().ok_or(SessionError::NotFound("connected tablet"))?;
        let _ = s.commands.send(SessionCommand::SetResolution(scale));
        let fp = s.fingerprint;
        drop(st);
        self.inner.remember_scale(&fp, scale);
        Ok(())
    }

    /// Streams the connected tablet at up to `fps` frames per second and
    /// remembers the choice for that tablet.
    pub fn set_frame_rate(&self, fps: u32) -> Result<(), SessionError> {
        let fps = fps.clamp(10, 240);
        let st = lock(&self.inner.state);
        let s = st.session.as_ref().ok_or(SessionError::NotFound("connected tablet"))?;
        let _ = s.commands.send(SessionCommand::SetFrameRate(fps));
        let fp = s.fingerprint;
        drop(st);
        self.inner.remember_fps(&fp, fps);
        Ok(())
    }

    /// Ends the current session (if any). The tablet will not auto-reconnect.
    pub fn disconnect(&self) {
        self.end_session(false);
    }

    fn end_session(&self, reconnect: bool) {
        let st = lock(&self.inner.state);
        if let Some(s) = &st.session {
            let _ = s.commands.send(SessionCommand::Disconnect { reconnect });
        }
    }

    pub fn set_preset(&self, preset: QualityPreset) {
        lock(&self.inner.settings).preset = preset;
        self.inner.save_settings();
        let st = lock(&self.inner.state);
        if let Some(s) = &st.session {
            let device_preset = lock(&self.inner.settings).device(&s.fingerprint.to_hex()).preset;
            let _ = s.commands.send(SessionCommand::SetPreset(device_preset.unwrap_or(preset)));
        }
        drop(st);
        self.inner.publish();
    }

    /// Replaces all settings. Network-related changes restart the listener.
    pub fn update_settings(&self, new: HostSettings) {
        let new = new.sanitized();
        let restart = {
            let mut s = lock(&self.inner.settings);
            let restart = s.control_port != new.control_port
                || s.discoverable != new.discoverable
                || s.host_name != new.host_name;
            let preset_changed = s.preset != new.preset;
            *s = new;
            if preset_changed {
                let preset = s.preset;
                drop(s);
                let st = lock(&self.inner.state);
                if let Some(sess) = &st.session {
                    let _ = sess.commands.send(SessionCommand::SetPreset(preset));
                }
            }
            restart
        };
        self.inner.save_settings();
        if restart {
            self.inner.restart_listener();
        }
        self.inner.publish();
    }

    /// Re-probes encoders and driver state (e.g. after installing the driver).
    pub fn refresh_platform(&self) {
        let inner = self.inner.clone();
        self.inner.handle.spawn_blocking(move || {
            *lock(&inner.encoders) = inner.platform.encoders();
            *lock(&inner.driver) = inner.platform.driver_status();
            inner.publish();
        });
    }

    /// A JSON report with secrets redacted, suitable for bug reports.
    pub fn export_diagnostics(&self) -> String {
        let snap = self.snapshot();
        let logs = self.inner.logs.as_ref().map(|l| l.recent(2000)).unwrap_or_default();
        secondscrn_diagnostics::export_report(
            vec![
                ("app", serde_json::json!({ "role": "host", "version": self.inner.app_version })),
                ("os", serde_json::json!({ "family": std::env::consts::OS, "arch": std::env::consts::ARCH })),
                ("state", serde_json::to_value(&snap).unwrap_or_default()),
            ],
            &logs,
        )
    }

    /// Stops the host. A connected tablet is told to reconnect, so app
    /// restarts and updates are seamless.
    pub fn shutdown(&self) {
        self.end_session(true);
        if let Some(l) = lock(&self.inner.listener).take() {
            l.abort();
        }
        lock(&self.inner.announcer).take();
        if let Some(rt) = lock(&self.runtime).take() {
            // Give the session a moment to send Goodbye and release the display.
            std::thread::sleep(Duration::from_millis(300));
            rt.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

impl Drop for HostEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn placeholder_snapshot(settings: &HostSettings) -> HostSnapshot {
    HostSnapshot {
        host_name: String::new(),
        fingerprint_short: String::new(),
        listening: false,
        port: settings.control_port,
        addresses: Vec::new(),
        session: None,
        pairing: None,
        paired_devices: Vec::new(),
        driver: DriverStatus::NotApplicable,
        encoders: Vec::new(),
        last_error: None,
        settings: settings.clone(),
    }
}

fn local_addresses() -> Vec<String> {
    secondscrn_transport::local_addresses(false).into_iter().map(|ip| ip.to_string()).collect()
}

impl HostInner {
    fn host_name(&self) -> String {
        lock(&self.settings)
            .host_name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| self.default_host_name.clone())
    }

    fn save_settings(&self) {
        let s = lock(&self.settings).clone();
        if let Err(e) = settings::save(&self.data_dir.join("settings.json"), &s) {
            tracing::error!(error = %e, "failed to save settings");
        }
    }

    fn remember_scale(&self, fp: &Fingerprint, scale: u32) {
        {
            let mut s = lock(&self.settings);
            let mut prefs = s.device(&fp.to_hex());
            prefs.resolution_scale = Some(scale);
            s.devices.insert(fp.to_hex(), prefs);
        }
        self.save_settings();
        self.publish();
    }

    fn remember_fps(&self, fp: &Fingerprint, fps: u32) {
        {
            let mut s = lock(&self.settings);
            let mut prefs = s.device(&fp.to_hex());
            prefs.max_fps = Some(fps);
            s.devices.insert(fp.to_hex(), prefs);
        }
        self.save_settings();
        self.publish();
    }

    fn set_error(&self, msg: Option<String>) {
        lock(&self.state).last_error = msg;
        self.publish();
    }

    fn publish(&self) {
        let settings = lock(&self.settings).clone();
        let paired_devices = lock(&self.trust)
            .peers()
            .map(|p| PairedDevice {
                fingerprint: p.fingerprint.to_hex(),
                fingerprint_short: p.fingerprint.short(),
                name: p.name.clone(),
                model: p.model.clone(),
                paired_at: p.paired_at,
                last_seen: p.last_seen,
                preferences: settings.device(&p.fingerprint.to_hex()),
            })
            .collect();
        let st = lock(&self.state);
        let snap = HostSnapshot {
            host_name: self.host_name(),
            fingerprint_short: self.identity.fingerprint().short(),
            listening: st.listening,
            port: st.port,
            addresses: local_addresses(),
            session: st.session.as_ref().map(|s| s.info.clone()),
            pairing: st.pairing.as_ref().map(|p| p.prompt.clone()),
            paired_devices,
            driver: lock(&self.driver).clone(),
            encoders: lock(&self.encoders).clone(),
            last_error: st.last_error.clone(),
            settings,
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

    fn update_session(&self, id: u64, f: impl FnOnce(&mut SessionInfo)) {
        let mut st = lock(&self.state);
        if let Some(s) = st.session.as_mut().filter(|s| s.id == id) {
            f(&mut s.info);
        }
        drop(st);
        self.publish();
    }

    fn restart_listener(self: &Arc<Self>) {
        if let Some(old) = lock(&self.listener).take() {
            old.abort();
        }
        lock(&self.announcer).take();
        let (port, discoverable) = {
            let s = lock(&self.settings);
            (s.control_port, s.discoverable)
        };
        let me = self.clone();
        let task = self.handle.spawn(async move {
            let listener = match TcpListener::bind(SocketAddr::new(me.listen_addr, port)).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(port, error = %e, "cannot listen for tablets");
                    {
                        let mut st = lock(&me.state);
                        st.listening = false;
                        st.last_error = Some(format!("Port {port} is unavailable: {e}"));
                    }
                    me.publish();
                    return;
                }
            };
            let actual = listener.local_addr().map(|a| a.port()).unwrap_or(port);
            {
                let mut st = lock(&me.state);
                st.listening = true;
                st.port = actual;
                st.last_error = None;
            }
            if discoverable && me.enable_mdns {
                match secondscrn_discovery::Announcer::start(
                    &me.host_name(),
                    &me.identity.fingerprint().to_hex(),
                    actual,
                ) {
                    Ok(a) => *lock(&me.announcer) = Some(a),
                    Err(e) => tracing::warn!(error = %e, "mDNS announcement failed; manual connection still works"),
                }
            }
            tracing::info!(port = actual, "listening for tablets");
            me.publish();
            loop {
                let (tcp, peer) = match listener.accept().await {
                    Ok(x) => x,
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                };
                let Ok(permit) = me.pending_connections.clone().try_acquire_owned() else {
                    tracing::warn!(%peer, "too many pending connections, dropping");
                    continue;
                };
                let me2 = me.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(me2, tcp, peer, permit).await {
                        tracing::info!(%peer, error = %e, "connection ended");
                    }
                });
            }
        });
        *lock(&self.listener) = Some(task);
    }
}

/// Periodically refreshes the driver status so the UI notices installs and
/// failures without user action.
async fn housekeeping(inner: std::sync::Weak<HostInner>) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let Some(inner) = inner.upgrade() else { return };
        let platform = inner.platform.clone();
        if let Ok(status) = tokio::task::spawn_blocking(move || platform.driver_status()).await {
            *lock(&inner.driver) = status;
            inner.publish();
        }
    }
}

type Tls = tokio_rustls::server::TlsStream<TcpStream>;
type HostChannel = ControlChannel<Tls, ClientMessage>;

async fn handle_connection(
    inner: Arc<HostInner>,
    tcp: TcpStream,
    peer: SocketAddr,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<(), SessionError> {
    let _ = tcp.set_nodelay(true);
    let tls = tokio::time::timeout(STEP_TIMEOUT, inner.acceptor.accept(tcp))
        .await
        .map_err(|_| SessionError::Timeout("TLS handshake"))??;
    let (fp, exporter) = {
        let (_, conn) = tls.get_ref();
        let fp =
            peer_fingerprint(conn.peer_certificates()).ok_or(SessionError::Protocol("missing client certificate"))?;
        let mut ekm = [0u8; 32];
        conn.export_keying_material(&mut ekm, PAIRING_EXPORTER_LABEL, None)
            .map_err(|_| SessionError::Protocol("TLS exporter unavailable"))?;
        (fp, ekm)
    };
    let mut ch: HostChannel = ControlChannel::new(tls);

    let (device, intent) = match ch.recv_timeout(STEP_TIMEOUT, "hello").await? {
        ClientMessage::Hello { protocol_version, device, intent } => {
            if protocol_version != PROTOCOL_VERSION {
                ch.send(&HostMessage::Rejected { reason: RejectReason::IncompatibleVersion }).await?;
                return Err(SessionError::Rejected(RejectReason::IncompatibleVersion));
            }
            (sanitize_device(device), intent)
        }
        _ => return Err(SessionError::Protocol("expected hello")),
    };
    tracing::info!(%peer, device = %device.name, fingerprint = %fp, ?intent, "tablet connected");

    let trusted = lock(&inner.trust).is_trusted(&fp);
    let hello = HostMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        host: DeviceInfo {
            name: inner.host_name(),
            model: "Windows PC".into(),
            app_version: inner.app_version.clone(),
        },
    };
    match (intent, trusted) {
        (Intent::Connect, false) => {
            ch.send(&HostMessage::Rejected { reason: RejectReason::NotPaired }).await?;
            return Err(SessionError::Rejected(RejectReason::NotPaired));
        }
        (Intent::Connect, true) => ch.send(&hello).await?,
        (Intent::Pair, _) => {
            ch.send(&hello).await?;
            pair(&inner, &mut ch, fp, &exporter, &device, peer).await?;
        }
    }
    drop(permit); // authenticated from here on

    if !lock(&inner.trust).is_trusted(&fp) {
        // Revoked while connecting.
        ch.send(&HostMessage::Rejected { reason: RejectReason::NotPaired }).await?;
        return Err(SessionError::Rejected(RejectReason::NotPaired));
    }
    let _ = lock(&inner.trust).touch(&fp);
    inner.publish();

    loop {
        match ch.recv_timeout(Duration::from_secs(60), "session request").await? {
            ClientMessage::SessionRequest { display, decoders, preset: _, preferred_codec } => {
                return run_session(inner, ch, fp, device, peer, display, decoders, preferred_codec).await;
            }
            ClientMessage::Ping { nonce } => ch.send(&HostMessage::Pong { nonce }).await?,
            ClientMessage::Goodbye => return Ok(()),
            _ => {}
        }
    }
}

fn sanitize_device(mut d: DeviceInfo) -> DeviceInfo {
    let clean = |s: &str, max: usize| -> String {
        s.chars().filter(|c| !c.is_control()).take(max).collect::<String>().trim().to_string()
    };
    d.name = clean(&d.name, 64);
    if d.name.is_empty() {
        d.name = "Android tablet".into();
    }
    d.model = clean(&d.model, 64);
    d.app_version = clean(&d.app_version, 32);
    d
}

async fn pair(
    inner: &Arc<HostInner>,
    ch: &mut HostChannel,
    fp: Fingerprint,
    exporter: &[u8],
    device: &DeviceInfo,
    peer: SocketAddr,
) -> Result<(), SessionError> {
    let id = inner.ids.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    let busy = {
        let mut st = lock(&inner.state);
        let now = Instant::now();
        while st.pairing_attempts.front().is_some_and(|t| now.duration_since(*t) > PAIRING_WINDOW) {
            st.pairing_attempts.pop_front();
        }
        st.pairing.is_some() || st.pairing_attempts.len() >= PAIRING_MAX_ATTEMPTS
    };
    if busy {
        ch.send(&HostMessage::Rejected { reason: RejectReason::PairingBusy }).await?;
        return Err(SessionError::Rejected(RejectReason::PairingBusy));
    }
    {
        let mut st = lock(&inner.state);
        st.pairing_attempts.push_back(Instant::now());
        st.pairing = Some(PendingPairing {
            prompt: PairingPrompt {
                id,
                device_name: device.name.clone(),
                device_model: device.model.clone(),
                fingerprint_short: fp.short(),
                remote_address: peer.ip().to_string(),
            },
            decision: Some(tx),
        });
    }
    inner.publish();

    let result = async {
        ch.send(&HostMessage::PairPending).await?;
        let client_msg = match ch.recv_timeout(STEP_TIMEOUT, "pairing start").await? {
            ClientMessage::PairStart { spake } => spake,
            _ => return Err(SessionError::Protocol("expected pair_start")),
        };
        // Wait for the PC user while watching the connection: if the tablet
        // gives up, the prompt must disappear.
        let decision = tokio::select! {
            d = tokio::time::timeout(PAIRING_TIMEOUT, rx) => match d {
                Ok(Ok(d)) => d,
                _ => PairDecision::Reject,
            },
            _ = wait_closed(ch) => return Err(SessionError::ConnectionClosed),
        };
        let pin = match decision {
            PairDecision::Pin(pin) => pin,
            PairDecision::Reject => {
                ch.send(&HostMessage::Rejected { reason: RejectReason::PairingDeclined }).await?;
                return Err(SessionError::Rejected(RejectReason::PairingDeclined));
            }
        };
        let (pairing, spake, mac) =
            HostPairing::respond(&pin, inner.identity.fingerprint(), fp, &client_msg, exporter)?;
        ch.send(&HostMessage::PairChallenge { spake, mac }).await?;
        let client_mac = match ch.recv_timeout(STEP_TIMEOUT, "pairing confirmation").await {
            Ok(ClientMessage::PairConfirm { mac }) => mac,
            // The tablet aborts silently when our MAC doesn't verify (wrong PIN).
            Ok(_) | Err(_) => return Err(SessionError::Rejected(RejectReason::PairingDeclined)),
        };
        if pairing.verify(&client_mac).is_err() {
            ch.send(&HostMessage::Rejected { reason: RejectReason::PairingDeclined }).await?;
            return Err(SessionError::Rejected(RejectReason::PairingDeclined));
        }
        lock(&inner.trust).add(TrustedPeer {
            fingerprint: fp,
            name: device.name.clone(),
            model: device.model.clone(),
            paired_at: now_unix(),
            last_seen: Some(now_unix()),
        })?;
        ch.send(&HostMessage::PairComplete).await?;
        tracing::info!(device = %device.name, fingerprint = %fp, "device paired");
        Ok(())
    }
    .await;

    lock(&inner.state).pairing = None;
    match &result {
        Err(SessionError::Rejected(RejectReason::PairingDeclined)) => {
            inner.set_error(Some(format!("Pairing with {} failed: wrong code or declined.", device.name)))
        }
        _ => inner.publish(),
    }
    result
}

/// Resolves when the peer closes the control channel (other messages are
/// discarded; only used while waiting for local user input).
async fn wait_closed(ch: &mut HostChannel) {
    loop {
        match ch.rx.recv().await {
            None | Some(Err(_)) => return,
            Some(Ok(ClientMessage::Goodbye)) => return,
            Some(Ok(_)) => {}
        }
    }
}

struct Sink {
    frames: FrameSender,
    errors: mpsc::UnboundedSender<PlatformError>,
}

impl PipelineSink for Sink {
    fn on_frame(&self, frame: secondscrn_transport::EncodedFrame) -> bool {
        self.frames.send_frame(frame)
    }
    fn on_error(&self, error: PlatformError) {
        let _ = self.errors.send(error);
    }
}

/// Input injection runs on its own thread: ordered, and never blocks the
/// async runtime.
fn spawn_input_thread(
    platform: Arc<dyn HostPlatform>,
    display: Arc<Mutex<DisplayInfo>>,
) -> std::sync::mpsc::Sender<InputEvent> {
    let (tx, rx) = std::sync::mpsc::channel::<InputEvent>();
    std::thread::Builder::new()
        .name("secondscrn-input".into())
        .spawn(move || {
            let mut last_seq = 0u64;
            loop {
                let ev = match rx.recv_timeout(Duration::from_millis(40)) {
                    Ok(ev) => ev,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        platform.input_idle(&lock(&display).clone());
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        platform.input_ended();
                        return;
                    }
                };
                if ev.seq() <= last_seq && last_seq != 0 {
                    continue; // stale or replayed
                }
                last_seq = ev.seq();
                let d = lock(&display).clone();
                if let Err(e) = platform.inject_input(&d, &ev) {
                    tracing::debug!(error = %e, "input injection failed");
                }
            }
        })
        .expect("spawn input thread");
    tx
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    inner: Arc<HostInner>,
    mut ch: HostChannel,
    fp: Fingerprint,
    device: DeviceInfo,
    peer: SocketAddr,
    display_req: DisplayRequest,
    decoders: Vec<CodecCapability>,
    client_preferred: Option<secondscrn_protocol::Codec>,
) -> Result<(), SessionError> {
    // --- Admission: one session at a time; the same tablet may replace its
    // own stale session (typical after a Wi-Fi drop the PC hasn't noticed).
    let id = inner.ids.fetch_add(1, Ordering::Relaxed);
    let (cmd_tx, mut commands) = mpsc::unbounded_channel();
    let (finished_tx, finished_rx) = watch::channel(false);
    let admission = {
        let mut st = lock(&inner.state);
        match st.session.take() {
            Some(s) if s.fingerprint != fp => {
                st.session = Some(s);
                None
            }
            Some(s) => {
                let _ = s.commands.send(SessionCommand::Disconnect { reconnect: false });
                Some(Some(s.finished.clone()))
            }
            None => Some(None),
        }
    };
    match admission {
        None => {
            ch.send(&HostMessage::Rejected { reason: RejectReason::Busy }).await?;
            return Err(SessionError::Rejected(RejectReason::Busy));
        }
        Some(Some(mut previous)) => {
            tracing::info!("replacing this tablet's previous session");
            let _ = tokio::time::timeout(Duration::from_secs(5), previous.wait_for(|done| *done)).await;
        }
        Some(None) => {}
    }
    let claimed = {
        let mut st = lock(&inner.state);
        if st.session.is_some() {
            false
        } else {
            st.session = Some(ActiveSession {
                id,
                fingerprint: fp,
                info: SessionInfo {
                    device_name: device.name.clone(),
                    device_model: device.model.clone(),
                    fingerprint: fp.to_hex(),
                    fingerprint_short: fp.short(),
                    remote_address: peer.ip().to_string(),
                    phase: SessionPhase::Negotiating,
                    stream: None,
                    display: None,
                    resolutions: Vec::new(),
                    frame_rates: Vec::new(),
                    metrics: HostMetrics::default(),
                    connected_since: now_unix(),
                },
                commands: cmd_tx,
                finished: finished_rx,
            });
            st.last_error = None;
            true
        }
    };
    if !claimed {
        ch.send(&HostMessage::Rejected { reason: RejectReason::Busy }).await?;
        return Err(SessionError::Rejected(RejectReason::Busy));
    }
    inner.publish();

    let result = stream(&inner, &mut ch, id, fp, display_req, decoders, client_preferred, &mut commands).await;

    {
        let mut st = lock(&inner.state);
        if st.session.as_ref().is_some_and(|s| s.id == id) {
            st.session = None;
        }
        match &result {
            Ok(()) => {}
            Err(SessionError::Rejected(r)) => st.last_error = Some(format!("{}: {r}", device.name)),
            Err(e) => st.last_error = Some(format!("Session with {} ended: {e}", device.name)),
        }
    }
    let _ = finished_tx.send(true);
    inner.publish();
    tracing::info!(device = %device.name, result = ?result.as_ref().err().map(|e| e.to_string()), "session ended");
    result
}

#[allow(clippy::too_many_arguments)]
async fn stream(
    inner: &Arc<HostInner>,
    ch: &mut HostChannel,
    id: u64,
    fp: Fingerprint,
    display_req: DisplayRequest,
    decoders: Vec<CodecCapability>,
    client_preferred: Option<secondscrn_protocol::Codec>,
    commands: &mut mpsc::UnboundedReceiver<SessionCommand>,
) -> Result<(), SessionError> {
    let (settings, mut preset, mut scale, mut fps_cap) = {
        let s = lock(&inner.settings).clone();
        let prefs = s.device(&fp.to_hex());
        let preset = prefs.preset.unwrap_or(s.preset);
        // A per-tablet frame rate overrides the global limit.
        (s.clone(), preset, prefs.scale_percent(), prefs.max_fps.unwrap_or(s.max_fps))
    };
    // The tablet reports its panel; the monitor is that panel at `scale`.
    let mut native = display_req;
    let encoders = lock(&inner.encoders).clone();
    let negotiate_for = |req: DisplayRequest, preset: QualityPreset, fps_cap: u32| {
        negotiate(&NegotiationInput {
            encoders: &encoders,
            decoders: &decoders,
            display: req,
            preset,
            forced_codec: settings.preferred_codec,
            client_preferred,
            host_max_fps: fps_cap,
        })
    };
    let mut config = match negotiate_for(native.scaled(scale), preset, fps_cap) {
        Ok(c) => c,
        Err(e) => {
            let reason = match e {
                NegotiationError::NoEncoder => RejectReason::InternalError,
                NegotiationError::NoCommonCodec(..) => RejectReason::NoCommonCodec,
            };
            ch.send(&HostMessage::Rejected { reason }).await?;
            inner.set_error(Some(e.to_string()));
            return Err(SessionError::Rejected(reason));
        }
    };

    // --- Display
    let linger = Duration::from_secs(settings.display_linger_secs as u64);
    let platform = inner.platform.clone();
    let plan = DisplayPlan::new(&native, scale);
    let lease = tokio::task::spawn_blocking(move || platform.acquire_display(&plan, linger))
        .await
        .map_err(|_| SessionError::Protocol("display task panicked"))?;
    let mut lease: Box<dyn DisplayLease> = match lease {
        Ok(l) => l,
        Err(e) => {
            ch.send(&HostMessage::Rejected { reason: RejectReason::DisplayUnavailable }).await?;
            inner.set_error(Some(format!("Virtual display: {e}")));
            return Err(e.into());
        }
    };
    let mut display = lease.info();
    // Stream what the display actually is (the driver may round the mode).
    config.width = display.width;
    config.height = display.height;
    config.scale_percent = native.scale_of(display.width, display.height);
    inner.update_session(id, |s| {
        s.stream = Some(config.clone());
        s.display = Some(display.clone());
        s.resolutions = native.resolution_options();
        s.frame_rates = native.frame_rate_options();
    });

    // --- WebRTC
    let user_max = (settings.max_bitrate_mbps > 0).then(|| settings.max_bitrate_mbps as u64 * 1_000_000);
    let policy_for = |c: &StreamConfig, p: QualityPreset| {
        BitratePolicy::for_stream(c.codec, c.width, c.height, c.max_fps, p, user_max)
    };
    let mut controller = BitrateController::new(policy_for(&config, preset));
    config.initial_bitrate_bps = controller.current().bitrate_bps;
    let (pending, offer) =
        PendingSender::create(SenderConfig { codec: config.codec, interfaces: inner.interfaces.clone() }).await?;
    ch.send(&HostMessage::SessionAccepted { stream: config.clone() }).await?;
    ch.send(&HostMessage::SdpOffer { sdp: offer }).await?;
    let answer = loop {
        match ch.recv_timeout(STEP_TIMEOUT, "SDP answer").await? {
            ClientMessage::SdpAnswer { sdp } => break sdp,
            ClientMessage::Ping { nonce } => ch.send(&HostMessage::Pong { nonce }).await?,
            ClientMessage::Goodbye => return Ok(()),
            _ => {}
        }
    };
    let mut media = pending.accept_answer(&answer)?;

    // --- Pipeline
    let (err_tx, mut pipeline_errors) = mpsc::unbounded_channel();
    let start_pipeline = |display: DisplayInfo,
                          config: StreamConfig,
                          target: crate::adapt::EncoderTarget,
                          frames: FrameSender,
                          errors: mpsc::UnboundedSender<PlatformError>| {
        let platform = inner.platform.clone();
        tokio::task::spawn_blocking(move || {
            let p = platform.start_video(&display, &config, Box::new(Sink { frames, errors }))?;
            p.set_target(target.bitrate_bps, target.fps);
            Ok::<_, PlatformError>(p)
        })
    };
    let mut pipeline: Box<dyn VideoPipeline> =
        start_pipeline(display.clone(), config.clone(), controller.current(), media.frame_sender(), err_tx.clone())
            .await
            .map_err(|_| SessionError::Protocol("pipeline task panicked"))??;

    let shared_display = Arc::new(Mutex::new(display.clone()));
    let input_tx = spawn_input_thread(inner.platform.clone(), shared_display.clone());

    // After setup: the tablet only listens for it once streaming.
    if let Some(s) = lease.ui_scale() {
        ch.send(&HostMessage::UiScale { current: s.current, options: s.options }).await?;
    }

    let mut ping = tokio::time::interval(PING_INTERVAL);
    let mut last_rx = Instant::now();
    let mut nonce = 0u64;
    let mut restarts = 0u32;
    let mut metrics = HostMetrics::default();
    let mut last_bytes: Option<(u64, Instant)> = None;

    // Ok(reconnect) = orderly end.
    let outcome: Result<bool, SessionError> = loop {
        let mut change: Option<DisplayChange> = None;
        tokio::select! {
            msg = ch.rx.recv() => {
                let msg = match msg {
                    None => break Err(SessionError::ConnectionClosed),
                    Some(Err(e)) => break Err(e.into()),
                    Some(Ok(m)) => m,
                };
                last_rx = Instant::now();
                match msg {
                    ClientMessage::Input { event } => { let _ = input_tx.send(event); }
                    ClientMessage::RequestKeyframe => pipeline.request_keyframe(),
                    ClientMessage::Stats { stats } => metrics.client = Some(stats),
                    ClientMessage::Ping { nonce } => ch.send(&HostMessage::Pong { nonce }).await?,
                    ClientMessage::Pong { .. } => {}
                    ClientMessage::SetPreset { preset: p } => {
                        preset = p;
                        if let Some(t) = controller.set_policy(policy_for(&config, preset)) {
                            pipeline.set_target(t.bitrate_bps, t.fps);
                        }
                    }
                    ClientMessage::DisplayChanged { display: req } => {
                        native = req;
                        change = Some(DisplayChange::Plan);
                    }
                    ClientMessage::SetResolution { scale_percent } => {
                        scale = settings::snap_scale(scale_percent);
                        inner.remember_scale(&fp, scale);
                        change = Some(DisplayChange::Plan);
                    }
                    ClientMessage::SetUiScale { percent } => {
                        match lease.set_ui_scale(percent) {
                            Ok(s) => {
                                tracing::info!(percent = s.current, "display scale changed");
                                ch.send(&HostMessage::UiScale { current: s.current, options: s.options }).await?;
                            }
                            Err(e) => tracing::warn!(error = %e, percent, "cannot change the display scale"),
                        }
                    }
                    ClientMessage::SetFrameRate { fps } => {
                        fps_cap = fps.clamp(10, 240);
                        inner.remember_fps(&fp, fps_cap);
                        change = Some(DisplayChange::FrameRate);
                    }
                    ClientMessage::Goodbye => break Ok(false),
                    ClientMessage::SessionRequest { .. } | ClientMessage::SdpAnswer { .. } | ClientMessage::Hello { .. }
                    | ClientMessage::PairStart { .. } | ClientMessage::PairConfirm { .. } => {
                        tracing::debug!("ignoring unexpected message during session");
                    }
                }
            }
            ev = media.events.recv() => {
                let Some(ev) = ev else { break Err(SessionError::Transport("media transport stopped".into())) };
                match ev {
                    TransportEvent::Connected { .. } => {
                        pipeline.request_keyframe();
                        inner.update_session(id, |s| s.phase = SessionPhase::Streaming);
                        tracing::info!("media connected");
                    }
                    TransportEvent::Interrupted => inner.update_session(id, |s| s.phase = SessionPhase::Interrupted),
                    TransportEvent::Resumed => {
                        pipeline.request_keyframe();
                        inner.update_session(id, |s| s.phase = SessionPhase::Streaming);
                    }
                    TransportEvent::BitrateEstimate(_) => {}
                    TransportEvent::KeyframeRequested => pipeline.request_keyframe(),
                    TransportEvent::Stats(s) => {
                        let now = Instant::now();
                        if let Some((b, t)) = last_bytes {
                            let dt = now.duration_since(t).as_secs_f64();
                            if dt > 0.1 {
                                metrics.send_rate_bps = ((s.bytes_sent.saturating_sub(b)) as f64 * 8.0 / dt) as u64;
                            }
                        }
                        last_bytes = Some((s.bytes_sent, now));
                        if let Some(t) = controller.on_feedback(s.loss, s.rtt, metrics.send_rate_bps, now) {
                            tracing::debug!(?t, budget = controller.budget_bps(), congestion = ?controller.last_congestion(), "encoder target");
                            pipeline.set_target(t.bitrate_bps, t.fps);
                        }
                        metrics.bitrate_estimate_bps = Some(controller.budget_bps());
                        metrics.bytes_sent = s.bytes_sent;
                        metrics.rtt_ms = s.rtt.map(|d| d.as_secs_f32() * 1000.0);
                        metrics.loss = s.loss;
                        metrics.nacks = s.nacks;
                        metrics.plis = s.plis;
                    }
                    TransportEvent::Frame(_) => {}
                    TransportEvent::Closed { reason } => break Err(SessionError::Transport(reason)),
                }
            }
            Some(err) = pipeline_errors.recv() => {
                if mode_changed(lease.as_ref(), &config) {
                    // Expected: capture stops when the monitor changes size.
                    change = Some(DisplayChange::External);
                } else {
                    restarts += 1;
                    tracing::warn!(error = %err, restarts, "video pipeline failed");
                    if restarts > MAX_PIPELINE_RESTARTS {
                        inner.set_error(Some(format!("Streaming stopped: {err}")));
                        break Err(err.into());
                    }
                    drop(pipeline);
                    tokio::time::sleep(Duration::from_millis(250 * restarts as u64)).await;
                    display = lease.info();
                    *lock(&shared_display) = display.clone();
                    pipeline = start_pipeline(display.clone(), config.clone(), controller.current(), media.frame_sender(), err_tx.clone())
                        .await
                        .map_err(|_| SessionError::Protocol("pipeline task panicked"))??;
                }
            }
            cmd = commands.recv() => match cmd {
                None => break Ok(false),
                Some(SessionCommand::Disconnect { reconnect }) => break Ok(reconnect),
                Some(SessionCommand::SetPreset(p)) => {
                    preset = p;
                    if let Some(t) = controller.set_policy(policy_for(&config, preset)) {
                        pipeline.set_target(t.bitrate_bps, t.fps);
                    }
                }
                Some(SessionCommand::SetResolution(p)) => {
                    scale = p;
                    change = Some(DisplayChange::Plan);
                }
                Some(SessionCommand::SetFrameRate(f)) => {
                    fps_cap = f;
                    change = Some(DisplayChange::FrameRate);
                }
            },
            _ = ping.tick() => {
                if last_rx.elapsed() > PEER_TIMEOUT {
                    break Err(SessionError::Timeout("tablet stopped responding"));
                }
                nonce += 1;
                ch.send(&HostMessage::Ping { nonce }).await?;
                metrics.pipeline = pipeline.stats();
                metrics.frames_dropped_network = media.frames_dropped();
                let target_fps = controller.current().fps;
                metrics.health = LinkHealth::assess(metrics.rtt_ms, metrics.loss, metrics.pipeline.encode_fps, target_fps);
                let m = metrics.clone();
                inner.update_session(id, |s| s.metrics = m);
                if mode_changed(lease.as_ref(), &config) {
                    change = Some(DisplayChange::External);
                }
            }
        }

        // Reconfigure the monitor and restart the encoder at the new size;
        // the WebRTC session and the tablet's decoder surface are kept.
        if let Some(change) = change {
            drop(pipeline);
            match change {
                DisplayChange::FrameRate => {}
                DisplayChange::Plan => {
                    let plan = DisplayPlan::new(&native, scale);
                    let mut l = lease;
                    let (l, r) = tokio::task::spawn_blocking(move || {
                        let r = l.reconfigure(&plan);
                        (l, r)
                    })
                    .await
                    .map_err(|_| SessionError::Protocol("display task panicked"))?;
                    lease = l;
                    match r {
                        Ok(d) => display = d,
                        Err(e) => {
                            tracing::warn!(error = %e, "display reconfiguration failed; keeping the current mode");
                            display = lease.info();
                        }
                    }
                }
                DisplayChange::External => {
                    display = lease.info();
                    let (w, h) = (display.width, display.height);
                    tracing::info!(width = w, height = h, "display mode changed on the PC; following it");
                }
            }
            *lock(&shared_display) = display.clone();
            if let Ok(c) = negotiate_for(native.scaled(scale), preset, fps_cap)
                && c.codec == config.codec
            {
                config.max_fps = c.max_fps;
            }
            config.width = display.width;
            config.height = display.height;
            config.scale_percent = native.scale_of(display.width, display.height);
            if let Some(t) = controller.set_policy(policy_for(&config, preset)) {
                tracing::debug!(?t, "target after display change");
            }
            ch.send(&HostMessage::StreamChanged { stream: config.clone() }).await?;
            // Windows picks a new default scale for a new resolution.
            if let Some(s) = lease.ui_scale() {
                ch.send(&HostMessage::UiScale { current: s.current, options: s.options }).await?;
            }
            pipeline = start_pipeline(
                display.clone(),
                config.clone(),
                controller.current(),
                media.frame_sender(),
                err_tx.clone(),
            )
            .await
            .map_err(|_| SessionError::Protocol("pipeline task panicked"))??;
            inner.update_session(id, |s| {
                s.stream = Some(config.clone());
                s.display = Some(display.clone());
                s.resolutions = native.resolution_options();
                s.frame_rates = native.frame_rate_options();
            });
        }
    };

    // Best effort: tell the tablet whether it should come back.
    if let Ok(reconnect) = outcome {
        let _ = tokio::time::timeout(Duration::from_secs(1), ch.send(&HostMessage::Goodbye { reconnect })).await;
    }
    drop(pipeline);
    drop(media);
    // Dropping the lease on a blocking thread: the platform may talk to the
    // display driver.
    let _ = tokio::task::spawn_blocking(move || drop(lease)).await;
    outcome.map(|_| ())
}

/// True when the leased monitor no longer has the size being streamed (the
/// user changed it in the Windows display settings).
fn mode_changed(lease: &dyn DisplayLease, config: &StreamConfig) -> bool {
    let d = lease.info();
    (d.width, d.height) != (config.width, config.height)
}
