//! `ClientPlatform` for Android (and a decoder-less stand-in elsewhere so
//! the crate builds and its API can be tested on desktop).

use std::sync::Mutex;

use secondscrn_protocol::{CodecCapability, DeviceInfo, StreamConfig};
use secondscrn_session::platform::{ClientPlatform, PlatformError, VideoDecoder};

pub struct AndroidPlatform {
    device: DeviceInfo,
    decoders: Mutex<Option<Vec<CodecCapability>>>,
}

impl AndroidPlatform {
    pub fn new(name: String, model: String, app_version: String) -> Self {
        AndroidPlatform { device: DeviceInfo { name, model, app_version }, decoders: Mutex::new(None) }
    }
}

impl ClientPlatform for AndroidPlatform {
    fn device_info(&self) -> DeviceInfo {
        self.device.clone()
    }

    fn decoders(&self) -> Vec<CodecCapability> {
        let mut cache = self.decoders.lock().unwrap_or_else(|e| e.into_inner());
        cache.get_or_insert_with(probe).clone()
    }

    fn create_decoder(&self, config: &StreamConfig) -> Result<Box<dyn VideoDecoder>, PlatformError> {
        create(config)
    }
}

#[cfg(target_os = "android")]
fn probe() -> Vec<CodecCapability> {
    crate::decoder::probe()
}

#[cfg(target_os = "android")]
fn create(config: &StreamConfig) -> Result<Box<dyn VideoDecoder>, PlatformError> {
    Ok(Box::new(crate::decoder::AndroidDecoder::new(config)?))
}

#[cfg(not(target_os = "android"))]
fn probe() -> Vec<CodecCapability> {
    Vec::new()
}

#[cfg(not(target_os = "android"))]
fn create(_config: &StreamConfig) -> Result<Box<dyn VideoDecoder>, PlatformError> {
    Err(PlatformError::new(secondscrn_session::platform::PlatformErrorKind::Decoder, "decoding requires Android"))
}
