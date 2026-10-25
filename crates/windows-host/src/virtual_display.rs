//! Lifecycle of the tablet's virtual monitor.
//!
//! A session acquires a lease; dropping the lease keeps the monitor attached
//! for a linger period so that brief network drops don't make Windows move
//! all windows back to the main screen. A new lease within that period
//! reuses the monitor untouched.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use secondscrn_protocol::DisplayRequest;
use secondscrn_session::platform::{DisplayInfo, DisplayLease, DisplayPlan, PlatformError, PlatformErrorKind, UiScale};

use crate::display::{self, Mode};
use crate::vdd;

#[derive(Default)]
struct State {
    generation: u64,
    leased: bool,
}

#[derive(Clone, Default)]
pub struct VirtualDisplayManager {
    state: Arc<Mutex<State>>,
}

fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn mode_for(req: &DisplayRequest) -> Mode {
    // Encoders and NV12 need even sizes.
    Mode { width: req.width & !1, height: req.height & !1, refresh_hz: req.refresh_hz.clamp(24, 240) }
}

/// Every size of the plan the driver should offer: the requested one first.
fn sizes_for(plan: &DisplayPlan) -> Vec<(u32, u32)> {
    let mode = mode_for(&plan.request);
    let mut sizes = vec![(mode.width, mode.height)];
    sizes.extend(plan.alternatives.iter().map(|&(w, h)| (w & !1, h & !1)).filter(|s| *s != (mode.width, mode.height)));
    sizes
}

impl VirtualDisplayManager {
    pub fn acquire(&self, plan: &DisplayPlan, linger: Duration) -> Result<VddLease, PlatformError> {
        let generation = {
            let mut st = lock(&self.state);
            st.generation += 1;
            st.leased = true;
            st.generation
        };
        let result = self.prepare(plan);
        match result {
            Ok(info) => Ok(VddLease { manager: self.clone(), info, linger, generation }),
            Err(e) => {
                lock(&self.state).leased = false;
                Err(PlatformError::new(PlatformErrorKind::Display, format!("{e:#}")))
            }
        }
    }

    fn prepare(&self, plan: &DisplayPlan) -> Result<DisplayInfo> {
        vdd::require_running()?;
        let mode = mode_for(&plan.request);
        let sizes = sizes_for(plan);
        let protected = display::protected_targets()?;
        let xml = vdd::read_settings()?;
        let needs_count = vdd::configured_count(&xml).unwrap_or(0) == 0;
        let needs_mode = vdd::sync_resolutions(&xml, &sizes, mode.refresh_hz)?.is_some();
        let reloaded = needs_count || needs_mode;
        if reloaded {
            reload_detached(&protected, || {
                if needs_count {
                    vdd::set_display_count(1)?;
                }
                if vdd::ensure_resolutions(&sizes, mode.refresh_hz)? {
                    vdd::reload()?;
                }
                Ok(())
            })?;
        }
        let gdi = attach_with_mode(mode, reloaded, &protected)?;
        let current = display::current_mode(&gdi).context("virtual monitor has no current mode")?;
        display::wait_for(|| crate::capture::monitor_by_device(&gdi), Duration::from_secs(5))
            .context("virtual monitor is not part of the desktop")?;
        tracing::info!(%gdi, width = current.width, height = current.height, hz = current.refresh_hz, "virtual monitor ready");
        Ok(DisplayInfo {
            id: gdi,
            name: "SecondScrn virtual display".into(),
            width: current.width,
            height: current.height,
            refresh_hz: current.refresh_hz,
            is_virtual: true,
        })
    }

    fn release(&self, generation: u64, linger: Duration) {
        {
            let mut st = lock(&self.state);
            if st.generation != generation {
                return; // superseded by a newer lease
            }
            st.leased = false;
        }
        let me = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(linger);
            let st = lock(&me.state);
            if st.generation == generation && !st.leased {
                drop(st);
                match display::detach_vdd() {
                    Ok(()) => tracing::info!("virtual monitor removed from the desktop"),
                    Err(e) => tracing::warn!(error = %e, "could not detach the virtual monitor"),
                }
            }
        });
    }

    /// Detaches immediately (application exit).
    pub fn release_now(&self) {
        let st = lock(&self.state);
        if !st.leased {
            drop(st);
            let _ = display::detach_vdd();
        }
    }
}

/// Runs a driver reconfiguration with the virtual monitor detached, then waits
/// until the user's displays are all active again. Reloading while attached
/// makes Windows re-apply topology mid-change, which once deactivated an
/// unrelated physical monitor during testing.
fn reload_detached(protected: &[String], reconfigure: impl FnOnce() -> Result<()>) -> Result<()> {
    display::detach_vdd()?;
    reconfigure()?;
    std::thread::sleep(Duration::from_millis(500));
    // Wait for the topology to settle: the VDD target is back and every
    // protected display is active again.
    let settled = display::wait_for(
        || {
            let present = display::vdd_present().ok()?;
            let ok = present && display::protected_targets().ok()?.len() >= protected.len();
            ok.then_some(())
        },
        Duration::from_secs(15),
    );
    if settled.is_none() {
        display::repair(protected);
    }
    Ok(())
}

/// Attaches the VDD monitor and switches it to `mode`. After a driver reload
/// the monitor disappears, comes back under a new GDI name, and offers the
/// new mode only after a few seconds, so the monitor is re-resolved on every
/// attempt.
fn attach_with_mode(mode: Mode, reloaded: bool, protected: &[String]) -> Result<String> {
    let started = std::time::Instant::now();
    let patience = if reloaded { Duration::from_secs(20) } else { Duration::from_secs(3) };
    if reloaded {
        std::thread::sleep(Duration::from_millis(500));
    }
    let mut last_gdi = None;
    while started.elapsed() < patience {
        if let Ok(gdi) = display::attach_vdd(protected) {
            if display::available_modes(&gdi).contains(&mode) {
                // Right after a driver reload the first mode change can
                // fail while Windows is still settling; retry.
                match display::set_mode(&gdi, mode) {
                    Ok(()) => return Ok(gdi),
                    Err(e) => tracing::debug!(error = %e, "mode change not accepted yet"),
                }
            }
            last_gdi = Some(gdi);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    display::repair(protected);
    let gdi = last_gdi.context("could not add the virtual monitor to the desktop")?;
    tracing::warn!(?mode, "requested mode not offered by the driver; keeping the current mode");
    Ok(gdi)
}

pub struct VddLease {
    manager: VirtualDisplayManager,
    info: DisplayInfo,
    linger: Duration,
    generation: u64,
}

impl DisplayLease for VddLease {
    fn info(&self) -> DisplayInfo {
        // Refresh in case the user changed the mode in Windows settings.
        let mut info = self.info.clone();
        if let Some(m) = display::current_mode(&info.id) {
            info.width = m.width;
            info.height = m.height;
            info.refresh_hz = m.refresh_hz;
        }
        info
    }

    fn ui_scale(&self) -> Option<UiScale> {
        display::ui_scale(&self.info.id).map(|s| UiScale { current: s.current, options: s.options })
    }

    fn set_ui_scale(&mut self, percent: u32) -> Result<UiScale, PlatformError> {
        display::set_ui_scale(&self.info.id, percent)
            .map(|s| UiScale { current: s.current, options: s.options })
            .map_err(|e| PlatformError::new(PlatformErrorKind::Display, format!("{e:#}")))
    }

    fn reconfigure(&mut self, plan: &DisplayPlan) -> Result<DisplayInfo, PlatformError> {
        let mode = mode_for(&plan.request);
        let sizes = sizes_for(plan);
        let r = (|| -> Result<DisplayInfo> {
            let protected = display::protected_targets()?;
            let xml = vdd::read_settings()?;
            let reloaded = vdd::sync_resolutions(&xml, &sizes, mode.refresh_hz)?.is_some();
            if reloaded {
                reload_detached(&protected, || {
                    vdd::ensure_resolutions(&sizes, mode.refresh_hz)?;
                    vdd::reload()
                })?;
            }
            self.info.id = attach_with_mode(mode, reloaded, &protected)?;
            Ok(self.info())
        })();
        r.inspect(|i| {
            self.info = i.clone();
        })
        .map_err(|e| PlatformError::new(PlatformErrorKind::Display, format!("{e:#}")))
    }
}

impl Drop for VddLease {
    fn drop(&mut self) {
        self.manager.release(self.generation, self.linger);
    }
}
