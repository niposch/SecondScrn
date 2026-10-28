//! `HostPlatform` implementation for Linux (GNOME, KDE Plasma on Wayland).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use secondscrn_protocol::{Codec, CodecCapability, InputEvent, StreamConfig};
use secondscrn_session::platform::{
    DisplayInfo, DisplayLease, DisplayPlan, DriverStatus, HostPlatform, PipelineSink, PlatformError, PlatformErrorKind,
    VideoPipeline,
};
use secondscrn_session::settings::EncoderGpu;

use crate::encoders::{self, Encoder};
use crate::input::Injector;
use crate::kscreen;
use crate::pipeline::{LinuxPipeline, Source};
use crate::portal::{Portal, PortalStatus, VirtualMonitor};

/// Where the stream comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplaySource {
    /// A virtual monitor through the desktop portal.
    Virtual,
    /// A moving test pattern, no desktop needed (development, tests).
    TestPattern,
}

pub struct LinuxHostPlatform {
    source: DisplaySource,
    portal: Portal,
    encoders: Mutex<Vec<(CodecCapability, Encoder)>>,
    prefer: Mutex<Option<EncoderGpu>>,
    input: Mutex<Injector>,
    /// Shared with leases: the open monitor and the linger bookkeeping.
    state: Arc<Mutex<MonitorState>>,
    /// Desktop capabilities, read once (the portal doesn't change while
    /// we run).
    status: Mutex<Option<PortalStatus>>,
}

#[derive(Default)]
struct MonitorState {
    monitor: Option<VirtualMonitor>,
    /// The compositor's name of the virtual monitor (KDE Plasma).
    output: Option<String>,
    leases: u32,
    /// Bumped on every acquire: a linger timer only closes the monitor if
    /// nothing acquired it since.
    epoch: Arc<AtomicU64>,
}

impl LinuxHostPlatform {
    pub fn new(source: DisplaySource, prefer: Option<EncoderGpu>, data_dir: std::path::PathBuf) -> Self {
        LinuxHostPlatform {
            source,
            portal: Portal::start(data_dir),
            encoders: Mutex::new(encoders::probe(prefer)),
            prefer: Mutex::new(prefer),
            input: Mutex::default(),
            state: Arc::default(),
            status: Mutex::new(None),
        }
    }

    fn injector(&self) -> std::sync::MutexGuard<'_, Injector> {
        self.input.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn reprobe(&self) {
        let prefer = *self.prefer.lock().unwrap_or_else(|e| e.into_inner());
        *self.encoders.lock().unwrap_or_else(|e| e.into_inner()) = encoders::probe(prefer);
    }

    /// Prefers `gpu`'s encoders (`None`: automatic) from the next stream on.
    pub fn set_encoder_gpu(&self, gpu: Option<EncoderGpu>) {
        *self.prefer.lock().unwrap_or_else(|e| e.into_inner()) = gpu;
        self.reprobe();
    }

    /// The selected encoder per codec ("H.265: vah265lpenc (VA-API)").
    pub fn encoder_summary(&self) -> Vec<String> {
        self.encoders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(c, e)| format!("{}: {} ({:?})", c.codec, e.factory, e.backend))
            .collect()
    }

    pub fn portal_status(&self) -> PortalStatus {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        s.get_or_insert_with(|| self.portal.status()).clone()
    }

    /// The pen is a real tablet (pressure, tilt) rather than the pointer.
    pub fn has_pen_tablet(&self) -> bool {
        self.injector().has_pen_tablet()
    }

    /// Asks the desktop for the screen sharing and input permission now (it
    /// is remembered), instead of on the first connection: opens the
    /// virtual monitor briefly.
    pub fn authorize(&self) -> Result<(), String> {
        if self.source == DisplaySource::TestPattern {
            return Ok(());
        }
        self.open_monitor().map_err(|e| e.message)?;
        std::thread::sleep(Duration::from_millis(500));
        self.release_displays();
        Ok(())
    }

    /// Closes the virtual monitor if no session uses it (application exit).
    pub fn release_displays(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.leases == 0 && st.monitor.take().is_some() {
            self.portal.close();
        }
    }

    fn open_monitor(&self) -> Result<(VirtualMonitor, Option<String>), PlatformError> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = &st.monitor {
            return Ok((m.clone(), st.output.clone()));
        }
        let before = kscreen::outputs();
        let monitor = self.portal.open().map_err(|e| PlatformError::new(PlatformErrorKind::Display, e))?;
        // KDE Plasma: the new entry in the monitor list is ours (named
        // for the pen mapping). It appears once the stream runs, so this
        // is looked up again when the pipeline starts.
        st.output = before.and_then(|b| new_output(&b));
        st.monitor = Some(monitor.clone());
        Ok((monitor, st.output.clone()))
    }
}

/// The monitor present now but not in `before`.
fn new_output(before: &[kscreen::Output]) -> Option<String> {
    kscreen::outputs()?.into_iter().find(|o| !before.iter().any(|b| b.name == o.name)).map(|o| o.name)
}

struct Lease {
    info: DisplayInfo,
    /// The monitor's size when the desktop decides it (KDE Plasma: 1920×1080
    /// whatever the stream asks for); the stream then uses it unscaled.
    fixed: Option<(u32, u32)>,
    state: Arc<Mutex<MonitorState>>,
    portal: Portal,
    linger: Duration,
}

impl DisplayLease for Lease {
    fn info(&self) -> DisplayInfo {
        self.info.clone()
    }

    /// The monitor follows the stream's size: the next pipeline negotiates
    /// the new one.
    fn reconfigure(&mut self, plan: &DisplayPlan) -> Result<DisplayInfo, PlatformError> {
        (self.info.width, self.info.height) = self.fixed.unwrap_or((plan.request.width, plan.request.height));
        self.info.refresh_hz = plan.request.refresh_hz;
        Ok(self.info.clone())
    }
}

/// KDE Plasma's portal makes virtual monitors of a fixed size instead of
/// following the stream (GNOME follows it).
fn fixed_size(monitor: &VirtualMonitor) -> Option<(u32, u32)> {
    let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.to_ascii_uppercase().contains("KDE"));
    monitor.size.filter(|(w, h)| kde && *w > 0 && *h > 0).map(|(w, h)| (w as u32 & !1, h as u32 & !1))
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.leases = st.leases.saturating_sub(1);
        if st.leases > 0 || st.monitor.is_none() {
            return;
        }
        // Keep the monitor (windows stay where they are) for the linger
        // time in case the tablet reconnects.
        let epoch = st.epoch.clone();
        let seen = epoch.load(Ordering::SeqCst);
        let (state, portal, linger) = (self.state.clone(), self.portal.clone(), self.linger);
        let close = move || {
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            if st.leases == 0 && epoch.load(Ordering::SeqCst) == seen && st.monitor.take().is_some() {
                st.output = None;
                portal.close();
                tracing::info!("virtual monitor removed");
            }
        };
        drop(st);
        if linger.is_zero() {
            close();
        } else {
            let _ = std::thread::Builder::new().name("secondscrn-linger".into()).spawn(move || {
                std::thread::sleep(linger);
                close();
            });
        }
    }
}

struct TestLease(DisplayInfo);

impl DisplayLease for TestLease {
    fn info(&self) -> DisplayInfo {
        self.0.clone()
    }
    fn reconfigure(&mut self, plan: &DisplayPlan) -> Result<DisplayInfo, PlatformError> {
        self.0.width = plan.request.width;
        self.0.height = plan.request.height;
        Ok(self.0.clone())
    }
}

impl HostPlatform for LinuxHostPlatform {
    fn encoders(&self) -> Vec<CodecCapability> {
        self.encoders.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(c, _)| c.clone()).collect()
    }

    fn driver_status(&self) -> DriverStatus {
        if self.source == DisplaySource::TestPattern {
            return DriverStatus::NotApplicable;
        }
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.monitor.is_some() {
            return DriverStatus::Ready { detail: "Virtual monitor active".into() };
        }
        drop(st);
        let status = self.portal_status();
        if status.virtual_monitors {
            DriverStatus::Ready { detail: status.detail }
        } else {
            DriverStatus::Unavailable { reason: status.detail }
        }
    }

    fn acquire_display(&self, plan: &DisplayPlan, linger: Duration) -> Result<Box<dyn DisplayLease>, PlatformError> {
        let info = |id: String| DisplayInfo {
            id,
            name: "SecondScrn".into(),
            width: plan.request.width,
            height: plan.request.height,
            refresh_hz: plan.request.refresh_hz,
            is_virtual: true,
        };
        if self.source == DisplaySource::TestPattern {
            return Ok(Box::new(TestLease(info("test-pattern".into()))));
        }
        let (monitor, _) = self.open_monitor()?;
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.leases += 1;
        st.epoch.fetch_add(1, Ordering::SeqCst);
        let fixed = fixed_size(&monitor);
        let mut info = info(format!("portal-{}", monitor.generation));
        if let Some((w, h)) = fixed {
            tracing::info!(width = w, height = h, "the desktop sets the virtual monitor's size");
            (info.width, info.height) = (w, h);
        }
        Ok(Box::new(Lease {
            info,
            fixed,
            state: self.state.clone(),
            portal: self.portal.clone(),
            linger,
        }))
    }

    fn start_video(
        &self,
        display: &DisplayInfo,
        config: &StreamConfig,
        sink: Box<dyn PipelineSink>,
    ) -> Result<Box<dyn VideoPipeline>, PlatformError> {
        let encoder = {
            let list = self.encoders.lock().unwrap_or_else(|e| e.into_inner());
            list.iter().find(|(c, _)| c.codec == config.codec).map(|(_, e)| e.clone()).ok_or_else(|| {
                PlatformError::new(PlatformErrorKind::Encoder, format!("no working {} encoder", config.codec))
            })?
        };
        let source = match self.source {
            DisplaySource::TestPattern => Source::TestPattern,
            DisplaySource::Virtual => {
                let (monitor, output) = self.open_monitor()?;
                let source = Source::Portal { fd: monitor.fd.clone(), node: monitor.node_id };
                let pipeline = LinuxPipeline::start(source, config, encoder, sink)?;
                // The monitor exists once the stream negotiated its size;
                // input goes to it in the stream's coordinates.
                let output = output.or_else(|| self.find_output(config));
                let size = monitor
                    .size
                    .filter(|(w, h)| *w > 0 && *h > 0)
                    .map(|(w, h)| (w as f64, h as f64))
                    .unwrap_or((config.width as f64, config.height as f64));
                let touch = self.portal_status().touch;
                self.injector().attach(self.portal.clone(), size, touch, output.as_deref());
                let id = &display.id;
                tracing::info!(display = %id, ?size, ?output, touch, "streaming the virtual monitor");
                return Ok(Box::new(pipeline));
            }
        };
        Ok(Box::new(LinuxPipeline::start(source, config, encoder, sink)?))
    }

    fn inject_input(&self, _display: &DisplayInfo, event: &InputEvent) -> Result<(), PlatformError> {
        self.injector().inject(event).map_err(|e| PlatformError::new(PlatformErrorKind::Input, format!("{e:#}")))
    }

    fn input_ended(&self) {
        self.injector().release_all();
    }
}

impl LinuxHostPlatform {
    /// KDE Plasma: the virtual monitor's name, recognised by its size once
    /// it appeared (a second or so after the stream started).
    fn find_output(&self, config: &StreamConfig) -> Option<String> {
        for _ in 0..15 {
            if let Some(outputs) = kscreen::outputs() {
                let found = outputs.iter().find(|o| {
                    let virt = o.name.to_ascii_lowercase().contains("virtual");
                    let size = (o.width.round() as u32, o.height.round() as u32);
                    virt && (size == (config.width, config.height) || size == (config.height, config.width))
                });
                let found = found.or_else(|| outputs.iter().find(|o| o.name.to_ascii_lowercase().contains("virtual")));
                if let Some(o) = found {
                    let name = o.name.clone();
                    self.state.lock().unwrap_or_else(|e| e.into_inner()).output = Some(name.clone());
                    return Some(name);
                }
            } else {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    }
}

/// The codec tablets normally negotiate, for status lines.
pub fn preferred(list: &[CodecCapability]) -> Option<&CodecCapability> {
    list.iter().find(|c| c.codec == Codec::H265).or(list.first())
}
