//! Capture → convert → encode pipeline on a dedicated thread.
//!
//! Everything stays on the GPU: WGC hands us a BGRA texture, a shader writes
//! NV12 into one of a few ring textures, and the hardware MFT encodes that
//! texture. Only the compressed bitstream reaches the CPU.
//!
//! Frame policy for a desktop:
//! * Frames are encoded only when the screen changes (WGC semantics), at most
//!   `max_fps`. When the encoder is busy, only the newest frame is kept.
//! * After the screen settles, the last frame is re-encoded twice so the
//!   encoder can refine it (static text converges to full sharpness).
//! * A keepalive frame every 200 ms keeps the decoder, statistics and the Wi-Fi
//!   link awake.

use std::collections::VecDeque;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use secondscrn_diagnostics::{Ewma, RateCounter};
use secondscrn_graphics::color::RgbToYuv;
use secondscrn_protocol::StreamConfig;
use secondscrn_session::platform::{PipelineSink, PipelineStats, PlatformError, PlatformErrorKind, VideoPipeline};
use secondscrn_transport::EncodedFrame;

use crate::capture::{MonitorCapture, MonitorInfo};
use crate::convert::{Converter, Nv12Target};
use crate::d3d;
use crate::encoder::{Encoder, EncoderDescriptor, EncoderEvent, EncoderSettings, Reference};
use secondscrn_session::platform::FrameReference;

const RING: usize = 4;
const REFRESH_DELAY: Duration = Duration::from_millis(120);
const REFRESH_COUNT: u32 = 2;
/// Repeat frames while the screen is static. Frequent enough to keep the
/// Wi-Fi link awake: with 1 s the tablet's idle round trip was 61 ms median
/// (181 ms p90), with 200 ms 28 ms (62 ms p90), so the first frame after a
/// sudden change arrives sooner. A repeated static frame is ~100 bytes.
const KEEPALIVE: Duration = Duration::from_millis(200);
/// Keyframe requests within this time after a keyframe are ignored: several
/// receivers' signals (PLI, gap detection, control message) usually report
/// the same loss, and each extra keyframe is a large burst.
const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(500);

enum Msg {
    Frame,
    Encoder(EncoderEvent),
    SetTarget(u64, u32),
    Keyframe,
    MarkReference(u32),
    Recover(u32),
    Stop,
}

pub struct WindowsPipeline {
    tx: mpsc::Sender<Msg>,
    stats: Arc<Mutex<PipelineStats>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Losses are recovered from long-term references.
    references: bool,
}

impl VideoPipeline for WindowsPipeline {
    fn request_keyframe(&self) {
        let _ = self.tx.send(Msg::Keyframe);
    }
    fn supports_references(&self) -> bool {
        self.references
    }
    fn mark_reference(&self, slot: u32) {
        let _ = self.tx.send(Msg::MarkReference(slot));
    }
    fn recover_from(&self, slot: u32) {
        let _ = self.tx.send(Msg::Recover(slot));
    }
    fn set_target(&self, bitrate_bps: u64, max_fps: u32) {
        let _ = self.tx.send(Msg::SetTarget(bitrate_bps, max_fps));
    }
    fn stats(&self) -> PipelineStats {
        *self.stats.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for WindowsPipeline {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn err(kind: PlatformErrorKind, e: impl std::fmt::Display) -> PlatformError {
    PlatformError::new(kind, e.to_string())
}

impl WindowsPipeline {
    pub fn start(
        monitor: MonitorInfo,
        config: StreamConfig,
        encoder: EncoderDescriptor,
        sink: Box<dyn PipelineSink>,
    ) -> Result<Self, PlatformError> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let stats = Arc::new(Mutex::new(PipelineStats::default()));
        let (tx2, stats2) = (tx.clone(), stats.clone());
        let thread = std::thread::Builder::new()
            .name("secondscrn-pipeline".into())
            .spawn(move || {
                let mut worker = match Worker::new(monitor, config, &encoder, tx2, stats2) {
                    Ok(w) => {
                        let _ = ready_tx.send(Ok(w.references));
                        w
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                if let Err(e) = worker.run(&rx, sink.as_ref()) {
                    tracing::warn!(error = %e, "pipeline stopped");
                    sink.on_error(e);
                }
            })
            .map_err(|e| err(PlatformErrorKind::Other, e))?;
        match ready_rx.recv() {
            Ok(Ok(references)) => {
                tracing::info!(references, "loss recovery with long-term references");
                Ok(WindowsPipeline { tx, stats, thread: Some(thread), references })
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(err(PlatformErrorKind::Other, "pipeline thread died during start")),
        }
    }
}

struct Worker {
    capture: MonitorCapture,
    converter: Converter,
    targets: Vec<Nv12Target>,
    encoder: Encoder,
    config: StreamConfig,
    stats: Arc<Mutex<PipelineStats>>,
    // State
    need_input: u32,
    pending_capture: bool,
    force_keyframe: bool,
    next_slot: usize,
    last_slot: Option<usize>,
    last_submit: Option<Instant>,
    last_capture: Option<Instant>,
    last_keyframe: Option<Instant>,
    /// When the next captured frame may be sent (fixed cadence).
    next_tick: Option<Instant>,
    refreshes: u32,
    fps: u32,
    start: Instant,
    /// Submitted frames: timestamp, capture time, reference use.
    in_flight: VecDeque<(i64, Instant, Reference)>,
    /// Losses are recovered from long-term references (verified encoder).
    references: bool,
    /// Reference use requested for the next frame.
    pending_reference: Reference,
    capture_rate: RateCounter,
    encode_rate: RateCounter,
    encode_ms: Ewma,
}

impl Worker {
    fn new(
        monitor: MonitorInfo,
        config: StreamConfig,
        desc: &EncoderDescriptor,
        tx: mpsc::Sender<Msg>,
        stats: Arc<Mutex<PipelineStats>>,
    ) -> Result<Self, PlatformError> {
        let gpu = d3d::create(desc.luid).map_err(|e| err(PlatformErrorKind::Encoder, e))?;
        let frame_tx = tx.clone();
        let capture = MonitorCapture::start(&gpu, &monitor, move || {
            let _ = frame_tx.send(Msg::Frame);
        })
        .map_err(|e| err(PlatformErrorKind::Capture, format!("cannot capture {}: {e:#}", monitor.device)))?;
        let (w, h) = (config.width & !1, config.height & !1);
        let converter = Converter::new(&gpu, &RgbToYuv::for_color(&config.color))
            .map_err(|e| err(PlatformErrorKind::Capture, e))?;
        let targets = (0..RING)
            .map(|_| converter.create_target(w, h))
            .collect::<Result<Vec<_>>>()
            .map_err(|e| err(PlatformErrorKind::Capture, e))?;
        let encoder = Encoder::new(
            &gpu,
            desc,
            EncoderSettings {
                codec: config.codec,
                width: w,
                height: h,
                fps: config.max_fps,
                bitrate_bps: config.initial_bitrate_bps,
                color: config.color,
            },
            move |e| tx.send(Msg::Encoder(e)).is_ok(),
        )
        .map_err(|e| err(PlatformErrorKind::Encoder, format!("{}: {e:#}", desc.name)))?;
        let now = Instant::now();
        let references = encoder.verified_ltr(desc);
        Ok(Worker {
            references,
            pending_reference: Reference::None,
            capture,
            converter,
            targets,
            encoder,
            fps: config.max_fps.max(1),
            config,
            stats,
            need_input: 0,
            pending_capture: true,
            force_keyframe: true,
            next_slot: 0,
            last_slot: None,
            last_submit: None,
            last_capture: None,
            last_keyframe: None,
            next_tick: None,
            refreshes: REFRESH_COUNT,
            start: now,
            in_flight: VecDeque::new(),
            capture_rate: RateCounter::new(now),
            encode_rate: RateCounter::new(now),
            encode_ms: Ewma::new(0.1),
        })
    }

    fn frame_interval(&self) -> Duration {
        Duration::from_secs(1) / self.fps
    }

    fn run(&mut self, rx: &mpsc::Receiver<Msg>, sink: &dyn PipelineSink) -> Result<(), PlatformError> {
        let mut last_stats = Instant::now();
        loop {
            let now = Instant::now();
            let wait = self.next_deadline(now).saturating_duration_since(now).max(Duration::from_millis(1));
            match rx.recv_timeout(wait) {
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
                Ok(Msg::Frame) => {
                    tracing::trace!("frame arrived");
                    self.pending_capture = true;
                    self.capture_rate.tick(Instant::now());
                }
                Ok(Msg::Keyframe) => {
                    if self.last_keyframe.is_some_and(|t| t.elapsed() < KEYFRAME_MIN_INTERVAL) {
                        tracing::trace!("keyframe request ignored, one was just sent");
                    } else {
                        self.force_keyframe = true;
                    }
                }
                Ok(Msg::MarkReference(slot)) => {
                    if self.references && self.pending_reference == Reference::None {
                        self.pending_reference = Reference::Mark(slot);
                    }
                }
                Ok(Msg::Recover(slot)) => {
                    if self.references {
                        self.pending_reference = Reference::Recover(slot);
                    } else {
                        self.force_keyframe = true;
                    }
                }
                Ok(Msg::SetTarget(bps, fps)) => {
                    // The encoder spends `bitrate / configured fps` per frame.
                    // Scaling that up when content changes less often (30 fps
                    // video) was tried and made frames up to 4x larger, which
                    // was visibly slower on Wi-Fi; frame size is kept bounded.
                    self.encoder.set_bitrate(bps);
                    self.fps = fps.clamp(1, self.config.max_fps.max(1));
                }
                Ok(Msg::Encoder(EncoderEvent::NeedInput)) => {
                    tracing::trace!("encoder needs input");
                    self.need_input += 1
                }
                Ok(Msg::Encoder(EncoderEvent::HaveOutput)) => {
                    tracing::trace!("encoder has output");
                    self.collect(sink)?
                }
                Ok(Msg::Encoder(EncoderEvent::Error(e))) => return Err(err(PlatformErrorKind::Encoder, e)),
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.maybe_submit(sink)?;
            if last_stats.elapsed() >= Duration::from_millis(200) {
                last_stats = Instant::now();
                self.publish_stats();
            }
        }
    }

    fn next_deadline(&self, now: Instant) -> Instant {
        let Some(last) = self.last_submit else { return now + Duration::from_millis(50) };
        if self.pending_capture {
            return self.next_tick.unwrap_or(last);
        }
        if self.refreshes < REFRESH_COUNT {
            return last + REFRESH_DELAY;
        }
        last + KEEPALIVE
    }

    fn maybe_submit(&mut self, sink: &dyn PipelineSink) -> Result<(), PlatformError> {
        if self.need_input == 0 {
            return Ok(());
        }
        let now = Instant::now();
        let since = self.last_submit.map(|t| now.duration_since(t));
        if self.pending_capture && self.next_tick.is_none_or(|t| now >= t) {
            // Fixed send clock: one frame per interval, the newest picture.
            // Following the capture rhythm instead (often the main display's
            // 144+ Hz) picked frames unevenly, which showed as stutter.
            let iv = self.frame_interval();
            self.next_tick = Some(match self.next_tick {
                Some(t) if now.saturating_duration_since(t) < iv => t + iv,
                _ => now + iv, // idle before: restart the clock
            });
            // The network is behind: a frame encoded now would only arrive
            // late. Skip this tick; the next one shows the newest picture.
            // Not a keyframe or recovery frame: the tablet waits for it.
            if !self.urgent() && sink.backlog() > iv {
                self.stats.lock().unwrap_or_else(|e| e.into_inner()).frames_skipped += 1;
                return Ok(());
            }
            self.pending_capture = false;
            if let Some(frame) = self.capture.latest().map_err(|e| err(PlatformErrorKind::Capture, e))? {
                let (w, h) = (self.targets[0].width, self.targets[0].height);
                if frame.width & !1 != w || frame.height & !1 != h {
                    return Err(err(
                        PlatformErrorKind::Display,
                        format!("display size changed to {}x{}", frame.width, frame.height),
                    ));
                }
                let slot = self.next_slot;
                self.next_slot = (self.next_slot + 1) % RING;
                let srv = self.converter.source_view(&frame.texture).map_err(|e| err(PlatformErrorKind::Capture, e))?;
                self.converter.convert(&srv, &self.targets[slot]);
                drop(frame);
                self.last_slot = Some(slot);
                self.last_capture = Some(now);
                self.refreshes = 0;
                return self.submit(slot, now);
            }
        }
        let Some(slot) = self.last_slot else { return Ok(()) };
        let since = since.unwrap_or(Duration::MAX);
        if (self.refreshes < REFRESH_COUNT && since >= REFRESH_DELAY) || since >= KEEPALIVE || self.urgent() {
            self.refreshes = self.refreshes.saturating_add(1);
            return self.submit(slot, now);
        }
        Ok(())
    }

    /// A keyframe or recovery frame is due: it goes out right away.
    fn urgent(&self) -> bool {
        self.force_keyframe || matches!(self.pending_reference, Reference::Recover(_))
    }

    fn submit(&mut self, slot: usize, now: Instant) -> Result<(), PlatformError> {
        let ts = (now.duration_since(self.start).as_nanos() / 100) as i64;
        let key = std::mem::take(&mut self.force_keyframe);
        let pending = std::mem::take(&mut self.pending_reference);
        // A keyframe empties the decoder's references; it becomes the first.
        let reference = match () {
            _ if key && self.references => Reference::Mark(0),
            _ if key => Reference::None,
            _ => pending,
        };
        self.encoder
            .encode(&self.targets[slot].texture, ts, key, reference)
            .map_err(|e| err(PlatformErrorKind::Encoder, format!("{e:#}")))?;
        self.need_input -= 1;
        self.last_submit = Some(now);
        self.in_flight.push_back((ts, now, reference));
        if self.in_flight.len() > 16 {
            self.in_flight.pop_front();
        }
        if key {
            self.last_keyframe = Some(now);
            self.stats.lock().unwrap_or_else(|e| e.into_inner()).keyframes += 1;
        }
        Ok(())
    }

    fn collect(&mut self, sink: &dyn PipelineSink) -> Result<(), PlatformError> {
        let Some(out) = self.encoder.collect().map_err(|e| err(PlatformErrorKind::Encoder, format!("{e:#}")))? else {
            return Ok(());
        };
        let now = Instant::now();
        let mut capture_time = now;
        let mut reference = Reference::None;
        while let Some((ts, t, r)) = self.in_flight.front().copied() {
            if ts <= out.time_100ns {
                self.in_flight.pop_front();
                if ts == out.time_100ns {
                    capture_time = t;
                    reference = r;
                    self.encode_ms.add(now.duration_since(t).as_secs_f32() * 1000.0);
                }
            } else {
                break;
            }
        }
        self.encode_rate.tick(now);
        self.stats.lock().unwrap_or_else(|e| e.into_inner()).frames_encoded += 1;
        // Without delimiters every frame starts with picture data or parameter
        // sets that the depacketizer keeps with the picture.
        let data = secondscrn_graphics::bitstream::strip_delimiters(self.config.codec, &out.data);
        let frame = EncodedFrame { data: Arc::from(data), capture_time, keyframe: out.keyframe };
        if !sink.on_frame(frame) {
            // Network queue full: the next frame must be an IDR.
            self.force_keyframe = true;
            self.stats.lock().unwrap_or_else(|e| e.into_inner()).frames_skipped += 1;
            return Ok(());
        }
        let used = match reference {
            Reference::Mark(slot) => Some(FrameReference::Marked { slot, keyframe: out.keyframe }),
            Reference::Recover(slot) if !out.keyframe => Some(FrameReference::Recovery { slot }),
            _ if out.keyframe => Some(FrameReference::Keyframe),
            _ => None,
        };
        if let Some(used) = used {
            sink.on_reference(capture_time, used);
        }
        Ok(())
    }

    fn publish_stats(&mut self) {
        let now = Instant::now();
        let mut s = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        s.capture_fps = self.capture_rate.rate(now);
        s.encode_fps = self.encode_rate.rate(now);
        s.encode_ms_avg = self.encode_ms.get();
        s.bitrate_target_bps = self.encoder.settings.bitrate_bps;
        s.fps_target = self.fps;
    }
}

/// Helper used by the CLI and tests: encodes `seconds` of a monitor into
/// `out`, returning the number of frames.
pub fn record(
    monitor: MonitorInfo,
    config: StreamConfig,
    encoder: EncoderDescriptor,
    seconds: f32,
    out: &mut dyn std::io::Write,
) -> Result<usize> {
    struct Collect(mpsc::Sender<EncodedFrame>);
    impl PipelineSink for Collect {
        fn on_frame(&self, f: EncodedFrame) -> bool {
            self.0.send(f).is_ok()
        }
        fn on_error(&self, e: PlatformError) {
            tracing::error!(error = %e, "pipeline error");
        }
    }
    let (tx, rx) = mpsc::channel();
    let pipeline =
        WindowsPipeline::start(monitor, config, encoder, Box::new(Collect(tx))).map_err(|e| anyhow::anyhow!(e))?;
    let deadline = Instant::now() + Duration::from_secs_f32(seconds);
    let mut n = 0;
    while Instant::now() < deadline {
        if let Ok(f) = rx.recv_timeout(Duration::from_millis(50)) {
            out.write_all(&f.data)?;
            n += 1;
        }
    }
    drop(pipeline);
    while let Ok(f) = rx.try_recv() {
        out.write_all(&f.data)?;
        n += 1;
    }
    out.flush().context("flush")?;
    Ok(n)
}
