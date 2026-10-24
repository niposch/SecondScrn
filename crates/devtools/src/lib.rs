//! A `ClientPlatform` whose "decoder" appends the received bitstream to a
//! file, so streams can be verified with standard tools (ffprobe/ffmpeg).

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use secondscrn_protocol::{Codec, CodecCapability, DeviceInfo, StreamConfig};
use secondscrn_session::platform::{
    ClientPlatform, DecodeOutcome, DecoderStats, PlatformError, PlatformErrorKind, VideoDecoder,
};
use secondscrn_transport::ReceivedFrame;

#[derive(Default)]
pub struct RecordingStats {
    pub frames: AtomicU64,
    pub keyframes: AtomicU64,
    pub bytes: AtomicU64,
    pub last_config: Mutex<Option<StreamConfig>>,
}

pub struct RecordingPlatform {
    pub output: Option<PathBuf>,
    pub stats: Arc<RecordingStats>,
    pub codecs: Vec<Codec>,
    pub name: String,
}

impl RecordingPlatform {
    pub fn new(output: Option<PathBuf>) -> Self {
        RecordingPlatform {
            output,
            stats: Arc::default(),
            codecs: Codec::ALL.to_vec(),
            name: "SecondScrn dev client".into(),
        }
    }
}

struct Recorder {
    file: Option<File>,
    stats: Arc<RecordingStats>,
    own: DecoderStats,
}

impl VideoDecoder for Recorder {
    fn decode(&mut self, frame: &ReceivedFrame) -> Result<DecodeOutcome, PlatformError> {
        if let Some(f) = &mut self.file {
            f.write_all(&frame.data).map_err(|e| PlatformError::new(PlatformErrorKind::Decoder, e.to_string()))?;
        }
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
        if frame.keyframe {
            self.stats.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        self.own.frames_decoded += 1;
        self.own.frames_rendered += 1;
        Ok(DecodeOutcome::Ok)
    }

    fn stats(&self) -> DecoderStats {
        self.own
    }
}

impl ClientPlatform for RecordingPlatform {
    fn device_info(&self) -> DeviceInfo {
        DeviceInfo { name: self.name.clone(), model: "devtools".into(), app_version: env!("CARGO_PKG_VERSION").into() }
    }

    fn decoders(&self) -> Vec<CodecCapability> {
        self.codecs
            .iter()
            .map(|c| CodecCapability {
                codec: *c,
                implementation: "file-recorder".into(),
                hardware: true,
                max_width: 8192,
                max_height: 8192,
                max_fps: 240,
            })
            .collect()
    }

    fn create_decoder(&self, config: &StreamConfig) -> Result<Box<dyn VideoDecoder>, PlatformError> {
        *self.stats.last_config.lock().unwrap() = Some(config.clone());
        let file = match &self.output {
            // Append: a reconfiguration starts a new decoder on the same file.
            Some(p) => Some(
                File::options()
                    .create(true)
                    .append(true)
                    .open(p)
                    .map_err(|e| PlatformError::new(PlatformErrorKind::Decoder, e.to_string()))?,
            ),
            None => None,
        };
        Ok(Box::new(Recorder { file, stats: self.stats.clone(), own: DecoderStats::default() }))
    }
}
