//! Hardware decoding with the NDK `AMediaCodec` API, rendering directly into
//! the app's `SurfaceView` surface. Frames never pass through Kotlin.
//!
//! * Input is fed from the session's decode thread.
//! * A dedicated output thread releases each decoded frame to the surface as
//!   soon as it is ready, so a static desktop doesn't wait for the next input
//!   frame before showing the last one.
//! * The surface can change at any time (rotation, backgrounding). A
//!   generation counter makes the decoder re-create the codec on the new
//!   surface and ask for a keyframe.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;
use secondscrn_diagnostics::Ewma;
use secondscrn_protocol::{Codec, CodecCapability, ColorRange, StreamConfig};
use secondscrn_session::platform::{DecodeOutcome, DecoderStats, PlatformError, PlatformErrorKind, VideoDecoder};
use secondscrn_transport::ReceivedFrame;

// MediaFormat keys/values (android.media.MediaFormat).
const KEY_LOW_LATENCY: &str = "low-latency";
const KEY_PRIORITY: &str = "priority";
const KEY_OPERATING_RATE: &str = "operating-rate";
const KEY_FRAME_RATE: &str = "frame-rate";
const KEY_COLOR_STANDARD: &str = "color-standard";
const KEY_COLOR_RANGE: &str = "color-range";
const KEY_COLOR_TRANSFER: &str = "color-transfer";
const COLOR_STANDARD_BT709: i32 = 1;
const COLOR_RANGE_FULL: i32 = 1;
const COLOR_RANGE_LIMITED: i32 = 2;
const COLOR_TRANSFER_SDR_VIDEO: i32 = 3;
/// Qualcomm's low-latency switch; ignored by other decoders.
const KEY_QTI_LOW_LATENCY: &str = "vendor.qti-ext-dec-low-latency.enable";

#[derive(Default)]
struct Surface {
    window: Option<NativeWindow>,
    generation: u64,
}

fn surface() -> &'static Mutex<Surface> {
    static SURFACE: OnceLock<Mutex<Surface>> = OnceLock::new();
    SURFACE.get_or_init(Default::default)
}

/// Called from Kotlin (via JNI) whenever the video surface changes.
pub fn set_surface(window: Option<NativeWindow>) {
    let mut s = surface().lock().unwrap_or_else(|e| e.into_inner());
    s.window = window;
    s.generation += 1;
    tracing::info!(generation = s.generation, has_surface = s.window.is_some(), "video surface changed");
}

fn current_surface() -> (Option<NativeWindow>, u64) {
    let s = surface().lock().unwrap_or_else(|e| e.into_inner());
    (s.window.clone(), s.generation)
}

fn mime(codec: Codec) -> &'static str {
    codec.mime()
}

/// Software decoders shipped with Android (vs. vendor hardware decoders).
pub fn is_software(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("c2.android.") || n.starts_with("omx.google.") || n.starts_with("c2.google.")
}

/// Pixels per second up to which the decoder runs in low-latency mode. That
/// mode finishes each picture before starting the next, so throughput is
/// 1 / decode time: measured ~65 fps at 3200x2136 (~13 ms per picture) on a
/// Snapdragon tablet. Above this the decoder pipelines pictures, one frame
/// more delay but enough throughput for 120/144 fps.
const LOW_LATENCY_MAX_PIXEL_RATE: u64 = 3200 * 2136 * 60;

fn low_latency(width: u32, height: u32, fps: u32) -> bool {
    width as u64 * height as u64 * fps.max(1) as u64 <= LOW_LATENCY_MAX_PIXEL_RATE
}

fn format_for(codec: Codec, width: u32, height: u32, range: ColorRange, low_latency: bool) -> MediaFormat {
    let mut f = MediaFormat::new();
    f.set_str("mime", mime(codec));
    f.set_i32("width", width as i32);
    f.set_i32("height", height as i32);
    f.set_i32(KEY_LOW_LATENCY, low_latency as i32);
    f.set_i32(KEY_QTI_LOW_LATENCY, low_latency as i32);
    f.set_i32(KEY_PRIORITY, 0); // realtime
    // Fallback colour description for streams whose VUI is ignored.
    f.set_i32(KEY_COLOR_STANDARD, COLOR_STANDARD_BT709);
    f.set_i32(KEY_COLOR_TRANSFER, COLOR_TRANSFER_SDR_VIDEO);
    f.set_i32(
        KEY_COLOR_RANGE,
        match range {
            ColorRange::Limited => COLOR_RANGE_LIMITED,
            ColorRange::Full => COLOR_RANGE_FULL,
        },
    );
    f
}

const PROBE_SIZES: [(u32, u32); 4] = [(3840, 2160), (2560, 1600), (1920, 1080), (1280, 720)];

/// Instantiates and configures each decoder to find what really works.
pub fn probe() -> Vec<CodecCapability> {
    let mut out = Vec::new();
    for codec in Codec::ALL {
        let Some(c) = MediaCodec::from_decoder_type(mime(codec)) else { continue };
        let name = c.name().unwrap_or_else(|_| format!("{codec} decoder"));
        drop(c);
        for (w, h) in PROBE_SIZES {
            // A fresh instance per attempt: a failed configure can leave the
            // codec in an error state.
            let Some(c) = MediaCodec::from_decoder_type(mime(codec)) else { break };
            if c.configure(&format_for(codec, w, h, ColorRange::Limited, true), None, MediaCodecDirection::Decoder)
                .is_ok()
            {
                out.push(CodecCapability {
                    codec,
                    implementation: name.clone(),
                    hardware: !is_software(&name),
                    max_width: w,
                    max_height: h,
                    // The NDK has no performance query; the PC never sends
                    // more than this panel's refresh rate.
                    max_fps: 240,
                });
                break;
            }
        }
    }
    out
}

struct Shared {
    codec: MediaCodec,
    running: AtomicBool,
    rendered: AtomicU64,
    /// Decoded but not shown because a newer picture was ready.
    skipped: AtomicU64,
    stutters: AtomicU64,
    pending: Mutex<VecDeque<(i64, Instant)>>,
    decode_ms: Mutex<Ewma>,
}

// AMediaCodec allows dequeueInputBuffer and dequeueOutputBuffer on different
// threads; all other calls happen on the owning (decode) thread.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

struct Active {
    shared: Arc<Shared>,
    output: Option<std::thread::JoinHandle<()>>,
    generation: u64,
    /// Present when the decoder renders into the upscaler instead of the
    /// window. Dropped after the codec has stopped.
    upscaler: Option<crate::upscale::Upscaler>,
    /// Present when pictures go to the screen through the zero-copy
    /// presenter instead of straight into the window.
    presenter: Option<crate::present::Presenter>,
}

impl Active {
    /// Pictures shown: by the upscaler if it is in use, else by the codec.
    fn rendered(&self) -> u64 {
        match (&self.upscaler, &self.presenter) {
            (Some(u), _) => u.presented.load(Ordering::Relaxed),
            (None, Some(p)) => p.presented.load(Ordering::Relaxed),
            (None, None) => self.shared.rendered.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        self.shared.running.store(false, Ordering::Relaxed);
        if let Some(t) = self.output.take() {
            let _ = t.join();
        }
        let _ = self.shared.codec.stop();
    }
}

pub struct AndroidDecoder {
    config: StreamConfig,
    active: Option<Active>,
    stats: DecoderStats,
    /// Diagnostics: input-buffer wait (s), copy (s), frames, window start.
    timing: (f64, f64, u64, Instant, f64, f64),
}

impl AndroidDecoder {
    pub fn new(config: &StreamConfig) -> Result<Self, PlatformError> {
        let mut d = AndroidDecoder {
            config: config.clone(),
            active: None,
            stats: DecoderStats::default(),
            timing: (0.0, 0.0, 0, Instant::now(), 0.0, 0.0),
        };
        d.ensure_codec()?;
        Ok(d)
    }

    /// (Re)creates the codec when the surface changed. Returns true if a new
    /// codec was created (it needs a keyframe).
    fn ensure_codec(&mut self) -> Result<bool, PlatformError> {
        let (window, generation) = current_surface();
        if self.active.as_ref().is_some_and(|a| a.generation == generation) {
            return Ok(false);
        }
        if let Some(old) = self.active.take() {
            // Stops the old codec; keep its count so stats stay monotonic.
            self.stats.frames_rendered += old.rendered();
            self.stats.frames_dropped += old.shared.skipped.load(Ordering::Relaxed);
            self.stats.stutters += old.shared.stutters.load(Ordering::Relaxed);
        }
        let Some(window) = window else { return Ok(false) };
        let err = |what: &str, e: ndk::media_error::MediaError| {
            PlatformError::new(PlatformErrorKind::Decoder, format!("{what}: {e:?}"))
        };
        let (w, h, fps) = (self.config.width, self.config.height, self.config.max_fps);
        let low_latency = low_latency(w, h, fps);
        // Qualcomm ships a separate low-latency variant of its decoders;
        // prefer it where it exists (and low latency is affordable).
        let codec = MediaCodec::from_decoder_type(mime(self.config.codec))
            .and_then(|c| {
                if !low_latency {
                    return Some(c);
                }
                let variant = format!("{}.low_latency", c.name().ok()?);
                Some(MediaCodec::from_codec_name(&variant).unwrap_or(c))
            })
            .ok_or_else(|| {
                PlatformError::new(PlatformErrorKind::Decoder, format!("no decoder for {}", self.config.codec))
            })?;
        let mut format = format_for(self.config.codec, w, h, self.config.color.range, low_latency);
        if codec.name().is_ok_and(|n| n.starts_with("c2.qti") || n.starts_with("OMX.qcom")) {
            // Qualcomm decoders clock down for content they think is slow
            // (~27 ms per frame at 3200x2136 measured); the highest
            // operating rate keeps them at full speed, as game streamers do.
            format.set_i32(KEY_OPERATING_RATE, i16::MAX as i32);
        }
        // Decoders size their clocks by the declared frame rate (default
        // ~30); at 120 fps and native resolution they fell behind.
        format.set_i32(KEY_FRAME_RATE, self.config.max_fps.max(30) as i32);
        // A window larger than the stream means the UI wants upscaling (it
        // sizes the surface to the view only then).
        let larger = window.width() > w as i32 || window.height() > h as i32;
        let mut upscaler = None;
        let mut output_window = window.clone();
        if larger && crate::upscale::enabled() && crate::upscale::supported() {
            match crate::upscale::Upscaler::start(window.clone(), w, h) {
                Ok((u, reader_window)) => {
                    upscaler = Some(u);
                    output_window = reader_window;
                }
                Err(e) => tracing::warn!(error = %e, "super resolution failed; the compositor scales instead"),
            }
        }
        let mut presenter = None;
        if upscaler.is_none() && crate::present::supported() {
            match crate::present::Presenter::start(window.clone(), w, h) {
                Ok((p, reader_window)) => {
                    presenter = Some(p);
                    output_window = reader_window;
                }
                Err(e) => tracing::warn!(error = %e, "zero-copy presentation failed; decoding into the window"),
            }
        }
        codec
            .configure(&format, Some(&output_window), MediaCodecDirection::Decoder)
            .map_err(|e| err("configure", e))?;
        codec.start().map_err(|e| err("start", e))?;
        tracing::info!(
            decoder = %codec.name().unwrap_or_default(),
            low_latency,
            width = self.config.width,
            height = self.config.height,
            "decoder started"
        );
        let shared = Arc::new(Shared {
            codec,
            running: AtomicBool::new(true),
            rendered: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            stutters: AtomicU64::new(0),
            pending: Mutex::new(VecDeque::new()),
            decode_ms: Mutex::new(Ewma::new(0.1)),
        });
        let s = shared.clone();
        let output = std::thread::Builder::new()
            .name("secondscrn-render".into())
            .spawn(move || render_loop(&s))
            .map_err(|e| PlatformError::new(PlatformErrorKind::Decoder, e.to_string()))?;
        self.active = Some(Active { shared, output: Some(output), generation, upscaler, presenter });
        Ok(true)
    }
}

/// `CLOCK_MONOTONIC` in nanoseconds (the clock `System.nanoTime` and
/// SurfaceFlinger use).
fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

/// Frames closer than this in capture time are continuous motion.
const CONTINUOUS_US: i64 = 25_000;

fn render_loop(s: &Shared) {
    // Previous picture: capture time (µs) and when it was shown.
    let mut last: Option<(i64, Instant)> = None;
    // Diagnostics: outputs, time in release, pictures skipped, window start.
    let mut diag = (0u64, 0f64, 0u64, Instant::now());
    while s.running.load(Ordering::Relaxed) {
        match s.codec.dequeue_output_buffer(Duration::from_millis(50)) {
            Ok(DequeuedOutputBufferInfoResult::Buffer(mut buf)) => {
                // After a burst several pictures are ready at once: show only
                // the newest (lowest latency), skip the others' rendering.
                while let Ok(DequeuedOutputBufferInfoResult::Buffer(newer)) =
                    s.codec.dequeue_output_buffer(Duration::ZERO)
                {
                    let _ = s.codec.release_output_buffer(std::mem::replace(&mut buf, newer), false);
                    s.skipped.fetch_add(1, Ordering::Relaxed);
                }
                let pts = buf.info().presentation_time_us();
                // Present now. With plain `render = true` the frame carries its
                // stream timestamp, and the platform (Xiaomi's at least) paced
                // presentation by it like video playback: frames were held up
                // to seconds after idle gaps or a new session.
                let r0 = Instant::now();
                let _ = s.codec.release_output_buffer_at_time(buf, monotonic_ns());
                diag.1 += r0.elapsed().as_secs_f64();
                diag.0 += 1;
                if diag.3.elapsed() >= Duration::from_secs(2) {
                    let skipped = s.skipped.load(Ordering::Relaxed);
                    tracing::debug!(
                        shown = diag.0,
                        skipped = skipped - diag.2,
                        release_ms = format!("{:.2}", diag.1 * 1000.0 / diag.0 as f64),
                        "decoder output timing"
                    );
                    diag = (0, 0.0, skipped, Instant::now());
                }
                s.rendered.fetch_add(1, Ordering::Relaxed);
                let now = Instant::now();
                if let Some((prev_pts, prev_at)) = last {
                    let captured = pts - prev_pts;
                    let shown = now.duration_since(prev_at).as_micros() as i64;
                    if captured > 0 && captured <= CONTINUOUS_US && shown - captured > captured {
                        s.stutters.fetch_add(1, Ordering::Relaxed);
                    }
                }
                last = Some((pts, now));
                let mut pending = s.pending.lock().unwrap_or_else(|e| e.into_inner());
                while let Some((p, t)) = pending.front().copied() {
                    if p > pts {
                        break;
                    }
                    pending.pop_front();
                    if p == pts {
                        s.decode_ms.lock().unwrap_or_else(|e| e.into_inner()).add(t.elapsed().as_secs_f32() * 1000.0);
                    }
                }
            }
            Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                tracing::debug!("decoder output format: {:?}", s.codec.output_format());
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = ?e, "decoder output error");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

impl VideoDecoder for AndroidDecoder {
    fn decode(&mut self, frame: &ReceivedFrame) -> Result<DecodeOutcome, PlatformError> {
        let recreated = self.ensure_codec()?;
        let Some(active) = &self.active else {
            // No surface (app in background): drop until one returns.
            self.stats.frames_dropped += 1;
            return Ok(DecodeOutcome::NeedKeyframe);
        };
        if recreated && !frame.keyframe {
            self.stats.frames_dropped += 1;
            return Ok(DecodeOutcome::NeedKeyframe);
        }
        let codec = &active.shared.codec;
        let pts_us = (frame.rtp_time as i64) * 1_000_000 / 90_000;
        let t0 = Instant::now();
        let mut input = None;
        // Wait out short stalls (a large keyframe takes longer to decode):
        // dropping a frame here breaks the reference chain and costs
        // everything up to the next keyframe. Sustained backlog is handled by
        // the session's decode-queue age limit.
        for _ in 0..10 {
            match codec.dequeue_input_buffer(Duration::from_millis(20)) {
                Ok(DequeuedInputBufferResult::Buffer(b)) => {
                    input = Some(b);
                    break;
                }
                Ok(DequeuedInputBufferResult::TryAgainLater) => continue,
                Err(e) => {
                    return Err(PlatformError::new(PlatformErrorKind::Decoder, format!("dequeue input: {e:?}")));
                }
            }
        }
        let Some(mut buf) = input else {
            // Decoder is saturated; dropping breaks references.
            self.stats.frames_dropped += 1;
            return Ok(DecodeOutcome::NeedKeyframe);
        };
        let dst = buf.buffer_mut();
        if dst.len() < frame.data.len() {
            self.stats.frames_dropped += 1;
            return Err(PlatformError::new(
                PlatformErrorKind::Decoder,
                format!("frame of {} bytes exceeds decoder buffer of {}", frame.data.len(), dst.len()),
            ));
        }
        let t1 = Instant::now();
        // SAFETY: MaybeUninit<u8> has the layout of u8; every byte written.
        unsafe {
            std::ptr::copy_nonoverlapping(frame.data.as_ptr(), dst.as_mut_ptr() as *mut u8, frame.data.len());
        }
        let t2 = Instant::now();
        active.shared.pending.lock().unwrap_or_else(|e| e.into_inner()).push_back((pts_us, frame.arrival));
        codec
            .queue_input_buffer(buf, 0, frame.data.len(), pts_us as u64, 0)
            .map_err(|e| PlatformError::new(PlatformErrorKind::Decoder, format!("queue input: {e:?}")))?;
        self.stats.frames_decoded += 1;
        let total = t0.elapsed().as_secs_f64();
        self.timing.4 += total;
        self.timing.5 = self.timing.5.max(total);
        self.timing.0 += t1.duration_since(t0).as_secs_f64();
        self.timing.1 += t2.duration_since(t1).as_secs_f64();
        self.timing.2 += 1;
        if self.timing.3.elapsed() >= Duration::from_secs(2) {
            let n = self.timing.2.max(1) as f64;
            tracing::debug!(
                frames = self.timing.2,
                input_wait_ms = format!("{:.1}", self.timing.0 * 1000.0 / n),
                copy_ms = format!("{:.2}", self.timing.1 * 1000.0 / n),
                total_ms = format!("{:.1}", self.timing.4 * 1000.0 / n),
                max_ms = format!("{:.1}", self.timing.5 * 1000.0),
                "decoder input timing"
            );
            self.timing = (0.0, 0.0, 0, Instant::now(), 0.0, 0.0);
        }
        Ok(DecodeOutcome::Ok)
    }

    fn stats(&self) -> DecoderStats {
        let mut s = self.stats;
        if let Some(a) = &self.active {
            s.frames_rendered += a.rendered();
            s.frames_dropped += a.shared.skipped.load(Ordering::Relaxed);
            s.stutters += a.shared.stutters.load(Ordering::Relaxed);
            s.decode_ms_avg = a.shared.decode_ms.lock().unwrap_or_else(|e| e.into_inner()).get();
        }
        s
    }
}
