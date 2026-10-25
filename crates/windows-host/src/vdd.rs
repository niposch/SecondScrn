//! Control of the VirtualDrivers Virtual Display Driver (MttVDD).
//!
//! VDD is an external, separately installed, signed IddCx driver. It is
//! controlled through:
//! * `vdd_settings.xml` (monitor count, resolution list), read by the driver
//!   when it (re)initialises its adapter;
//! * the named pipe `\\.\pipe\MTTVirtualDisplayPipe`, which accepts UTF-16
//!   commands such as `SETDISPLAYCOUNT n` and `RELOAD_DRIVER` from any local
//!   user (no elevation needed).
//!
//! The driver keeps at least one monitor even with a count of 0, so whether
//! the monitor is part of the desktop is controlled separately through the
//! Windows display configuration (see `display`).

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub const PIPE: &str = r"\\.\pipe\MTTVirtualDisplayPipe";
const DEFAULT_DIR: &str = r"C:\VirtualDisplayDriver";
/// Substring of the monitor device path of VDD monitors (EDID vendor "MTT").
pub const MONITOR_ID: &str = "MTT1337";

/// Folder holding `vdd_settings.xml` (registry override, else the default).
pub fn settings_dir() -> PathBuf {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::w;
    let mut buf = [0u16; 512];
    let mut len = (buf.len() * 2) as u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\MikeTheTech\VirtualDisplayDriver"),
            w!("VDDPATH"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as _),
            Some(&mut len),
        )
    };
    if ok.is_ok() {
        let s = String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]);
        if !s.trim().is_empty() {
            return PathBuf::from(s.trim());
        }
    }
    PathBuf::from(DEFAULT_DIR)
}

pub fn settings_path() -> PathBuf {
    settings_dir().join("vdd_settings.xml")
}

/// True if the driver's control pipe exists. Uses `WaitNamedPipeW`, which
/// does not connect: the driver serves a single pipe instance, and a probe
/// connection without a command would keep it busy for a while.
pub fn pipe_available() -> bool {
    use windows::Win32::System::Pipes::WaitNamedPipeW;
    use windows::core::HSTRING;
    unsafe { WaitNamedPipeW(&HSTRING::from(PIPE), 2000) }.as_bool()
}

/// Sends one command. The driver reads UTF-16LE messages.
pub fn send(command: &str) -> Result<()> {
    let mut pipe = None;
    // The server handles one client at a time; retry while it is busy.
    for _ in 0..100 {
        match std::fs::OpenOptions::new().read(true).write(true).open(PIPE) {
            Ok(p) => {
                pipe = Some(p);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let mut pipe = pipe.context("the Virtual Display Driver is not running")?;
    let bytes: Vec<u8> = command.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    pipe.write_all(&bytes)?;
    pipe.flush()?;
    tracing::debug!(command, "sent VDD command");
    Ok(())
}

pub fn set_display_count(n: u32) -> Result<()> {
    send(&format!("SETDISPLAYCOUNT {n}"))
}

pub fn reload() -> Result<()> {
    send("RELOAD_DRIVER")
}

/// Monitor count configured in the XML.
pub fn configured_count(xml: &str) -> Option<u32> {
    let start = xml.find("<count>")? + "<count>".len();
    let end = xml[start..].find("</count>")? + start;
    xml[start..end].trim().parse().ok()
}

fn has_resolution(xml: &str, width: u32, height: u32) -> bool {
    let compact: String = xml.split_whitespace().collect();
    compact.contains(&format!("<width>{width}</width><height>{height}</height>"))
}

fn has_global_refresh(xml: &str, hz: u32) -> bool {
    let compact: String = xml.split_whitespace().collect();
    compact.contains(&format!("<g_refresh_rate>{hz}</g_refresh_rate>"))
}

/// Marks the entries SecondScrn added, so it can remove them again.
const MARK: &str = "<!-- added by SecondScrn -->";
/// At most this many entries of our own. Every entry is multiplied by every
/// global refresh rate, and VDD 2.35 crashed (access violation, device taken
/// offline) with ten more entries on top of a dozen; four plus the driver's
/// defaults has been safe.
pub const MAX_OWN_ENTRIES: usize = 4;

fn tag_value(s: &str, tag: &str) -> Option<u32> {
    let open = format!("<{tag}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&format!("</{tag}>"))? + start;
    s[start..end].trim().parse().ok()
}

/// Our entries as (byte range including the marker, width, height).
fn own_entries(xml: &str) -> Vec<(std::ops::Range<usize>, u32, u32)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = xml[from..].find(MARK).map(|i| i + from) {
        let Some(end) = xml[i..].find("</resolution>").map(|e| i + e + "</resolution>".len()) else { break };
        let body = &xml[i..end];
        if let (Some(w), Some(h)) = (tag_value(body, "width"), tag_value(body, "height")) {
            // Swallow the line break and indentation before the marker too.
            let start = xml[..i].trim_end().len();
            out.push((start..end, w, h));
        }
        from = end;
    }
    out
}

/// Returns the XML offering `wanted` at `hz`, or `None` if nothing needed to
/// change. When something must be added (which costs a driver reload),
/// entries SecondScrn added earlier that are no longer wanted are removed in
/// the same go, keeping the driver's mode count small; the user's own
/// entries are preserved verbatim.
pub fn sync_resolutions(xml: &str, wanted: &[(u32, u32)], hz: u32) -> Result<Option<String>> {
    let wanted = &wanted[..wanted.len().min(MAX_OWN_ENTRIES)];
    let mut out = xml.to_string();
    let mut changed = false;
    let missing = wanted.iter().any(|&(w, h)| !has_resolution(xml, w, h));
    for (range, w, h) in own_entries(xml).into_iter().rev().filter(|_| missing) {
        if !wanted.contains(&(w, h)) {
            out.replace_range(range, "");
            changed = true;
        }
    }
    for &(width, height) in wanted {
        if has_resolution(&out, width, height) {
            continue;
        }
        let idx = out.rfind("</resolutions>").context("vdd_settings.xml has no <resolutions> section")?;
        let idx = out[..idx].trim_end().len();
        let entry = format!(
            "
    {MARK}
    <resolution>
      <width>{width}</width>
      <height>{height}</height>
      <refresh_rate>{hz}</refresh_rate>
    </resolution>"
        );
        out.insert_str(idx, &entry);
        changed = true;
    }
    if !has_global_refresh(&out, hz)
        && let Some(idx) = out.find("</global>")
    {
        out.insert_str(
            idx,
            &format!(
                "    <g_refresh_rate>{hz}</g_refresh_rate>
	"
            ),
        );
        changed = true;
    }
    Ok(changed.then_some(out))
}

/// Makes the driver offer `wanted` at `hz` (see [`sync_resolutions`]).
/// Returns true if the settings changed (the driver must then be reloaded).
pub fn ensure_resolutions(wanted: &[(u32, u32)], hz: u32) -> Result<bool> {
    let path = settings_path();
    let xml = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    match sync_resolutions(&xml, wanted, hz)? {
        None => Ok(false),
        Some(new) => {
            std::fs::write(&path, new).with_context(|| {
                format!("cannot update {} (run the SecondScrn driver setup to fix permissions)", path.display())
            })?;
            tracing::info!(?wanted, hz, "updated the virtual display driver's resolutions");
            Ok(true)
        }
    }
}

pub fn read_settings() -> Result<String> {
    let path = settings_path();
    std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installation {
    /// Settings file present and the pipe answers.
    Running,
    /// Settings present but the driver isn't responding (disabled/crashed).
    Stopped,
    NotInstalled,
}

pub fn installation() -> Installation {
    if pipe_available() {
        Installation::Running
    } else if settings_path().exists() {
        Installation::Stopped
    } else {
        Installation::NotInstalled
    }
}

pub fn require_running() -> Result<()> {
    match installation() {
        Installation::Running => Ok(()),
        Installation::Stopped => bail!("the Virtual Display Driver is installed but not running"),
        Installation::NotInstalled => bail!("the Virtual Display Driver is not installed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version='1.0' encoding='utf-8'?>
<vdd_settings>
    <monitors>
        <count>1</count>
    </monitors>
	<global>
		<g_refresh_rate>60</g_refresh_rate>
		<g_refresh_rate>90</g_refresh_rate>
	</global>
    <resolutions>
        <resolution>
            <width>1920</width>
            <height>1080</height>
            <refresh_rate>30</refresh_rate>
        </resolution>
    </resolutions>
</vdd_settings>"#;

    #[test]
    fn adds_missing_resolution_once() {
        let new = sync_resolutions(XML, &[(2560, 1600)], 60).unwrap().unwrap();
        assert!(has_resolution(&new, 2560, 1600));
        assert!(has_resolution(&new, 1920, 1080), "existing entries kept");
        assert!(sync_resolutions(&new, &[(2560, 1600)], 60).unwrap().is_none(), "idempotent");
        assert_eq!(configured_count(&new), Some(1));
        assert_eq!(new.matches("<resolution>").count(), 2);
        assert_eq!(new.matches("</resolution>").count(), 2);
    }

    #[test]
    fn adds_missing_global_refresh_rate() {
        assert!(sync_resolutions(XML, &[(1920, 1080)], 60).unwrap().is_none());
        let new = sync_resolutions(XML, &[(1920, 1080)], 120).unwrap().unwrap();
        assert!(has_global_refresh(&new, 120));
        assert!(has_global_refresh(&new, 60));
    }

    #[test]
    fn replaces_own_entries_and_keeps_the_users() {
        let a = sync_resolutions(XML, &[(1920, 1080), (1600, 1000), (1000, 1600)], 60).unwrap().unwrap();
        assert_eq!(a.matches("<resolution>").count(), 3, "the user's 1920x1080 is not duplicated");
        // Switching to another size drops our previous entries.
        let b = sync_resolutions(&a, &[(800, 500), (500, 800)], 60).unwrap().unwrap();
        assert!(!has_resolution(&b, 1600, 1000) && !has_resolution(&b, 1000, 1600));
        assert!(has_resolution(&b, 800, 500) && has_resolution(&b, 500, 800));
        assert!(has_resolution(&b, 1920, 1080), "the user's entry stays");
        assert_eq!(b.matches(MARK).count(), 2);
        assert_eq!(b.matches("<resolution>").count(), b.matches("</resolution>").count());
        // Sizes already offered need no change (and no reload)...
        assert!(sync_resolutions(&b, &[(1920, 1080)], 60).unwrap().is_none());
        // ...the next addition prunes the stale ones.
        let c = sync_resolutions(&b, &[(1920, 1080), (1080, 1920)], 60).unwrap().unwrap();
        assert_eq!(c.matches(MARK).count(), 1);
        assert!(!has_resolution(&c, 800, 500));
    }

    #[test]
    fn never_adds_more_than_the_limit() {
        let many: Vec<_> = (0..10).map(|i| (1000 + i * 2, 700)).collect();
        let new = sync_resolutions(XML, &many, 60).unwrap().unwrap();
        assert_eq!(new.matches(MARK).count(), MAX_OWN_ENTRIES);
    }

    #[test]
    fn portrait_is_a_distinct_resolution() {
        let new = sync_resolutions(XML, &[(1080, 1920)], 60).unwrap().unwrap();
        assert!(has_resolution(&new, 1080, 1920));
    }

    #[test]
    fn rejects_unexpected_format() {
        assert!(sync_resolutions("<vdd_settings/>", &[(1, 1)], 60).is_err());
    }
}
