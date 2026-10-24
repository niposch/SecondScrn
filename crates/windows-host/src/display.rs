//! Windows display configuration (CCD API) for the virtual monitor.
//!
//! Only paths whose target is a VDD monitor are ever modified. Every other
//! display path is passed back to `SetDisplayConfig` exactly as queried, so
//! the user's arrangement of physical monitors and other virtual monitors
//! (VR headsets, other tools) is preserved.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
use windows::Win32::Graphics::Gdi::{
    CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT,
    DM_PELSWIDTH, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE, EnumDisplaySettingsW,
};
use windows::core::PCWSTR;

use crate::vdd::MONITOR_ID;

const MODE_IDX_INVALID: u32 = 0xffff_ffff;
/// `DISPLAYCONFIG_PATH_ACTIVE` from wingdi.h.
const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x1;

pub struct Config {
    pub paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    pub modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

pub fn query(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Result<Config> {
    loop {
        let (mut np, mut nm) = (0u32, 0u32);
        let r = unsafe { GetDisplayConfigBufferSizes(flags, &mut np, &mut nm) };
        if r != ERROR_SUCCESS {
            bail!("GetDisplayConfigBufferSizes failed: {r:?}");
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        let r = unsafe { QueryDisplayConfig(flags, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None) };
        if r == ERROR_INSUFFICIENT_BUFFER {
            continue; // topology changed between the two calls
        }
        if r != ERROR_SUCCESS {
            bail!("QueryDisplayConfig failed: {r:?}");
        }
        paths.truncate(np as usize);
        modes.truncate(nm as usize);
        return Ok(Config { paths, modes });
    }
}

#[derive(Debug, Clone)]
pub struct TargetName {
    pub friendly: String,
    pub device_path: String,
}

pub fn target_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<TargetName> {
    let mut req = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
    req.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
    req.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
    req.header.adapterId = path.targetInfo.adapterId;
    req.header.id = path.targetInfo.id;
    if unsafe { DisplayConfigGetDeviceInfo(&mut req.header) } != 0 {
        return None;
    }
    let s = |b: &[u16]| String::from_utf16_lossy(b).trim_end_matches('\0').to_string();
    Some(TargetName { friendly: s(&req.monitorFriendlyDeviceName), device_path: s(&req.monitorDevicePath) })
}

/// GDI name (`\\.\DISPLAYn`) of a path's source.
pub fn source_gdi_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
    let mut req = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
    req.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
    req.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
    req.header.adapterId = path.sourceInfo.adapterId;
    req.header.id = path.sourceInfo.id;
    if unsafe { DisplayConfigGetDeviceInfo(&mut req.header) } != 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&req.viewGdiDeviceName).trim_end_matches('\0').to_string())
}

fn is_vdd(path: &DISPLAYCONFIG_PATH_INFO) -> bool {
    target_name(path).is_some_and(|t| t.device_path.to_ascii_uppercase().contains(MONITOR_ID))
}

/// GDI device of the active VDD monitor, if any.
pub fn active_vdd() -> Result<Option<String>> {
    let cfg = query(QDC_ONLY_ACTIVE_PATHS)?;
    Ok(cfg.paths.iter().find(|p| is_vdd(p)).and_then(source_gdi_name))
}

/// True if a VDD target exists (attached or not).
pub fn vdd_present() -> Result<bool> {
    Ok(query(QDC_ALL_PATHS)?.paths.iter().any(is_vdd))
}

fn apply(paths: &[DISPLAYCONFIG_PATH_INFO], modes: &[DISPLAYCONFIG_MODE_INFO]) -> Result<()> {
    let flags = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE;
    let r = unsafe { SetDisplayConfig(Some(paths), Some(modes), flags) };
    if r != 0 {
        bail!("SetDisplayConfig failed with error {r}");
    }
    Ok(())
}

/// Activates the (inactive, connected) display whose target device path
/// contains `id`, leaving every other path unchanged. Used to repair a
/// topology and by `attach_vdd`.
pub fn activate_target(id: &str) -> Result<()> {
    let id = id.to_ascii_uppercase();
    let matches =
        |p: &DISPLAYCONFIG_PATH_INFO| target_name(p).is_some_and(|t| t.device_path.to_ascii_uppercase().contains(&id));
    let active = query(QDC_ONLY_ACTIVE_PATHS)?;
    if active.paths.iter().any(matches) {
        return Ok(());
    }
    let all = query(QDC_ALL_PATHS)?;
    let used: Vec<(u64, u32)> =
        active.paths.iter().map(|p| (crate::d3d::luid_to_u64(p.sourceInfo.adapterId), p.sourceInfo.id)).collect();
    let mut candidate = all
        .paths
        .iter()
        .filter(|p| matches(p) && p.targetInfo.targetAvailable.as_bool())
        .find(|p| !used.contains(&(crate::d3d::luid_to_u64(p.sourceInfo.adapterId), p.sourceInfo.id)))
        .copied()
        .with_context(|| format!("no available path for {id}"))?;
    candidate.flags |= DISPLAYCONFIG_PATH_ACTIVE;
    candidate.sourceInfo.Anonymous.modeInfoIdx = MODE_IDX_INVALID;
    candidate.targetInfo.Anonymous.modeInfoIdx = MODE_IDX_INVALID;
    let mut paths = active.paths.clone();
    paths.push(candidate);
    apply(&paths, &active.modes)
}

/// Lists all connected targets as (device path, friendly name, active).
pub fn targets() -> Result<Vec<(String, String, bool)>> {
    let active = query(QDC_ONLY_ACTIVE_PATHS)?;
    let active_ids: Vec<String> = active.paths.iter().filter_map(target_name).map(|t| t.device_path).collect();
    let mut out: Vec<(String, String, bool)> = Vec::new();
    for p in query(QDC_ALL_PATHS)?.paths.iter().filter(|p| p.targetInfo.targetAvailable.as_bool()) {
        if let Some(t) = target_name(p)
            && !out.iter().any(|(d, _, _)| *d == t.device_path)
        {
            let is_active = active_ids.contains(&t.device_path);
            out.push((t.device_path, t.friendly, is_active));
        }
    }
    Ok(out)
}

/// Device paths of the active displays that are not ours. These must stay
/// active through everything SecondScrn does.
pub fn protected_targets() -> Result<Vec<String>> {
    Ok(query(QDC_ONLY_ACTIVE_PATHS)?
        .paths
        .iter()
        .filter(|p| !is_vdd(p))
        .filter_map(target_name)
        .map(|t| t.device_path)
        .collect())
}

fn missing(protected: &[String], active: &Config) -> Vec<String> {
    let now: Vec<String> = active.paths.iter().filter_map(target_name).map(|t| t.device_path).collect();
    protected.iter().filter(|p| !now.contains(p)).cloned().collect()
}

/// Re-activates protected displays that are no longer active (for example
/// after Windows re-applied a topology during a driver reload).
pub fn repair(protected: &[String]) {
    let Ok(active) = query(QDC_ONLY_ACTIVE_PATHS) else { return };
    for path in missing(protected, &active) {
        match activate_target(&path) {
            Ok(()) => tracing::warn!(display = %path, "re-activated a display that dropped out during reconfiguration"),
            Err(e) => tracing::error!(display = %path, error = %e, "could not restore a display"),
        }
    }
}

/// Adds the VDD monitor to the desktop (extended, placed by Windows) and
/// returns its GDI device name. Refuses to apply a topology that lacks any
/// of the `protected` displays: `SetDisplayConfig` with a supplied config
/// deactivates everything not supplied, so applying a transient state would
/// turn off the user's monitors.
pub fn attach_vdd(protected: &[String]) -> Result<String> {
    if let Some(gdi) = active_vdd()? {
        return Ok(gdi);
    }
    let active = query(QDC_ONLY_ACTIVE_PATHS)?;
    let absent = missing(protected, &active);
    if !absent.is_empty() {
        bail!("display topology is changing ({} display(s) not active yet)", absent.len());
    }
    let all = query(QDC_ALL_PATHS)?;
    let used_sources: Vec<(u64, u32)> =
        active.paths.iter().map(|p| (crate::d3d::luid_to_u64(p.sourceInfo.adapterId), p.sourceInfo.id)).collect();
    let mut candidate = all
        .paths
        .iter()
        .filter(|p| is_vdd(p) && p.targetInfo.targetAvailable.as_bool())
        .find(|p| !used_sources.contains(&(crate::d3d::luid_to_u64(p.sourceInfo.adapterId), p.sourceInfo.id)))
        .copied()
        .context("no virtual display target is available")?;
    candidate.flags |= DISPLAYCONFIG_PATH_ACTIVE;
    candidate.sourceInfo.Anonymous.modeInfoIdx = MODE_IDX_INVALID;
    candidate.targetInfo.Anonymous.modeInfoIdx = MODE_IDX_INVALID;
    let mut paths = active.paths.clone();
    paths.push(candidate);
    apply(&paths, &active.modes)?;
    let gdi =
        wait_for(|| active_vdd().ok().flatten(), Duration::from_secs(5)).context("the virtual monitor did not appear");
    repair(protected);
    gdi
}

/// Removes the VDD monitor from the desktop. Never detaches the last
/// active display.
pub fn detach_vdd() -> Result<()> {
    let active = query(QDC_ONLY_ACTIVE_PATHS)?;
    let remaining: Vec<DISPLAYCONFIG_PATH_INFO> = active.paths.iter().filter(|p| !is_vdd(p)).copied().collect();
    if remaining.len() == active.paths.len() {
        return Ok(());
    }
    if remaining.is_empty() {
        bail!("refusing to detach the only active display");
    }
    apply(&remaining, &active.modes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn current_mode(gdi: &str) -> Option<Mode> {
    let name = wide(gdi);
    let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    unsafe { EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }.as_bool().then_some(Mode {
        width: dm.dmPelsWidth,
        height: dm.dmPelsHeight,
        refresh_hz: dm.dmDisplayFrequency,
    })
}

pub fn available_modes(gdi: &str) -> Vec<Mode> {
    let name = wide(gdi);
    let mut out = Vec::new();
    for i in 0.. {
        let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
        if !unsafe { EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_DISPLAY_SETTINGS_MODE(i), &mut dm) }.as_bool() {
            break;
        }
        let m = Mode { width: dm.dmPelsWidth, height: dm.dmPelsHeight, refresh_hz: dm.dmDisplayFrequency };
        if !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

/// Switches `gdi` to `mode`. The registry is updated so the mode persists.
pub fn set_mode(gdi: &str, mode: Mode) -> Result<()> {
    if current_mode(gdi) == Some(mode) {
        return Ok(());
    }
    let name = wide(gdi);
    let dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY,
        dmPelsWidth: mode.width,
        dmPelsHeight: mode.height,
        dmDisplayFrequency: mode.refresh_hz,
        ..Default::default()
    };
    let r = unsafe { ChangeDisplaySettingsExW(PCWSTR(name.as_ptr()), Some(&dm), None, CDS_UPDATEREGISTRY, None) };
    if r != DISP_CHANGE_SUCCESSFUL {
        bail!("changing {gdi} to {}x{}@{} failed ({r:?})", mode.width, mode.height, mode.refresh_hz);
    }
    Ok(())
}

pub fn wait_for<T>(mut f: impl FnMut() -> Option<T>, timeout: Duration) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
