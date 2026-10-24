//! `HostPlatform` implementation for Windows.

use std::sync::Mutex;
use std::time::Duration;

use secondscrn_protocol::{CodecCapability, DisplayRequest, InputEvent, StreamConfig};
use secondscrn_session::platform::{
    DisplayInfo, DisplayLease, DriverStatus, HostPlatform, PipelineSink, PlatformError, PlatformErrorKind,
    VideoPipeline,
};

use crate::encoder::{self, EncoderDescriptor};
use crate::pipeline::WindowsPipeline;
use crate::virtual_display::VirtualDisplayManager;
use crate::{capture, display, input, vdd};

/// Where the stream comes from.
#[derive(Debug, Clone)]
pub enum DisplaySource {
    /// The tablet becomes a new monitor via the Virtual Display Driver.
    Virtual,
    /// Mirror an existing monitor (development, or when the driver is
    /// unavailable). `None` = primary monitor.
    Existing(Option<String>),
}

pub struct WindowsHostPlatform {
    encoders: Mutex<Vec<(CodecCapability, EncoderDescriptor)>>,
    displays: VirtualDisplayManager,
    source: DisplaySource,
}

impl WindowsHostPlatform {
    pub fn new(source: DisplaySource) -> Self {
        let encoders = encoder::probe();
        if encoders.is_empty() {
            tracing::error!("no working hardware video encoder found");
        }
        WindowsHostPlatform { encoders: Mutex::new(encoders), displays: VirtualDisplayManager::default(), source }
    }

    pub fn reprobe(&self) {
        *self.encoders.lock().unwrap_or_else(|e| e.into_inner()) = encoder::probe();
    }

    /// Detaches the virtual monitor if no session uses it (application exit).
    pub fn release_displays(&self) {
        if matches!(self.source, DisplaySource::Virtual) {
            self.displays.release_now();
        }
    }
}

struct ExistingMonitor(DisplayInfo);

impl DisplayLease for ExistingMonitor {
    fn info(&self) -> DisplayInfo {
        self.0.clone()
    }
    fn reconfigure(&mut self, _req: &DisplayRequest) -> Result<DisplayInfo, PlatformError> {
        Ok(self.0.clone())
    }
}

impl HostPlatform for WindowsHostPlatform {
    fn encoders(&self) -> Vec<CodecCapability> {
        self.encoders.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(c, _)| c.clone()).collect()
    }

    fn driver_status(&self) -> DriverStatus {
        if let DisplaySource::Existing(_) = self.source {
            return DriverStatus::NotApplicable;
        }
        match vdd::installation() {
            vdd::Installation::NotInstalled => DriverStatus::NotInstalled,
            vdd::Installation::Stopped => {
                DriverStatus::Unavailable { reason: "The Virtual Display Driver is installed but not running.".into() }
            }
            vdd::Installation::Running => {
                let detail = match display::active_vdd() {
                    Ok(Some(gdi)) => {
                        let mode = display::current_mode(&gdi)
                            .map(|m| format!("{}x{} @ {} Hz", m.width, m.height, m.refresh_hz))
                            .unwrap_or_default();
                        format!("Virtual monitor active ({gdi}, {mode})")
                    }
                    _ => "Ready".into(),
                };
                DriverStatus::Ready { detail }
            }
        }
    }

    fn acquire_display(
        &self,
        request: &DisplayRequest,
        linger: Duration,
    ) -> Result<Box<dyn DisplayLease>, PlatformError> {
        match &self.source {
            DisplaySource::Virtual => Ok(Box::new(self.displays.acquire(request, linger)?)),
            DisplaySource::Existing(device) => {
                let m = match device {
                    Some(d) => capture::monitor_by_device(d),
                    None => capture::monitors().into_iter().find(|m| m.primary),
                }
                .ok_or_else(|| PlatformError::new(PlatformErrorKind::Display, "monitor not found"))?;
                let mode = display::current_mode(&m.device);
                Ok(Box::new(ExistingMonitor(DisplayInfo {
                    id: m.device.clone(),
                    name: m.device.clone(),
                    width: (m.rect.right - m.rect.left) as u32,
                    height: (m.rect.bottom - m.rect.top) as u32,
                    refresh_hz: mode.map(|m| m.refresh_hz).unwrap_or(60),
                    is_virtual: false,
                })))
            }
        }
    }

    fn start_video(
        &self,
        display: &DisplayInfo,
        config: &StreamConfig,
        sink: Box<dyn PipelineSink>,
    ) -> Result<Box<dyn VideoPipeline>, PlatformError> {
        let desc = {
            let enc = self.encoders.lock().unwrap_or_else(|e| e.into_inner());
            encoder::ensure_supported(&enc, config.codec)
                .map_err(|e| PlatformError::new(PlatformErrorKind::Encoder, e.to_string()))?
                .clone()
        };
        let monitor = capture::monitor_by_device(&display.id)
            .ok_or_else(|| PlatformError::new(PlatformErrorKind::Display, format!("{} is not active", display.id)))?;
        Ok(Box::new(WindowsPipeline::start(monitor, config.clone(), desc, sink)?))
    }

    fn inject_input(&self, display: &DisplayInfo, event: &InputEvent) -> Result<(), PlatformError> {
        input::inject(&display.id, event).map_err(|e| PlatformError::new(PlatformErrorKind::Input, e.to_string()))
    }
}
