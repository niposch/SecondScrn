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
/// Receive-side reordering window (packets) and how long a hole may block it.
const REORDER_PACKETS: usize = 2048;
const REORDER_TIMEOUT: Duration = Duration::from_millis(250);
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

/// Usable local IPv4 addresses (also shown in the UI for manual connection),
/// the address of the default-route interface first: virtual adapters
/// (Hyper-V, WSL, VirtualBox) are rarely what a tablet can reach.
pub fn local_addresses(include_loopback: bool) -> Vec<IpAddr> {
    let mut list = InterfacePolicy { include_loopback, only: Vec::new() }.addresses();
    if let Some(primary) = primary_address() {
        list.retain(|a| *a != primary);
        list.insert(0, primary);
    }
    list
}

/// Local address used for the default route. Connecting a UDP socket sends
/// nothing; it only asks the OS which interface it would use.
fn primary_address() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?; // TEST-NET-1, never routed
    s.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_unspecified())
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
    RequestKeyframe,
    SetImpairment(Option<Impairment>),
}

/// Network impairment applied to this peer's outgoing packets, for
/// reproducible tests of loss recovery, adaptation and interruptions.
/// Deterministic for a given seed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Impairment {
    /// Probability that a loss event starts at a packet (0..=1).
    pub loss: f32,
    /// Packets dropped per loss event (1 = independent loss, >1 = bursts).
    pub burst: u32,
    /// Constant one-way delay added to every packet.
    pub delay: Duration,
    /// Uniform random extra delay in 0..jitter (may reorder packets).
    pub jitter: Duration,
    /// Bottleneck rate; packets wait in a FIFO and are dropped when it holds
    /// more than `queue_limit` of data.
    pub bandwidth_bps: Option<u64>,
    pub queue_limit: Duration,
    pub seed: u64,
}

impl Impairment {
    /// Drops everything: a complete network outage.
    pub fn outage() -> Self {
        Impairment { loss: 1.0, burst: 1, ..Default::default() }
    }
}

/// Runtime state of an active impairment.
struct Impairer {
    cfg: Impairment,
    rng: u64,
    burst_left: u32,
    /// Time the bottleneck link becomes free.
    link_free: Instant,
    queue: std::collections::BinaryHeap<std::cmp::Reverse<Delayed>>,
    seq: u64,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Delayed {
    at: Instant,
    seq: u64,
    socket: usize,
    destination: SocketAddr,
    data: Vec<u8>,
}

impl Impairer {
    fn new(cfg: Impairment) -> Self {
        Impairer { rng: cfg.seed | 1, cfg, burst_left: 0, link_free: Instant::now(), queue: Default::default(), seq: 0 }
    }

    /// xorshift64*: tiny, deterministic, good enough for test randomness.
    fn rand(&mut self) -> f64 {
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Decides the fate of a packet: `None` = dropped, otherwise when to send.
    fn schedule(&mut self, now: Instant, len: usize) -> Option<Instant> {
        if self.burst_left > 0 {
            self.burst_left -= 1;
            return None;
        }
        if self.cfg.loss > 0.0 && self.rand() < self.cfg.loss as f64 {
            self.burst_left = self.cfg.burst.saturating_sub(1);
            return None;
        }
        let mut at = now;
        if let Some(bps) = self.cfg.bandwidth_bps.filter(|b| *b > 0) {
            let start = self.link_free.max(now);
            if start.saturating_duration_since(now) > self.cfg.queue_limit.max(Duration::from_millis(1)) {
                return None; // bottleneck queue full: tail drop
            }
            let tx = Duration::from_secs_f64(len as f64 * 8.0 / bps as f64);
            self.link_free = start + tx;
            at = self.link_free;
        }
        let jitter = self.cfg.jitter.mul_f64(self.rand());
        Some(at + self.cfg.delay + jitter)
    }
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

    /// Receiver: ask the sender for a keyframe (throttled internally).
    pub fn request_keyframe(&self) {
        let _ = self.commands.try_send(Command::RequestKeyframe);
    }

    /// Applies (or with `None` removes) a network impairment to this peer's
    /// outgoing packets. Testing aid; never used in normal operation.
    pub fn set_impairment(&self, impairment: Option<Impairment>) {
        let _ = self.commands.try_send(Command::SetImpairment(impairment));
    }
}

#[derive(Debug, Clone)]
pub struct SenderConfig {
    pub codec: Codec,
    pub interfaces: InterfacePolicy,
}

/// Host side before the answer arrived.
pub struct PendingSender {
    rtc: Rtc,
    pending: SdpPendingOffer,
    sockets: Vec<UdpSocket>,
    mid: Mid,
    codec: Codec,
}

fn rtc_config(codec: Codec) -> RtcConfig {
    let mut cfg = RtcConfig::new();
    // LAN timings: notice a vanished peer within a few seconds instead of the
    // internet-oriented defaults (tens of seconds).
    cfg.set_initial_stun_rto(Duration::from_millis(200));
    cfg.set_max_stun_rto(Duration::from_millis(1000));
    cfg.set_max_stun_retransmits(5);
    let cfg = cfg
        .clear_codecs()
        .set_send_buffer_video(4000)
        // A desktop keyframe at tablet resolution spans hundreds of packets;
        // the default window (30) silently drops such frames.
        .set_reordering_size_video(REORDER_PACKETS)
        // Give NACK repair a few LAN round trips, then abandon the hole: the
        // gap triggers a keyframe request instead of a stalled picture (the
        // default waits 2 s).
        .set_reordering_timeout_video(Some(REORDER_TIMEOUT))
        .set_stats_interval(Some(Duration::from_secs(1)));
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

/// UDP socket buffer size. A keyframe at tablet resolution is several hundred
/// kilobytes and leaves the PC as one burst; with the OS defaults (Windows:
/// 64 KB send, Android: ~200 KB receive) the tail of every large frame was
/// dropped locally, which looked like Wi-Fi loss and forced more keyframes.
const SOCKET_BUFFER: usize = 8 << 20;

fn enlarge_buffers(sock: &UdpSocket) {
    let s = socket2::SockRef::from(sock);
    let before = (s.send_buffer_size().unwrap_or(0), s.recv_buffer_size().unwrap_or(0));
    let _ = s.set_send_buffer_size(SOCKET_BUFFER);
    let _ = s.set_recv_buffer_size(SOCKET_BUFFER);
    let after = (s.send_buffer_size().unwrap_or(0), s.recv_buffer_size().unwrap_or(0));
    tracing::debug!(?before, ?after, "UDP socket buffers (send, receive)");
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
        enlarge_buffers(&sock);
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
        // No str0m BWE: its pacer sends at 1.1x the GCC estimate, which for an
        // often-idle desktop collapses to the idle rate and then delays every
        // large frame for seconds. Without BWE str0m sends immediately;
        // congestion is handled by the session's RTCP-based controller.
        let mut rtc = rtc_config(config.codec).build(Instant::now());
        add_candidates(&mut rtc, &sockets)?;
        let mut change = rtc.sdp_api();
        let mid = change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let (offer, pending) = change.apply().expect("offer has one media");
        let sdp = offer.to_sdp_string();
        Ok((PendingSender { rtc, pending, sockets, mid, codec: config.codec }, sdp))
    }

    /// Applies the client's answer and starts the I/O task.
    pub fn accept_answer(mut self, answer_sdp: &str) -> Result<MediaHandle, TransportError> {
        let answer = SdpAnswer::from_sdp_string(answer_sdp).map_err(|e| TransportError::Sdp(e.to_string()))?;
        self.rtc.sdp_api().accept_answer(self.pending, answer)?;
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
    let handle = spawn_driver(rtc, sockets, mid, Role::Receiver { codec: config.codec });
    Ok((handle, answer.to_sdp_string()))
}

fn answer_mid(answer: &SdpAnswer) -> Option<Mid> {
    sdp_util::first_mid(&answer.to_sdp_string()).map(|m| Mid::from(m.as_str()))
}

enum Role {
    Sender { codec: Codec, pt: Option<str0m::media::Pt> },
    Receiver { codec: Codec },
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
        impairer: None,
        last_seq: None,
        prefix: None,
        send_drops: 0,
        gaps: 0,
        last_seq_before: None,
        overflowed: false,
        overflows: 0,
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
    impairer: Option<Impairer>,
    /// Highest RTP sequence number of an emitted frame (receiver).
    last_seq: Option<u64>,
    /// A piece without picture data (parameter sets) waiting to be joined
    /// with the picture of the same timestamp (receiver).
    prefix: Option<(u64, Vec<u8>)>,
    /// Packets dropped because the socket's send buffer was full.
    send_drops: u64,
    /// Diagnostics: frames after a gap, and the previous frame's last sequence.
    gaps: u64,
    last_seq_before: Option<u64>,
    /// A frame was dropped because the event queue was full.
    overflowed: bool,
    overflows: u64,
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
            let next = self.flush_delayed().map_or(timeout, |due| due.min(timeout));
            let wait = next.saturating_duration_since(Instant::now());
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
            Command::RequestKeyframe => {
                self.request_keyframe();
                Ok(())
            }
            Command::SetImpairment(cfg) => {
                // Packets already delayed are still delivered.
                let pending = self.impairer.take().map(|i| i.queue).unwrap_or_default();
                self.impairer = cfg.map(Impairer::new);
                if let Some(i) = &mut self.impairer {
                    i.queue = pending;
                } else {
                    for std::cmp::Reverse(d) in pending {
                        let _ = self.sockets[d.socket].try_send_to(&d.data, d.destination);
                    }
                }
                Ok(())
            }
        }
    }

    /// Sends delayed packets that are due; returns when the next one is.
    fn flush_delayed(&mut self) -> Option<Instant> {
        let imp = self.impairer.as_mut()?;
        let now = Instant::now();
        while let Some(std::cmp::Reverse(d)) = imp.queue.peek() {
            if d.at > now {
                return Some(d.at);
            }
            let std::cmp::Reverse(d) = imp.queue.pop().expect("peeked");
            let _ = self.sockets[d.socket].try_send_to(&d.data, d.destination);
        }
        None
    }

    fn transmit(&mut self, source: SocketAddr, destination: SocketAddr, data: &[u8]) {
        let Some(socket) = self.sockets.iter().position(|s| s.local_addr().ok() == Some(source)) else { return };
        match &mut self.impairer {
            None => {
                // UDP send errors (e.g. no route) are transient; ICE handles
                // failover. A full send buffer drops the packet: count it.
                if let Err(e) = self.sockets[socket].try_send_to(data, destination)
                    && e.kind() == std::io::ErrorKind::WouldBlock
                {
                    self.send_drops += 1;
                    if self.send_drops.is_power_of_two() {
                        tracing::warn!(drops = self.send_drops, "UDP send buffer full, packets dropped");
                    }
                }
            }
            Some(imp) => {
                let now = Instant::now();
                if let Some(at) = imp.schedule(now, data.len()) {
                    imp.seq += 1;
                    let seq = imp.seq;
                    imp.queue.push(std::cmp::Reverse(Delayed { at, seq, socket, destination, data: data.to_vec() }));
                }
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
                    self.transmit(t.source, t.destination, &t.contents);
                    self.flush_delayed();
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
                // str0m doesn't flag every gap (frames it could never
                // complete are simply not emitted), so check RTP sequence
                // continuity as well.
                let (start, end) = (**data.seq_range.start(), **data.seq_range.end());
                self.last_seq_before = self.last_seq;
                let gap = self.last_seq.is_some_and(|last| start > last + 1);
                self.last_seq = Some(self.last_seq.map_or(end, |l| l.max(end)));
                let contiguous = data.contiguous && !gap && !std::mem::take(&mut self.overflowed);
                if !contiguous {
                    self.gaps += 1;
                    if self.gaps.is_power_of_two() {
                        tracing::warn!(
                            gaps = self.gaps,
                            str0m_contiguous = data.contiguous,
                            start,
                            end,
                            last = ?self.last_seq_before,
                            "frame after a gap"
                        );
                    }
                    self.request_keyframe();
                }
                let rtp_time = data.time.numer();
                // A piece without a picture (e.g. parameter sets sent ahead of a
                // fragmented slice) can be delivered on its own; decoding it as a
                // frame corrupts the picture. Join it with the picture that
                // follows it under the same timestamp.
                let mut payload = data.data;
                if let Role::Receiver { codec } = self.role {
                    if !secondscrn_graphics::bitstream::has_picture(codec, &payload) {
                        self.prefix = Some((rtp_time, payload.to_vec()));
                        return;
                    }
                    if let Some((t, mut prefix)) = self.prefix.take()
                        && t == rtp_time
                    {
                        prefix.extend_from_slice(&payload);
                        payload = prefix.into();
                    }
                }
                let keyframe = keyframe
                    || matches!(self.role, Role::Receiver { codec } if secondscrn_graphics::bitstream::is_keyframe(codec, &payload));
                let frame = ReceivedFrame { data: payload, rtp_time, arrival: data.network_time, keyframe, contiguous };
                if self.events.try_send(TransportEvent::Frame(frame)).is_err() {
                    // The client is behind: the dropped frame breaks the
                    // reference chain. Mark what follows as after a gap (so it
                    // isn't decoded against a missing picture) and ask for a
                    // fresh keyframe.
                    self.overflowed = true;
                    self.overflows += 1;
                    if self.overflows.is_power_of_two() {
                        tracing::warn!(overflows = self.overflows, "frame dropped: event queue full");
                    }
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
                // Loss comes from RTCP stream stats on both sides (receiver
                // reports / own reception); the transport-wide figures count
                // sequence gaps such as dropped padding and overstate it.
                self.emit(TransportEvent::Stats(self.stats)).await;
            }
            Event::MediaEgressStats(s) => {
                self.stats.nacks = s.nacks;
                self.stats.plis = s.plis;
                self.stats.rtt = s.rtt.or(self.stats.rtt);
                self.stats.loss = s.loss.or(self.stats.loss);
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
