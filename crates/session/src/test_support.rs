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

/// Builds an access unit that str0m's packetisers accept for `codec`.
pub fn synthetic_frame(codec: Codec, index: u64, keyframe: bool) -> Vec<u8> {
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
    let n = if codec == Codec::Av1 {
        100
    } else if keyframe {
        4000
    } else {
        600
    };
    f.extend((0..n).map(|i| ((i as u64 + index) % 250 + 1) as u8));
    f
}

#[derive(Default)]
pub struct FakeHostState {
    pub inputs: Mutex<Vec<InputEvent>>,
    pub displays_acquired: AtomicU32,
    pub displays_released: AtomicU32,
    pub keyframe_requests: AtomicU64,
    pub frames_produced: AtomicU64,
    pub last_target: Mutex<Option<(u64, u32)>>,
    /// Every display plan applied, in order.
    pub plans: Mutex<Vec<DisplayPlan>>,
    /// Simulates the user changing the mode in the OS display settings.
    pub external_mode: Mutex<Option<(u32, u32)>>,
    /// The display's scale factor (percent); 0 = default (150).
    pub ui_scale: AtomicU32,
}

pub struct FakeHostPlatform {
    pub state: Arc<FakeHostState>,
    pub fps: u32,
}

impl Default for FakeHostPlatform {
    fn default() -> Self {
        FakeHostPlatform { state: Arc::default(), fps: 60 }
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
        self.info.width = plan.request.width;
        self.info.height = plan.request.height;
        Ok(self.info.clone())
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
        let codec = cfg.codec;
        let interval = Duration::from_secs(1) / self.fps;
        std::thread::spawn(move || {
            let mut i = 0u64;
            while !s.load(Ordering::SeqCst) {
                let key = k.swap(false, Ordering::SeqCst);
                let data = synthetic_frame(codec, i, key);
                if !sink.on_frame(EncodedFrame { data: Arc::from(data), capture_time: Instant::now(), keyframe: key }) {
                    k.store(true, Ordering::SeqCst);
                }
                st.frames_produced.fetch_add(1, Ordering::SeqCst);
                i += 1;
                std::thread::sleep(interval);
            }
        });
        Ok(Box::new(FakePipeline { stop, keyframe, state: self.state.clone() }))
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
}

#[derive(Default)]
pub struct FakeClientPlatform {
    pub state: Arc<FakeClientState>,
    pub name: String,
}

struct FakeDecoder {
    state: Arc<FakeClientState>,
    stats: DecoderStats,
}

impl VideoDecoder for FakeDecoder {
    fn decode(&mut self, frame: &ReceivedFrame) -> Result<DecodeOutcome, PlatformError> {
        self.state.frames_decoded.fetch_add(1, Ordering::SeqCst);
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
        Ok(Box::new(FakeDecoder { state: self.state.clone(), stats: DecoderStats::default() }))
    }
}
