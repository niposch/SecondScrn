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

fn format_for(codec: Codec, width: u32, height: u32, range: ColorRange) -> MediaFormat {
    let mut f = MediaFormat::new();
    f.set_str("mime", mime(codec));
    f.set_i32("width", width as i32);
    f.set_i32("height", height as i32);
    f.set_i32(KEY_LOW_LATENCY, 1);
    f.set_i32(KEY_QTI_LOW_LATENCY, 1);
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
            if c.configure(&format_for(codec, w, h, ColorRange::Limited), None, MediaCodecDirection::Decoder).is_ok() {
                out.push(CodecCapability {
                    codec,
                    implementation: name.clone(),
                    hardware: !is_software(&name),
                    max_width: w,
                    max_height: h,
                    max_fps: 60,
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
}

impl AndroidDecoder {
    pub fn new(config: &StreamConfig) -> Result<Self, PlatformError> {
        let mut d = AndroidDecoder { config: config.clone(), active: None, stats: DecoderStats::default() };
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
            self.stats.frames_rendered += old.shared.rendered.load(Ordering::Relaxed);
            self.stats.frames_dropped += old.shared.skipped.load(Ordering::Relaxed);
        }
        let Some(window) = window else { return Ok(false) };
        let err = |what: &str, e: ndk::media_error::MediaError| {
            PlatformError::new(PlatformErrorKind::Decoder, format!("{what}: {e:?}"))
        };
        let codec = MediaCodec::from_decoder_type(mime(self.config.codec)).ok_or_else(|| {
            PlatformError::new(PlatformErrorKind::Decoder, format!("no decoder for {}", self.config.codec))
        })?;
        let format = format_for(self.config.codec, self.config.width, self.config.height, self.config.color.range);
        codec.configure(&format, Some(&window), MediaCodecDirection::Decoder).map_err(|e| err("configure", e))?;
        codec.start().map_err(|e| err("start", e))?;
        tracing::info!(
            decoder = %codec.name().unwrap_or_default(),
            width = self.config.width,
            height = self.config.height,
            "decoder started"
        );
        let shared = Arc::new(Shared {
            codec,
            running: AtomicBool::new(true),
            rendered: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            pending: Mutex::new(VecDeque::new()),
            decode_ms: Mutex::new(Ewma::new(0.1)),
        });
        let s = shared.clone();
        let output = std::thread::Builder::new()
            .name("secondscrn-render".into())
            .spawn(move || render_loop(&s))
            .map_err(|e| PlatformError::new(PlatformErrorKind::Decoder, e.to_string()))?;
        self.active = Some(Active { shared, output: Some(output), generation });
        Ok(true)
    }
}

fn render_loop(s: &Shared) {
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
                let _ = s.codec.release_output_buffer(buf, true);
                s.rendered.fetch_add(1, Ordering::Relaxed);
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
        let mut input = None;
        for _ in 0..3 {
            match codec.dequeue_input_buffer(Duration::from_millis(15)) {
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
        for (d, s) in dst.iter_mut().zip(frame.data.iter()) {
            d.write(*s);
        }
        active.shared.pending.lock().unwrap_or_else(|e| e.into_inner()).push_back((pts_us, frame.arrival));
        codec
            .queue_input_buffer(buf, 0, frame.data.len(), pts_us as u64, 0)
            .map_err(|e| PlatformError::new(PlatformErrorKind::Decoder, format!("queue input: {e:?}")))?;
        self.stats.frames_decoded += 1;
        Ok(DecodeOutcome::Ok)
    }

    fn stats(&self) -> DecoderStats {
        let mut s = self.stats;
        if let Some(a) = &self.active {
            s.frames_rendered += a.shared.rendered.load(Ordering::Relaxed);
            s.frames_dropped += a.shared.skipped.load(Ordering::Relaxed);
            s.decode_ms_avg = a.shared.decode_ms.lock().unwrap_or_else(|e| e.into_inner()).get();
        }
        s
    }
}
