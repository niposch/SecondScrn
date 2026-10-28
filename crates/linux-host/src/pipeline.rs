//! Capture → encode with GStreamer.
//!
//! Two pipelines with the frame pacing in between, as on Windows:
//!
//! * **capture:** `pipewiresrc` on the portal's stream (or a test pattern)
//!   into an app sink that keeps only the newest picture. The caps request
//!   the stream's size, which is what makes the compositor size the virtual
//!   monitor.
//! * **encode:** an app source fed on a fixed clock at the stream's frame
//!   rate with the newest picture, converted on the GPU and encoded
//!   ([`crate::encoders`]); access units go to the network.
//!
//! Between them the worker skips a capture while the network is behind
//! (instead of queueing frames that could only arrive late), repeats the
//! last picture as a keep-alive on a still screen and forces keyframes on
//! request.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use secondscrn_diagnostics::{Ewma, RateCounter};
use secondscrn_protocol::StreamConfig;
use secondscrn_session::platform::{PipelineSink, PipelineStats, PlatformError, PlatformErrorKind, VideoPipeline};
use secondscrn_transport::EncodedFrame;

use crate::encoders::{self, Encoder};

/// A still screen is repeated this often: keeps the Wi-Fi link awake and the
/// tablet's picture refreshed (a repeated frame is ~100 bytes).
const KEEPALIVE: Duration = Duration::from_millis(200);

/// Where pictures come from.
#[derive(Clone)]
pub enum Source {
    /// The portal's virtual monitor stream.
    Portal { fd: Arc<OwnedFd>, node: u32 },
    /// A moving test pattern (development, tests without a desktop).
    TestPattern,
}

enum Ctl {
    Keyframe,
    Target(u64, u32),
}

pub struct LinuxPipeline {
    ctl: mpsc::Sender<Ctl>,
    stop: Arc<AtomicBool>,
    stats: Arc<Mutex<PipelineStats>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VideoPipeline for LinuxPipeline {
    fn request_keyframe(&self) {
        let _ = self.ctl.send(Ctl::Keyframe);
    }
    fn set_target(&self, bitrate_bps: u64, max_fps: u32) {
        let _ = self.ctl.send(Ctl::Target(bitrate_bps, max_fps));
    }
    fn stats(&self) -> PipelineStats {
        *self.stats.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for LinuxPipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn err(kind: PlatformErrorKind, e: impl std::fmt::Display) -> PlatformError {
    PlatformError::new(kind, e.to_string())
}

fn capture_description(source: &Source, width: u32, height: u32, fps: u32) -> String {
    match source {
        // The size in the caps is the virtual monitor's: the compositor
        // creates the monitor for the size the stream negotiates. Any
        // frame rate; pictures come when the screen changes. A desktop
        // that insists on its own size is scaled (on the CPU) instead of
        // failing.
        Source::Portal { fd, node } => {
            let fd = fd.as_raw_fd();
            let dup = unsafe { libc_dup(fd) };
            format!(
                "pipewiresrc name=cap fd={dup} path={node} do-timestamp=true keepalive-time={} ! \
                 capsfilter caps=\"video/x-raw,width={width},height={height};video/x-raw\" ! videoscale ! \
                 video/x-raw,width={width},height={height},pixel-aspect-ratio=1/1 ! appsink name=frames max-buffers=1 \
                 drop=true sync=false",
                KEEPALIVE.as_millis()
            )
        }
        Source::TestPattern => format!(
            "videotestsrc name=cap is-live=true pattern=ball ! video/x-raw,format=BGRx,width={width},height={height},\
             framerate={fps}/1 ! appsink name=frames max-buffers=1 drop=true sync=false"
        ),
    }
}

/// `dup(2)`: `pipewiresrc` takes ownership of the descriptor it is given.
unsafe fn libc_dup(fd: i32) -> i32 {
    unsafe extern "C" {
        fn dup(fd: i32) -> i32;
    }
    unsafe { dup(fd) }
}

impl LinuxPipeline {
    pub fn start(
        source: Source,
        config: &StreamConfig,
        encoder: Encoder,
        sink: Box<dyn PipelineSink>,
    ) -> Result<Self, PlatformError> {
        gst::init().map_err(|e| err(PlatformErrorKind::Encoder, e))?;
        let (w, h) = (config.width & !1, config.height & !1);
        let fps = config.max_fps.max(1);
        let capture = gst::parse::launch(&capture_description(&source, w, h, fps))
            .map_err(|e| err(PlatformErrorKind::Capture, format!("capture pipeline: {e}")))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| err(PlatformErrorKind::Capture, "capture is not a pipeline"))?;
        let frames = capture
            .by_name("frames")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| err(PlatformErrorKind::Capture, "no frame sink"))?;
        let encode = gst::parse::launch(&format!(
            "appsrc name=src is-live=true format=time do-timestamp=false ! {} ! appsink name=out sync=false",
            encoder.chain()
        ))
        .map_err(|e| err(PlatformErrorKind::Encoder, format!("{}: {e}", encoder.factory)))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| err(PlatformErrorKind::Encoder, "encode is not a pipeline"))?;
        let src = encode
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .ok_or_else(|| err(PlatformErrorKind::Encoder, "no app source"))?;
        let enc = encode.by_name("enc").ok_or_else(|| err(PlatformErrorKind::Encoder, "no encoder element"))?;
        let out = encode
            .by_name("out")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| err(PlatformErrorKind::Encoder, "no output sink"))?;
        encoder.configure(&enc, config.initial_bitrate_bps, fps);

        let stats = Arc::new(Mutex::new(PipelineStats {
            bitrate_target_bps: config.initial_bitrate_bps,
            fps_target: fps,
            ..Default::default()
        }));
        let sink: Arc<dyn PipelineSink> = Arc::from(sink);
        let start = Instant::now();
        let force_keyframe = Arc::new(AtomicBool::new(true));
        let encoded = Arc::new(Mutex::new(Encoded::default()));
        {
            let (sink, stats, force, encoded) = (sink.clone(), stats.clone(), force_keyframe.clone(), encoded.clone());
            let codec = config.codec;
            out.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |appsink| {
                        let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                        let now = Instant::now();
                        let pts = buffer.pts().map(|p| p.nseconds());
                        let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                        let data = secondscrn_graphics::bitstream::strip_delimiters(codec, map.as_slice());
                        let capture_time;
                        {
                            let mut e = encoded.lock().unwrap_or_else(|e| e.into_inner());
                            capture_time = e.captured(pts).unwrap_or(now);
                            e.rate.tick(now);
                            e.ms.add(now.saturating_duration_since(capture_time).as_secs_f32() * 1000.0);
                        }
                        let frame = EncodedFrame { data: Arc::from(data), capture_time, keyframe };
                        let mut s = stats.lock().unwrap_or_else(|e| e.into_inner());
                        s.frames_encoded += 1;
                        if keyframe {
                            s.keyframes += 1;
                        }
                        drop(s);
                        if !sink.on_frame(frame) {
                            // Network queue full: the next frame must be a keyframe.
                            force.store(true, Ordering::Relaxed);
                            stats.lock().unwrap_or_else(|e| e.into_inner()).frames_skipped += 1;
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }
        capture
            .set_state(gst::State::Playing)
            .map_err(|_| err(PlatformErrorKind::Capture, "the capture pipeline did not start"))?;

        let (ctl, ctl_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            capture,
            frames,
            encode,
            src,
            enc,
            encoder,
            sink,
            stats: stats.clone(),
            force_keyframe,
            encoded,
            start,
            fps,
            ctl: ctl_rx,
            stop: stop.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("secondscrn-pipeline".into())
            .spawn(move || worker.run())
            .map_err(|e| err(PlatformErrorKind::Other, e))?;
        Ok(LinuxPipeline { ctl, stop, stats, thread: Some(thread) })
    }
}

#[derive(Default)]
struct Encoded {
    rate: RateCounter,
    ms: EwmaDefault,
    /// Pictures in the encoder: (input PTS, capture time). Encoders may
    /// shift timestamps (x264 adds 1000 hours so DTS stays positive), so
    /// the offset is learnt from the first output; without B-frames
    /// pictures come out in input order.
    pending: std::collections::VecDeque<(u64, Instant)>,
    offset: Option<i128>,
}

impl Encoded {
    fn push(&mut self, pts: u64, captured: Instant) {
        self.pending.push_back((pts, captured));
        // Pictures the encoder dropped would otherwise pile up.
        while self.pending.len() > 64 {
            self.pending.pop_front();
        }
    }

    /// The capture time of the picture an output buffer with `pts` encodes.
    fn captured(&mut self, pts: Option<u64>) -> Option<Instant> {
        let pts = pts? as i128;
        let offset = *self.offset.get_or_insert_with(|| pts - self.pending.front().map_or(pts, |p| p.0 as i128));
        let input = pts - offset;
        while let Some(&(p, t)) = self.pending.front() {
            if (p as i128) < input {
                self.pending.pop_front();
                continue;
            }
            if p as i128 == input {
                self.pending.pop_front();
                return Some(t);
            }
            return None;
        }
        None
    }
}

/// `Ewma` with the smoothing used for encode times.
struct EwmaDefault(Ewma);

impl Default for EwmaDefault {
    fn default() -> Self {
        EwmaDefault(Ewma::new(0.1))
    }
}

impl EwmaDefault {
    fn add(&mut self, v: f32) {
        self.0.add(v)
    }
}

struct Worker {
    capture: gst::Pipeline,
    frames: gst_app::AppSink,
    encode: gst::Pipeline,
    src: gst_app::AppSrc,
    enc: gst::Element,
    encoder: Encoder,
    sink: Arc<dyn PipelineSink>,
    stats: Arc<Mutex<PipelineStats>>,
    force_keyframe: Arc<AtomicBool>,
    encoded: Arc<Mutex<Encoded>>,
    start: Instant,
    fps: u32,
    ctl: mpsc::Receiver<Ctl>,
    stop: Arc<AtomicBool>,
}

impl Worker {
    fn run(self) {
        if let Err(e) = self.pump() {
            tracing::warn!(error = %e.message, "pipeline stopped");
            self.sink.on_error(e);
        }
        let _ = self.encode.set_state(gst::State::Null);
        let _ = self.capture.set_state(gst::State::Null);
    }

    fn pump(&self) -> Result<(), PlatformError> {
        let capture_bus = self.capture.bus().ok_or_else(|| err(PlatformErrorKind::Capture, "no bus"))?;
        let encode_bus = self.encode.bus().ok_or_else(|| err(PlatformErrorKind::Encoder, "no bus"))?;
        let mut fps = self.fps;
        let mut latest: Option<gst::Sample> = None;
        let mut fresh = false;
        let mut encoding = false;
        let mut next_tick: Option<Instant> = None;
        let mut last_push: Option<Instant> = None;
        let mut capture_rate = RateCounter::default();
        let mut last_stats = Instant::now();
        let mut first_frame_deadline = Some(Instant::now() + Duration::from_secs(10));
        while !self.stop.load(Ordering::Relaxed) {
            while let Ok(c) = self.ctl.try_recv() {
                match c {
                    Ctl::Keyframe => self.force_keyframe.store(true, Ordering::Relaxed),
                    Ctl::Target(bps, f) => {
                        self.encoder.set_bitrate(&self.enc, bps);
                        fps = f.clamp(1, self.fps);
                        let mut s = self.stats.lock().unwrap_or_else(|e| e.into_inner());
                        s.bitrate_target_bps = bps;
                        s.fps_target = fps;
                    }
                }
            }
            if let Some(e) = encoders::poll_error(&capture_bus, Duration::ZERO) {
                return Err(err(PlatformErrorKind::Capture, e));
            }
            if let Some(e) = encoders::poll_error(&encode_bus, Duration::ZERO) {
                return Err(err(PlatformErrorKind::Encoder, e));
            }
            let iv = Duration::from_secs(1) / fps;
            let now = Instant::now();
            let wait = next_tick.map_or(iv, |t| t.saturating_duration_since(now)).clamp(Duration::from_millis(1), iv);
            if let Some(sample) = self.frames.try_pull_sample(gst::ClockTime::from_nseconds(wait.as_nanos() as u64)) {
                capture_rate.tick(Instant::now());
                if !encoding {
                    // The encoder learns the picture format from the first one.
                    if let Some(caps) = sample.caps() {
                        self.src.set_caps(Some(&caps.to_owned()));
                    }
                    self.encode
                        .set_state(gst::State::Playing)
                        .map_err(|_| err(PlatformErrorKind::Encoder, "the encoder did not start"))?;
                    encoding = true;
                    first_frame_deadline = None;
                }
                latest = Some(sample);
                fresh = true;
            }
            if first_frame_deadline.is_some_and(|t| Instant::now() > t) {
                return Err(err(PlatformErrorKind::Capture, "no picture from the virtual monitor"));
            }
            let now = Instant::now();
            if last_stats.elapsed() >= Duration::from_millis(200) {
                last_stats = now;
                let mut e = self.encoded.lock().unwrap_or_else(|e| e.into_inner());
                let mut s = self.stats.lock().unwrap_or_else(|e| e.into_inner());
                s.capture_fps = capture_rate.rate(now);
                s.encode_fps = e.rate.rate(now);
                s.encode_ms_avg = e.ms.0.get();
            }
            let Some(sample) = latest.as_ref() else { continue };
            if next_tick.is_some_and(|t| now < t) {
                continue;
            }
            let force = self.force_keyframe.load(Ordering::Relaxed);
            let stale = last_push.is_none_or(|t| now.duration_since(t) >= KEEPALIVE);
            if !(fresh || force || stale) {
                next_tick = None; // idle: restart the clock with the next picture
                continue;
            }
            // Fixed send clock: one picture per interval, the newest.
            next_tick = Some(match next_tick {
                Some(t) if now.saturating_duration_since(t) < iv => t + iv,
                _ => now + iv,
            });
            // The network is behind: a frame encoded now would only arrive
            // late. Skip this tick (not a keyframe: the tablet waits for it).
            if !force && self.sink.backlog() > iv {
                self.stats.lock().unwrap_or_else(|e| e.into_inner()).frames_skipped += 1;
                continue;
            }
            let Some(mut buffer) = sample.buffer_owned() else { continue };
            let pts = now.duration_since(self.start).as_nanos() as u64;
            self.encoded.lock().unwrap_or_else(|e| e.into_inner()).push(pts, now);
            {
                let b = buffer.make_mut();
                b.set_pts(gst::ClockTime::from_nseconds(pts));
                b.set_dts(gst::ClockTime::NONE);
                b.set_duration(gst::ClockTime::from_nseconds(iv.as_nanos() as u64));
            }
            if force {
                self.force_keyframe.store(false, Ordering::Relaxed);
                let event = gst_video::DownstreamForceKeyUnitEvent::builder().all_headers(true).build();
                let _ = self.src.send_event(event);
            }
            if self.src.push_buffer(buffer).is_err() {
                return Err(err(PlatformErrorKind::Encoder, "the encoder stopped accepting pictures"));
            }
            fresh = false;
            last_push = Some(now);
        }
        Ok(())
    }
}
