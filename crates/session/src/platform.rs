//! Interfaces implemented by the OS-specific crates.
//!
//! The engines in this crate own all protocol, pairing, negotiation and
//! recovery logic. Platforms only provide media and OS integration:
//!
//! * Windows host: virtual display (VDD), capture (WGC), GPU colour
//!   conversion, hardware encoding (Media Foundation), input injection.
//! * Android client: MediaCodec decoding onto a Surface.
//!
//! All methods may block and are called from dedicated threads, never from
//! the async runtime's worker threads.

use std::time::Duration;

use secondscrn_protocol::{CodecCapability, DeviceInfo, DisplayRequest, InputEvent, StreamConfig};
use secondscrn_transport::{EncodedFrame, ReceivedFrame};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct PlatformError {
    pub kind: PlatformErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformErrorKind {
    Driver,
    Display,
    Capture,
    Encoder,
    Decoder,
    Input,
    Other,
}

impl PlatformError {
    pub fn new(kind: PlatformErrorKind, message: impl Into<String>) -> Self {
        PlatformError {
            kind,
            message: message.into(),
        }
    }
}

/// State of the virtual display driver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DriverStatus {
    Ready {
        detail: String,
    },
    NotInstalled,
    Unavailable {
        reason: String,
    },
    /// The platform streams an existing monitor (development / tests).
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayInfo {
    /// Stable OS identifier (e.g. `\\.\DISPLAY5`).
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub is_virtual: bool,
}

/// A display reserved for one session. Dropping it releases the display; the
/// platform may keep it attached for the linger time given at acquisition.
pub trait DisplayLease: Send {
    fn info(&self) -> DisplayInfo;
    /// Changes the mode (e.g. after the tablet rotated).
    fn reconfigure(&mut self, request: &DisplayRequest) -> Result<DisplayInfo, PlatformError>;
}

/// Receives the output of a running capture/encode pipeline.
pub trait PipelineSink: Send + Sync {
    /// Returns false if the frame was dropped (queue full); the pipeline must
    /// then make its next frame a keyframe.
    fn on_frame(&self, frame: EncodedFrame) -> bool;
    /// The pipeline stopped because of an unrecoverable error.
    fn on_error(&self, error: PlatformError);
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct PipelineStats {
    pub capture_fps: f32,
    pub encode_fps: f32,
    pub encode_ms_avg: f32,
    /// Captured frames skipped because the encoder was busy or frame pacing
    /// dropped them.
    pub frames_skipped: u64,
    pub keyframes: u64,
    pub bitrate_target_bps: u64,
    pub fps_target: u32,
}

/// A running capture → convert → encode pipeline.
pub trait VideoPipeline: Send {
    fn request_keyframe(&self);
    fn set_target(&self, bitrate_bps: u64, max_fps: u32);
    fn stats(&self) -> PipelineStats;
}

pub trait HostPlatform: Send + Sync + 'static {
    /// Hardware encoders verified to work, one entry per codec.
    fn encoders(&self) -> Vec<CodecCapability>;
    fn driver_status(&self) -> DriverStatus;
    fn acquire_display(
        &self,
        request: &DisplayRequest,
        linger: Duration,
    ) -> Result<Box<dyn DisplayLease>, PlatformError>;
    fn start_video(
        &self,
        display: &DisplayInfo,
        config: &StreamConfig,
        sink: Box<dyn PipelineSink>,
    ) -> Result<Box<dyn VideoPipeline>, PlatformError>;
    /// Injects authenticated input targeting `display`.
    fn inject_input(&self, display: &DisplayInfo, event: &InputEvent) -> Result<(), PlatformError>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct DecoderStats {
    pub frames_decoded: u64,
    pub frames_rendered: u64,
    pub frames_dropped: u64,
    pub decode_ms_avg: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeOutcome {
    Ok,
    /// The decoder cannot continue without an IDR (startup, surface change,
    /// corruption). Following non-key frames are skipped until one arrives.
    NeedKeyframe,
}

pub trait VideoDecoder: Send {
    fn decode(&mut self, frame: &ReceivedFrame) -> Result<DecodeOutcome, PlatformError>;
    fn stats(&self) -> DecoderStats;
}

pub trait ClientPlatform: Send + Sync + 'static {
    fn device_info(&self) -> DeviceInfo;
    fn decoders(&self) -> Vec<CodecCapability>;
    fn create_decoder(&self, config: &StreamConfig)
    -> Result<Box<dyn VideoDecoder>, PlatformError>;
}
