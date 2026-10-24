//! WebRTC media transport built on [str0m].
//!
//! One crate serves both ends: the host is the *sender* (creates the SDP offer
//! with a single send-only video m-line), the tablet is the *receiver*
//! (answers it). Signaling is done by the caller over the authenticated
//! control channel; this crate never talks to a signaling server.
//!
//! str0m provides ICE, DTLS-SRTP, NACK/RTX, TWCC and GCC bandwidth estimation.
//! This crate adds the I/O around it: one UDP socket per local interface,
//! a tokio task driving the `Rtc` state machine, bounded channels towards the
//! encoder/decoder, and keyframe-request throttling.
//!
//! The caller-visible boundary is [`MediaHandle`]: send frames in (host),
//! receive [`TransportEvent`]s out (both). Other transports (e.g. USB) can be
//! added later behind the same shape without touching capture or decode.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use secondscrn_protocol::Codec;
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::format::Codec as RtcCodec;
use str0m::media::{Direction, Frequency, KeyframeRequestKind, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

mod sdp_util;
pub use sdp_util::candidate_ports;

/// Bitrate of the BWE before the first estimate arrives.
pub const DEFAULT_START_BITRATE_BPS: u64 = 8_000_000;
/// Minimum interval between keyframe requests from the receiver.
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(300);
/// How long ICE may stay disconnected before the peer is declared lost.
const DISCONNECT_GRACE: Duration = Duration::from_secs(4);
/// Encoded frames buffered towards the network / decoder. Small on purpose:
/// for an interactive display a late frame is worth less than a fresh one.
const FRAME_QUEUE: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("network i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("webrtc: {0}")]
    Rtc(#[from] str0m::RtcError),
    #[error("invalid SDP: {0}")]
    Sdp(String),
    #[error("no usable local network interface")]
    NoInterfaces,
    #[error("the negotiated session has no video track for {0}")]
    CodecNotNegotiated(Codec),
}

/// Network interfaces to offer as ICE host candidates.
#[derive(Debug, Clone)]
pub struct InterfacePolicy {
    /// Include the loopback interface. Needed for the Android emulator, whose
    /// NAT maps the guest's 10.0.2.2 to the host's 127.0.0.1.
    pub include_loopback: bool,
    /// Restrict to these addresses (used in tests). Empty = all interfaces.
    pub only: Vec<IpAddr>,
}

impl Default for InterfacePolicy {
    fn default() -> Self {
        InterfacePolicy { include_loopback: true, only: Vec::new() }
    }
}

/// Usable local IPv4 addresses (also shown in the UI for manual connection).
pub fn local_addresses(include_loopback: bool) -> Vec<IpAddr> {
    InterfacePolicy { include_loopback, only: Vec::new() }.addresses()
}

impl InterfacePolicy {
    pub fn loopback_only() -> Self {
        InterfacePolicy { include_loopback: true, only: vec![IpAddr::from([127, 0, 0, 1])] }
    }

    fn addresses(&self) -> Vec<IpAddr> {
        if !self.only.is_empty() {
            return self.only.clone();
        }
        let mut out: Vec<IpAddr> = if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .filter(|i| !i.is_link_local())
            .map(|i| i.ip())
            // IPv4 only for now: LAN tablets nearly always use it and it keeps
            // the candidate list (and connectivity checks) short.
            .filter(|ip| ip.is_ipv4())
            .filter(|ip| self.include_loopback || !ip.is_loopback())
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// An encoded video access unit (Annex-B for H.264/HEVC, OBU stream for AV1).
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Arc<[u8]>,
    /// Capture time on the host's monotonic clock.
    pub capture_time: Instant,
    pub keyframe: bool,
}

/// A frame reassembled by the receiver.
#[derive(Debug, Clone)]
pub struct ReceivedFrame {
    pub data: Arc<[u8]>,
    /// RTP media time in 90 kHz units.
    pub rtp_time: u64,
    pub arrival: Instant,
    pub keyframe: bool,
    /// False if packets were lost before this frame; the decoder may show
    /// artefacts until the next keyframe.
    pub contiguous: bool,
}

/// Transport statistics, emitted roughly once per second.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TransportStats {
    pub rtt: Option<Duration>,
    /// Fraction of packets lost (0.0..=1.0) in the media direction.
    pub loss: Option<f32>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub bitrate_estimate_bps: Option<u64>,
    pub nacks: u64,
    pub plis: u64,
    /// Receiver jitter in RTP ticks (90 kHz), if known.
    pub jitter: Option<u32>,
}

#[derive(Debug)]
pub enum TransportEvent {
    Connected {
        remote: Option<SocketAddr>,
    },
    /// ICE lost connectivity. The transport keeps trying for a short grace
    /// period before emitting [`TransportEvent::Closed`].
    Interrupted,
    Resumed,
    /// Sender only: new target bitrate from congestion control.
    BitrateEstimate(u64),
    /// Sender only: the receiver lost sync and needs an IDR.
    KeyframeRequested,
    /// Receiver only.
    Frame(ReceivedFrame),
    Stats(TransportStats),
    /// Terminal. The handle is unusable afterwards.
    Closed {
        reason: String,
    },
}

enum Command {
    SetDesiredBitrate(u64),
    RequestKeyframe,
}

/// Cloneable, thread-safe handle the encoder uses to hand frames to the
/// network task.
#[derive(Clone)]
pub struct FrameSender {
    frames: mpsc::Sender<EncodedFrame>,
    dropped: Arc<std::sync::atomic::AtomicU64>,
}

impl FrameSender {
    /// Queues an encoded frame for sending. Never blocks: when the queue is
    /// full the frame is dropped and `false` returned, and the caller must
    /// force a keyframe so the receiver recovers.
    pub fn send_frame(&self, frame: EncodedFrame) -> bool {
        match self.frames.try_send(frame) {
            Ok(()) => true,
            Err(_) => {
                self.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                false
            }
        }
    }

    pub fn frames_dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Handle to a running media peer. Dropping it shuts the peer down.
pub struct MediaHandle {
    commands: mpsc::Sender<Command>,
    frames: FrameSender,
    pub events: mpsc::Receiver<TransportEvent>,
}

impl MediaHandle {
    pub fn send_frame(&self, frame: EncodedFrame) -> bool {
        self.frames.send_frame(frame)
    }

    pub fn frame_sender(&self) -> FrameSender {
        self.frames.clone()
    }

    pub fn frames_dropped(&self) -> u64 {
        self.frames.frames_dropped()
    }

    /// Upper bound the bandwidth estimator should probe towards.
    pub fn set_desired_bitrate(&self, bps: u64) {
        let _ = self.commands.try_send(Command::SetDesiredBitrate(bps));
    }

    /// Receiver: ask the sender for a keyframe (throttled internally).
    pub fn request_keyframe(&self) {
        let _ = self.commands.try_send(Command::RequestKeyframe);
    }
}

#[derive(Debug, Clone)]
pub struct SenderConfig {
    pub codec: Codec,
    pub start_bitrate_bps: u64,
    pub max_bitrate_bps: u64,
    pub interfaces: InterfacePolicy,
}

/// Host side before the answer arrived.
pub struct PendingSender {
    rtc: Rtc,
    pending: SdpPendingOffer,
    sockets: Vec<UdpSocket>,
    mid: Mid,
    codec: Codec,
    max_bitrate_bps: u64,
}

fn rtc_config(codec: Codec) -> RtcConfig {
    let mut cfg = RtcConfig::new();
    // LAN timings: notice a vanished peer within a few seconds instead of the
    // internet-oriented defaults (tens of seconds).
    cfg.set_initial_stun_rto(Duration::from_millis(200));
    cfg.set_max_stun_rto(Duration::from_millis(1000));
    cfg.set_max_stun_retransmits(5);
    let cfg = cfg.clear_codecs().set_send_buffer_video(4000).set_stats_interval(Some(Duration::from_secs(1)));
    match codec {
        Codec::H264 => cfg.enable_h264(true),
        Codec::H265 => cfg.enable_h265(true),
        Codec::Av1 => cfg.enable_av1(true),
    }
}

fn rtc_codec(codec: Codec) -> RtcCodec {
    match codec {
        Codec::H264 => RtcCodec::H264,
        Codec::H265 => RtcCodec::H265,
        Codec::Av1 => RtcCodec::Av1,
    }
}

/// Binds one UDP socket per interface, all on the same port when possible so
/// that a single port can be advertised to peers behind NAT (emulator).
async fn bind_sockets(policy: &InterfacePolicy) -> Result<Vec<UdpSocket>, TransportError> {
    let addrs = policy.addresses();
    let mut sockets = Vec::new();
    let mut port = 0u16;
    for ip in addrs {
        let sock = match UdpSocket::bind(SocketAddr::new(ip, port)).await {
            Ok(s) => s,
            Err(_) if port != 0 => match UdpSocket::bind(SocketAddr::new(ip, 0)).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(%ip, error = %e, "skipping interface");
                    continue;
                }
            },
            Err(e) => {
                tracing::debug!(%ip, error = %e, "skipping interface");
                continue;
            }
        };
        if port == 0 {
            port = sock.local_addr()?.port();
        }
        sockets.push(sock);
    }
    if sockets.is_empty() {
        return Err(TransportError::NoInterfaces);
    }
    Ok(sockets)
}

fn add_candidates(rtc: &mut Rtc, sockets: &[UdpSocket]) -> Result<(), TransportError> {
    for s in sockets {
        let addr = s.local_addr()?;
        match Candidate::host(addr, "udp") {
            Ok(c) => {
                rtc.add_local_candidate(c);
            }
            Err(e) => tracing::debug!(%addr, error = %e, "rejected local candidate"),
        }
    }
    Ok(())
}

fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| str0m::crypto::from_feature_flags().install_process_default());
}

impl PendingSender {
    /// Creates the host's offer. Returns the SDP to send to the client.
    pub async fn create(config: SenderConfig) -> Result<(Self, String), TransportError> {
        install_crypto();
        let sockets = bind_sockets(&config.interfaces).await?;
        let mut rtc = rtc_config(config.codec)
            .enable_bwe(Some(str0m::bwe::Bitrate::bps(config.start_bitrate_bps)))
            .build(Instant::now());
        add_candidates(&mut rtc, &sockets)?;
        let mut change = rtc.sdp_api();
        let mid = change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let (offer, pending) = change.apply().expect("offer has one media");
        let sdp = offer.to_sdp_string();
        Ok((
            PendingSender { rtc, pending, sockets, mid, codec: config.codec, max_bitrate_bps: config.max_bitrate_bps },
            sdp,
        ))
    }

    /// Applies the client's answer and starts the I/O task.
    pub fn accept_answer(mut self, answer_sdp: &str) -> Result<MediaHandle, TransportError> {
        let answer = SdpAnswer::from_sdp_string(answer_sdp).map_err(|e| TransportError::Sdp(e.to_string()))?;
        self.rtc.sdp_api().accept_answer(self.pending, answer)?;
        self.rtc.bwe().set_desired_bitrate(str0m::bwe::Bitrate::bps(self.max_bitrate_bps));
        let codec = self.codec;
        Ok(spawn_driver(self.rtc, self.sockets, self.mid, Role::Sender { codec, pt: None }))
    }
}

#[derive(Debug, Clone)]
pub struct ReceiverConfig {
    pub codec: Codec,
    pub interfaces: InterfacePolicy,
    /// Address the client used to reach the host's control port. Added as an
    /// extra remote candidate (with the ports from the offer) so connectivity
    /// works through the emulator NAT and other address-translating setups.
    pub host_hint: Option<IpAddr>,
}

/// Tablet side: accepts the host's offer and starts the I/O task. Returns the
/// SDP answer to send back.
pub async fn accept_offer(config: ReceiverConfig, offer_sdp: &str) -> Result<(MediaHandle, String), TransportError> {
    install_crypto();
    let sockets = bind_sockets(&config.interfaces).await?;
    let mut rtc = rtc_config(config.codec).build(Instant::now());
    add_candidates(&mut rtc, &sockets)?;
    let offer = SdpOffer::from_sdp_string(offer_sdp).map_err(|e| TransportError::Sdp(e.to_string()))?;
    let answer = rtc.sdp_api().accept_offer(offer)?;
    if let Some(ip) = config.host_hint {
        for port in candidate_ports(offer_sdp) {
            if let Ok(c) = Candidate::host(SocketAddr::new(ip, port), "udp") {
                rtc.add_remote_candidate(c);
            }
        }
    }
    let mid = answer_mid(&answer).ok_or(TransportError::CodecNotNegotiated(config.codec))?;
    let handle = spawn_driver(rtc, sockets, mid, Role::Receiver);
    Ok((handle, answer.to_sdp_string()))
}

fn answer_mid(answer: &SdpAnswer) -> Option<Mid> {
    sdp_util::first_mid(&answer.to_sdp_string()).map(|m| Mid::from(m.as_str()))
}

enum Role {
    Sender { codec: Codec, pt: Option<str0m::media::Pt> },
    Receiver,
}

struct Datagram {
    data: Vec<u8>,
    source: SocketAddr,
    destination: SocketAddr,
}

fn spawn_driver(rtc: Rtc, sockets: Vec<UdpSocket>, mid: Mid, role: Role) -> MediaHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let (frame_tx, frame_rx) = mpsc::channel(FRAME_QUEUE);
    let (evt_tx, evt_rx) = mpsc::channel(64);
    let sockets: Vec<Arc<UdpSocket>> = sockets.into_iter().map(Arc::new).collect();
    let (net_tx, net_rx) = mpsc::channel::<Datagram>(512);

    let mut readers = Vec::new();
    for s in &sockets {
        let s = s.clone();
        let tx = net_tx.clone();
        readers.push(tokio::spawn(async move {
            let local = match s.local_addr() {
                Ok(a) => a,
                Err(_) => return,
            };
            let mut buf = vec![0u8; 2048];
            loop {
                match s.recv_from(&mut buf).await {
                    Ok((n, source)) => {
                        let dg = Datagram { data: buf[..n].to_vec(), source, destination: local };
                        if tx.send(dg).await.is_err() {
                            return;
                        }
                    }
                    // Windows reports ICMP port-unreachable as a recv error
                    // (WSAECONNRESET); it is not fatal for a UDP socket.
                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                    Err(e) => {
                        tracing::warn!(%local, error = %e, "udp receive failed");
                        return;
                    }
                }
            }
        }));
    }
    drop(net_tx);

    let driver = Driver {
        rtc,
        sockets,
        mid,
        role,
        events: evt_tx,
        last_keyframe_request: None,
        disconnected_since: None,
        connected: false,
        estimate: None,
        stats: TransportStats::default(),
        first_frame_time: None,
    };
    tokio::spawn(async move {
        let reason = driver.run(cmd_rx, frame_rx, net_rx).await;
        for r in readers {
            r.abort();
        }
        reason
    });

    MediaHandle {
        commands: cmd_tx,
        frames: FrameSender { frames: frame_tx, dropped: Default::default() },
        events: evt_rx,
    }
}

struct Driver {
    rtc: Rtc,
    sockets: Vec<Arc<UdpSocket>>,
    mid: Mid,
    role: Role,
    events: mpsc::Sender<TransportEvent>,
    last_keyframe_request: Option<Instant>,
    disconnected_since: Option<Instant>,
    connected: bool,
    estimate: Option<u64>,
    stats: TransportStats,
    first_frame_time: Option<Instant>,
}

impl Driver {
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Command>,
        mut frames: mpsc::Receiver<EncodedFrame>,
        mut net: mpsc::Receiver<Datagram>,
    ) {
        let reason = loop {
            let timeout = match self.drain().await {
                Ok(t) => t,
                Err(e) => break format!("webrtc error: {e}"),
            };
            if let Some(since) = self.disconnected_since
                && since.elapsed() > DISCONNECT_GRACE
            {
                break "peer unreachable".to_string();
            }
            let wait = timeout.saturating_duration_since(Instant::now());
            let input_result = tokio::select! {
                biased;
                cmd = commands.recv() => match cmd {
                    // The MediaHandle was dropped.
                    None => break "closed locally".to_string(),
                    Some(cmd) => self.handle_command(cmd),
                },
                Some(frame) = frames.recv() => self.write_frame(frame),
                dg = net.recv() => match dg {
                    None => break "network sockets closed".to_string(),
                    Some(dg) => self.handle_datagram(dg),
                },
                _ = tokio::time::sleep(wait) => self.rtc.handle_input(Input::Timeout(Instant::now())).map_err(Into::into),
            };
            if let Err(e) = input_result {
                tracing::debug!(error = %e, "input rejected");
            }
        };
        self.rtc.disconnect();
        let _ = self.events.send(TransportEvent::Closed { reason }).await;
    }

    fn handle_datagram(&mut self, dg: Datagram) -> Result<(), TransportError> {
        let Ok(contents) = dg.data.as_slice().try_into() else {
            return Ok(()); // not STUN/DTLS/RTP: ignore
        };
        let input = Input::Receive(
            Instant::now(),
            Receive { proto: Protocol::Udp, source: dg.source, destination: dg.destination, contents },
        );
        if self.rtc.accepts(&input) {
            self.rtc.handle_input(input)?;
        }
        Ok(())
    }

    fn handle_command(&mut self, cmd: Command) -> Result<(), TransportError> {
        match cmd {
            Command::SetDesiredBitrate(bps) => {
                self.rtc.bwe().set_desired_bitrate(str0m::bwe::Bitrate::bps(bps));
                Ok(())
            }
            Command::RequestKeyframe => {
                self.request_keyframe();
                Ok(())
            }
        }
    }

    fn write_frame(&mut self, frame: EncodedFrame) -> Result<(), TransportError> {
        let Role::Sender { codec, pt } = &mut self.role else {
            return Ok(());
        };
        if !self.connected {
            return Ok(()); // str0m would drop it anyway
        }
        let codec = *codec;
        let Some(writer) = self.rtc.writer(self.mid) else {
            return Ok(());
        };
        let pt = match pt {
            Some(pt) => *pt,
            None => {
                let found = writer
                    .payload_params()
                    .find(|p| p.spec().codec == rtc_codec(codec))
                    .map(|p| p.pt())
                    .ok_or(TransportError::CodecNotNegotiated(codec))?;
                *pt = Some(found);
                found
            }
        };
        let base = *self.first_frame_time.get_or_insert(frame.capture_time);
        let micros = frame.capture_time.saturating_duration_since(base).as_micros() as u64;
        let rtp_time = MediaTime::new(micros * 9 / 100, Frequency::NINETY_KHZ);
        writer.write(pt, frame.capture_time, rtp_time, frame.data)?;
        Ok(())
    }

    fn request_keyframe(&mut self) {
        let now = Instant::now();
        if self.last_keyframe_request.is_some_and(|t| now.duration_since(t) < KEYFRAME_REQUEST_INTERVAL) {
            return;
        }
        if let Some(mut writer) = self.rtc.writer(self.mid)
            && writer.request_keyframe(None, KeyframeRequestKind::Pli).is_ok()
        {
            self.last_keyframe_request = Some(now);
        }
    }

    /// Drains `poll_output` until a timeout, dispatching transmits and events.
    async fn drain(&mut self) -> Result<Instant, TransportError> {
        loop {
            match self.rtc.poll_output()? {
                Output::Timeout(t) => return Ok(t),
                Output::Transmit(t) => {
                    if let Some(sock) = self.sockets.iter().find(|s| s.local_addr().ok() == Some(t.source)) {
                        // UDP send errors (e.g. no route) are transient; ICE handles failover.
                        let _ = sock.try_send_to(&t.contents, t.destination);
                    }
                }
                Output::Event(e) => self.handle_event(e).await,
            }
        }
    }

    async fn emit(&self, e: TransportEvent) {
        // Frames use try_send so a slow decoder never stalls the network loop.
        match e {
            TransportEvent::Frame(_) => {
                if self.events.try_send(e).is_err() {
                    tracing::trace!("decoder queue full, dropping frame");
                }
            }
            other => {
                let _ = self.events.send(other).await;
            }
        }
    }

    async fn handle_event(&mut self, e: Event) {
        match e {
            Event::Connected => {
                self.connected = true;
                let remote = None;
                self.emit(TransportEvent::Connected { remote }).await;
            }
            Event::IceConnectionStateChange(state) => match state {
                IceConnectionState::Disconnected if self.disconnected_since.is_none() => {
                    self.disconnected_since = Some(Instant::now());
                    self.emit(TransportEvent::Interrupted).await;
                }
                IceConnectionState::Connected | IceConnectionState::Completed
                    if self.disconnected_since.take().is_some() =>
                {
                    self.emit(TransportEvent::Resumed).await;
                }
                _ => {}
            },
            Event::MediaData(data) => {
                let keyframe = match &data.codec_extra {
                    str0m::format::CodecExtra::H264(x) => x.is_keyframe,
                    str0m::format::CodecExtra::H265(x) => x.is_keyframe,
                    str0m::format::CodecExtra::Av1(x) => x.is_keyframe,
                    _ => false,
                };
                if !data.contiguous {
                    self.request_keyframe();
                }
                let frame = ReceivedFrame {
                    data: data.data,
                    rtp_time: data.time.numer(),
                    arrival: data.network_time,
                    keyframe,
                    contiguous: data.contiguous,
                };
                if self.events.try_send(TransportEvent::Frame(frame)).is_err() {
                    // Decoder is behind: the dropped frame breaks the reference
                    // chain, so ask for a fresh keyframe.
                    self.request_keyframe();
                }
            }
            Event::KeyframeRequest(_) => self.emit(TransportEvent::KeyframeRequested).await,
            Event::EgressBitrateEstimate(kind) => {
                let bps = match kind {
                    str0m::bwe::BweKind::Twcc { estimate, .. } => estimate.as_u64(),
                    str0m::bwe::BweKind::Remb { estimate, .. } => estimate.as_u64(),
                    _ => return,
                };
                self.estimate = Some(bps);
                self.emit(TransportEvent::BitrateEstimate(bps)).await;
            }
            Event::PeerStats(s) => {
                self.stats.rtt = s.rtt.or(self.stats.rtt);
                self.stats.bytes_sent = s.peer_bytes_tx;
                self.stats.bytes_received = s.peer_bytes_rx;
                self.stats.bitrate_estimate_bps = s.bwe_tx.map(|b| b.as_u64()).or(self.estimate);
                // Sender: TWCC feedback-based loss. The receiver's transport-wide
                // estimate counts sequence gaps (e.g. dropped probe padding)
                // and overstates loss; it uses RTCP stream stats instead.
                if let Role::Sender { .. } = self.role {
                    self.stats.loss = s.egress_loss_fraction.or(self.stats.loss);
                }
                self.emit(TransportEvent::Stats(self.stats)).await;
            }
            Event::MediaEgressStats(s) => {
                self.stats.nacks = s.nacks;
                self.stats.plis = s.plis;
                self.stats.rtt = s.rtt.or(self.stats.rtt);
                if let Some(r) = s.remote {
                    self.stats.jitter = Some(r.jitter);
                }
            }
            Event::MediaIngressStats(s) => {
                self.stats.nacks = s.nacks;
                self.stats.plis = s.plis;
                self.stats.jitter = Some(s.jitter);
                self.stats.loss = s.loss.or(self.stats.loss);
            }
            _ => {}
        }
    }
}
