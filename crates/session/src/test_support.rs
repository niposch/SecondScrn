//! Fake platforms for engine tests and headless development: a synthetic
//! "encoder" producing H.264-shaped access units and a decoder that only
//! counts frames.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use secondscrn_protocol::{Codec, CodecCapability, DeviceInfo, InputEvent, StreamConfig};
use secondscrn_transport::{EncodedFrame, ReceivedFrame};

use crate::platform::*;

fn caps() -> Vec<CodecCapability> {
    Codec::ALL
        .iter()
        .map(|c| CodecCapability {
            codec: *c,
            implementation: format!("fake-{c}"),
            hardware: true,
            max_width: 4096,
            max_height: 4096,
            max_fps: 120,
        })
        .collect()
}

/// Origin of the capture timestamps synthetic frames carry.
fn epoch() -> Instant {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Marker before the capture time (16 hex digits, so the payload never
/// contains a start code) in synthetic frames.
const STAMP: &[u8] = b"TS";

/// The capture time a synthetic frame carries.
pub fn capture_time(frame: &[u8]) -> Option<Instant> {
    let at = frame.windows(STAMP.len()).position(|w| w == STAMP)? + STAMP.len();
    let hex = std::str::from_utf8(frame.get(at..at + 16)?).ok()?;
    Some(epoch() + Duration::from_nanos(u64::from_str_radix(hex, 16).ok()?))
}

/// Builds an access unit that str0m's packetisers accept for `codec`.
pub fn synthetic_frame(codec: Codec, index: u64, keyframe: bool) -> Vec<u8> {
    synthetic_frame_sized(codec, index, keyframe, None, Instant::now())
}

/// Like [`synthetic_frame`], `size` bytes long (if given) and stamped with
/// `captured`.
pub fn synthetic_frame_sized(
    codec: Codec,
    index: u64,
    keyframe: bool,
    size: Option<usize>,
    captured: Instant,
) -> Vec<u8> {
    let mut f = Vec::new();
    match codec {
        Codec::H264 => {
            if keyframe {
                f.extend_from_slice(&[
                    0, 0, 0, 1, 0x67, 0x64, 0, 0x28, 0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80, 0, 0, 0, 1, 0x65,
                ]);
            } else {
                f.extend_from_slice(&[0, 0, 0, 1, 0x41]);
            }
        }
        Codec::H265 => {
            if keyframe {
                f.extend_from_slice(&[0, 0, 0, 1, 0x26, 0x01]);
            } else {
                f.extend_from_slice(&[0, 0, 0, 1, 0x02, 0x01]);
            }
        }
        Codec::Av1 => {
            // Temporal delimiter OBU, then a padding OBU carrying the payload.
            f.extend_from_slice(&[0x12, 0x00]);
            f.push(0x7a); // OBU_PADDING with size field
            f.push(100);
        }
    }
    f.extend_from_slice(STAMP);
    f.extend_from_slice(format!("{:016x}", captured.saturating_duration_since(epoch()).as_nanos() as u64).as_bytes());
    let n = if codec == Codec::Av1 {
        100
    } else if keyframe {
        4000
    } else {
        600
    };
    let n = size.map_or(n, |s| s.saturating_sub(f.len()).max(16));
    f.extend((0..n).map(|i| ((i as u64 + index) % 250 + 1) as u8));
    if codec == Codec::Av1 {
        // The padding OBU's size field (one LEB128 byte above): keep it valid.
        f[3] = (f.len() - 4).min(127) as u8;
        f.truncate(4 + f[3] as usize);
    }
    f
}

#[derive(Default)]
pub struct FakeHostState {
    pub inputs: Mutex<Vec<InputEvent>>,
    pub displays_acquired: AtomicU32,
    pub displays_released: AtomicU32,
    pub keyframe_requests: AtomicU64,
    /// Losses recovered from a reference picture instead of a keyframe.
    pub recoveries: AtomicU64,
    pub frames_produced: AtomicU64,
    pub last_target: Mutex<Option<(u64, u32)>>,
    /// Every display plan applied, in order.
    pub plans: Mutex<Vec<DisplayPlan>>,
    /// Simulates the user changing the mode in the OS display settings.
    pub external_mode: Mutex<Option<(u32, u32)>>,
    /// Simulates the OS resetting the refresh rate; a reconfiguration
    /// restores it unless `stubborn_refresh` is set.
    pub external_refresh: Mutex<Option<u32>>,
    pub stubborn_refresh: AtomicBool,
    /// The display's scale factor (percent); 0 = default (150).
    pub ui_scale: AtomicU32,
}

pub struct FakeHostPlatform {
    pub state: Arc<FakeHostState>,
    pub fps: u32,
    /// Frames as large as the bitrate target allows (keyframes 4x), like a
    /// real encoder on full-screen motion; small fixed frames otherwise.
    pub sized_by_bitrate: bool,
    /// The "encoder" supports long-term references (loss recovery without
    /// keyframes).
    pub references: bool,
}

impl Default for FakeHostPlatform {
    fn default() -> Self {
        FakeHostPlatform { state: Arc::default(), fps: 60, sized_by_bitrate: false, references: false }
    }
}

struct FakeLease {
    info: DisplayInfo,
    state: Arc<FakeHostState>,
}

impl DisplayLease for FakeLease {
    fn info(&self) -> DisplayInfo {
        let mut info = self.info.clone();
        if let Some((w, h)) = *self.state.external_mode.lock().unwrap() {
            (info.width, info.height) = (w, h);
        }
        if let Some(hz) = *self.state.external_refresh.lock().unwrap() {
            info.refresh_hz = hz;
        }
        info
    }
    fn ui_scale(&self) -> Option<UiScale> {
        let current = match self.state.ui_scale.load(Ordering::SeqCst) {
            0 => 150,
            v => v,
        };
        Some(UiScale { current, options: vec![100, 125, 150, 175, 200] })
    }
    fn set_ui_scale(&mut self, percent: u32) -> Result<UiScale, PlatformError> {
        self.state.ui_scale.store(percent, Ordering::SeqCst);
        Ok(self.ui_scale().expect("fake scale"))
    }
    fn reconfigure(&mut self, plan: &DisplayPlan) -> Result<DisplayInfo, PlatformError> {
        self.state.plans.lock().unwrap().push(plan.clone());
        self.state.external_mode.lock().unwrap().take();
        if !self.state.stubborn_refresh.load(Ordering::SeqCst) {
            self.state.external_refresh.lock().unwrap().take();
        }
        self.info.width = plan.request.width;
        self.info.height = plan.request.height;
        self.info.refresh_hz = plan.request.refresh_hz;
        Ok(self.info())
    }
}

impl Drop for FakeLease {
    fn drop(&mut self) {
        self.state.displays_released.fetch_add(1, Ordering::SeqCst);
    }
}

struct FakePipeline {
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    state: Arc<FakeHostState>,
    references: bool,
    /// Reference use requested for the next frame.
    next_reference: Arc<Mutex<Option<FrameReference>>>,
}

impl VideoPipeline for FakePipeline {
    fn request_keyframe(&self) {
        self.state.keyframe_requests.fetch_add(1, Ordering::SeqCst);
        self.keyframe.store(true, Ordering::SeqCst);
    }
    fn set_target(&self, bitrate_bps: u64, max_fps: u32) {
        *self.state.last_target.lock().unwrap() = Some((bitrate_bps, max_fps));
    }
    fn stats(&self) -> PipelineStats {
        PipelineStats { encode_fps: 60.0, capture_fps: 60.0, ..Default::default() }
    }
    fn supports_references(&self) -> bool {
        self.references
    }
    fn mark_reference(&self, slot: u32) {
        self.next_reference.lock().unwrap().get_or_insert(FrameReference::Marked { slot, keyframe: false });
    }
    fn recover_from(&self, slot: u32) {
        self.state.recoveries.fetch_add(1, Ordering::SeqCst);
        *self.next_reference.lock().unwrap() = Some(FrameReference::Recovery { slot });
    }
}

impl Drop for FakePipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl HostPlatform for FakeHostPlatform {
    fn encoders(&self) -> Vec<CodecCapability> {
        caps()
    }
    fn driver_status(&self) -> DriverStatus {
        DriverStatus::NotApplicable
    }
    fn acquire_display(&self, plan: &DisplayPlan, _linger: Duration) -> Result<Box<dyn DisplayLease>, PlatformError> {
        self.state.displays_acquired.fetch_add(1, Ordering::SeqCst);
        self.state.plans.lock().unwrap().push(plan.clone());
        let r = &plan.request;
        Ok(Box::new(FakeLease {
            info: DisplayInfo {
                id: "FAKE1".into(),
                name: "Fake display".into(),
                width: r.width,
                height: r.height,
                refresh_hz: r.refresh_hz,
                is_virtual: true,
            },
            state: self.state.clone(),
        }))
    }
    fn start_video(
        &self,
        _d: &DisplayInfo,
        cfg: &StreamConfig,
        sink: Box<dyn PipelineSink>,
    ) -> Result<Box<dyn VideoPipeline>, PlatformError> {
        let stop = Arc::new(AtomicBool::new(false));
        let keyframe = Arc::new(AtomicBool::new(true));
        let (s, k, st) = (stop.clone(), keyframe.clone(), self.state.clone());
        let (references, next_reference) = (self.references, Arc::new(Mutex::new(None)));
        let next = next_reference.clone();
        let codec = cfg.codec;
        let interval = Duration::from_secs(1) / self.fps;
        let (fps, sized, initial) = (self.fps, self.sized_by_bitrate, cfg.initial_bitrate_bps);
        *self.state.last_target.lock().unwrap() = None;
        std::thread::spawn(move || {
            let mut i = 0u64;
            while !s.load(Ordering::SeqCst) {
                // Like the real pipeline: no capture while the network is
                // behind, except a keyframe or recovery frame (the tablet
                // waits for it).
                let urgent =
                    k.load(Ordering::SeqCst) || matches!(*next.lock().unwrap(), Some(FrameReference::Recovery { .. }));
                if !urgent && sink.backlog() > interval {
                    std::thread::sleep(interval);
                    continue;
                }
                let key = k.swap(false, Ordering::SeqCst);
                let size = sized.then(|| {
                    let bps = st.last_target.lock().unwrap().map_or(initial, |(b, _)| b);
                    let bytes = (bps / 8 / fps as u64) as usize;
                    if key { bytes * 4 } else { bytes }
                });
                let captured = Instant::now();
                let data = synthetic_frame_sized(codec, i, key, size, captured);
                let reference = match next.lock().unwrap().take() {
                    _ if key => Some(if references {
                        FrameReference::Marked { slot: 0, keyframe: true }
                    } else {
                        FrameReference::Keyframe
                    }),
                    r => r.filter(|_| references),
                };
                if !sink.on_frame(EncodedFrame { data: Arc::from(data), capture_time: captured, keyframe: key }) {
                    k.store(true, Ordering::SeqCst);
                } else if let Some(r) = reference {
                    sink.on_reference(captured, r);
                }
                st.frames_produced.fetch_add(1, Ordering::SeqCst);
                i += 1;
                std::thread::sleep(interval);
            }
        });
        Ok(Box::new(FakePipeline { stop, keyframe, state: self.state.clone(), references, next_reference }))
    }
    fn inject_input(&self, _d: &DisplayInfo, e: &InputEvent) -> Result<(), PlatformError> {
        self.state.inputs.lock().unwrap().push(e.clone());
        Ok(())
    }
}

#[derive(Default)]
pub struct FakeClientState {
    pub frames_decoded: AtomicU64,
    pub keyframes_decoded: AtomicU64,
    pub decoders_created: AtomicU32,
    /// When each frame was decoded and how long after its capture.
    pub latencies: Mutex<Vec<(Instant, Duration)>>,
}

#[derive(Default)]
pub struct FakeClientPlatform {
    pub state: Arc<FakeClientState>,
    pub name: String,
    /// The decoder stalls (every, for how long), like a busy tablet.
    pub decode_stall: Option<(Duration, Duration)>,
}

struct FakeDecoder {
    state: Arc<FakeClientState>,
    stats: DecoderStats,
    stall: Option<(Duration, Duration)>,
    last_stall: Instant,
}

impl VideoDecoder for FakeDecoder {
    fn decode(&mut self, frame: &ReceivedFrame) -> Result<DecodeOutcome, PlatformError> {
        if let Some((every, stall)) = self.stall
            && self.last_stall.elapsed() >= every
        {
            std::thread::sleep(stall);
            self.last_stall = Instant::now();
        }
        self.state.frames_decoded.fetch_add(1, Ordering::SeqCst);
        if let Some(t) = capture_time(&frame.data) {
            let now = Instant::now();
            self.state.latencies.lock().unwrap().push((now, now.saturating_duration_since(t)));
        }
        if frame.keyframe {
            self.state.keyframes_decoded.fetch_add(1, Ordering::SeqCst);
        }
        self.stats.frames_decoded += 1;
        self.stats.frames_rendered += 1;
        Ok(DecodeOutcome::Ok)
    }
    fn stats(&self) -> DecoderStats {
        self.stats
    }
}

impl ClientPlatform for FakeClientPlatform {
    fn device_info(&self) -> DeviceInfo {
        DeviceInfo {
            name: if self.name.is_empty() { "Test Tablet".into() } else { self.name.clone() },
            model: "Fake".into(),
            app_version: "test".into(),
        }
    }
    fn decoders(&self) -> Vec<CodecCapability> {
        caps()
    }
    fn create_decoder(&self, _c: &StreamConfig) -> Result<Box<dyn VideoDecoder>, PlatformError> {
        self.state.decoders_created.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeDecoder {
            state: self.state.clone(),
            stats: DecoderStats::default(),
            stall: self.decode_stall,
            last_stall: Instant::now(),
        }))
    }
}
