//! `HostPlatform` implementation for Windows.

use std::sync::Mutex;
use std::time::Duration;

use secondscrn_protocol::{Codec, CodecCapability, InputEvent, StreamConfig};
use secondscrn_session::platform::{
    DisplayInfo, DisplayLease, DisplayPlan, DriverStatus, HostPlatform, PipelineSink, PlatformError, PlatformErrorKind,
    VideoPipeline,
};
use secondscrn_session::settings::EncoderGpu;

use crate::encoder;
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
    encoders: Mutex<encoder::Inventory>,
    /// The GPU whose encoders are preferred (user setting).
    prefer: Mutex<Option<EncoderGpu>>,
    displays: VirtualDisplayManager,
    source: DisplaySource,
    input: Mutex<input::Injector>,
}

fn probe(prefer: Option<EncoderGpu>) -> encoder::Inventory {
    let inv = encoder::probe(prefer);
    if inv.selected.is_empty() {
        tracing::error!("no working hardware video encoder found");
    }
    inv
}

impl WindowsHostPlatform {
    pub fn new(source: DisplaySource, prefer: Option<EncoderGpu>) -> Self {
        let encoders = probe(prefer);
        if matches!(source, DisplaySource::Virtual) {
            // A previous run that crashed may have left the virtual monitor
            // attached; no session exists yet, so remove it.
            if let Ok(Some(gdi)) = display::active_vdd() {
                tracing::info!(%gdi, "removing virtual monitor left over from a previous run");
                let _ = display::detach_vdd();
            }
        }
        WindowsHostPlatform {
            encoders: Mutex::new(encoders),
            prefer: Mutex::new(prefer),
            displays: VirtualDisplayManager::default(),
            source,
            input: Mutex::default(),
        }
    }

    fn injector(&self) -> std::sync::MutexGuard<'_, input::Injector> {
        self.input.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn reprobe(&self) {
        let prefer = *self.prefer.lock().unwrap_or_else(|e| e.into_inner());
        *self.encoders.lock().unwrap_or_else(|e| e.into_inner()) = probe(prefer);
    }

    /// Prefers `gpu`'s encoders (`None`: automatic) from the next stream on
    /// (re-probes).
    pub fn set_encoder_gpu(&self, gpu: Option<EncoderGpu>) {
        *self.prefer.lock().unwrap_or_else(|e| e.into_inner()) = gpu;
        self.reprobe();
    }

    /// Every GPU with hardware encoders, and what works on it in this process.
    pub fn gpus(&self) -> Vec<encoder::GpuEncoders> {
        self.encoders.lock().unwrap_or_else(|e| e.into_inner()).gpus.clone()
    }

    /// The GPU the selected encoders run on, if any.
    pub fn active_gpu(&self) -> Option<(String, EncoderGpu)> {
        let inv = self.encoders.lock().unwrap_or_else(|e| e.into_inner());
        // H.265 is what tablets normally negotiate; any codec otherwise.
        let pick = inv.selected.iter().find(|(c, _)| c.codec == Codec::H265).or(inv.selected.first());
        pick.map(|(_, d)| (d.gpu.clone(), d.kind))
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
    fn reconfigure(&mut self, _plan: &DisplayPlan) -> Result<DisplayInfo, PlatformError> {
        Ok(self.0.clone())
    }
}

impl HostPlatform for WindowsHostPlatform {
    fn encoders(&self) -> Vec<CodecCapability> {
        self.encoders.lock().unwrap_or_else(|e| e.into_inner()).selected.iter().map(|(c, _)| c.clone()).collect()
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

    fn acquire_display(&self, plan: &DisplayPlan, linger: Duration) -> Result<Box<dyn DisplayLease>, PlatformError> {
        match &self.source {
            DisplaySource::Virtual => Ok(Box::new(self.displays.acquire(plan, linger)?)),
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
            encoder::ensure_supported(&enc.selected, config.codec)
                .map_err(|e| PlatformError::new(PlatformErrorKind::Encoder, e.to_string()))?
                .clone()
        };
        let monitor = capture::monitor_by_device(&display.id)
            .ok_or_else(|| PlatformError::new(PlatformErrorKind::Display, format!("{} is not active", display.id)))?;
        Ok(Box::new(WindowsPipeline::start(monitor, config.clone(), desc, sink)?))
    }

    fn inject_input(&self, display: &DisplayInfo, event: &InputEvent) -> Result<(), PlatformError> {
        self.injector()
            .inject(&display.id, event)
            .map_err(|e| PlatformError::new(PlatformErrorKind::Input, format!("{e:#}")))
    }

    fn input_idle(&self, _display: &DisplayInfo) {
        self.injector().idle();
    }

    fn input_ended(&self) {
        self.injector().release_all();
    }
}
