//! Which GPU encodes, on PCs with an integrated and a dedicated one.
//!
//! On hybrid laptops (NVIDIA Optimus and similar) the dedicated GPU drives no
//! screen, and its Media Foundation encoder only activates in a process that
//! Windows runs on that GPU (`ActivateObject` fails with `E_UNEXPECTED`
//! otherwise). Windows decides that when the process starts, from the
//! per-app graphics preference (Settings → System → Display → Graphics),
//! stored per executable in `HKCU\Software\Microsoft\DirectX\UserGpuPreferences`.
//! Choosing the dedicated GPU therefore sets that preference and needs a
//! restart; the integrated GPU's encoder works either way.

use std::path::Path;

use anyhow::{Context, Result};
use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::core::{HSTRING, w};

const KEY: windows::core::PCWSTR = w!(r"Software\Microsoft\DirectX\UserGpuPreferences");
/// `GpuPreference=2`: high performance (the dedicated GPU).
pub const HIGH_PERFORMANCE: u32 = 2;

fn read(exe: &Path) -> Option<String> {
    let name = HSTRING::from(exe.as_os_str());
    let mut buf = [0u16; 512];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, KEY, &name, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as _), Some(&mut len))
    };
    r.is_ok().then(|| String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]))
}

/// The `GpuPreference` part of a preference string such as
/// `"GpuPreference=2;SwapEffectUpgradeEnable=1;"`.
fn parse(value: &str) -> Option<u32> {
    value.split(';').find_map(|kv| kv.trim().strip_prefix("GpuPreference=")).and_then(|v| v.trim().parse().ok())
}

/// `value` with `GpuPreference` set to `pref` (or removed), keeping other
/// options Windows stores in the same string.
fn with_preference(value: &str, pref: Option<u32>) -> String {
    let mut parts: Vec<String> = value
        .split(';')
        .map(str::trim)
        .filter(|kv| !kv.is_empty() && !kv.starts_with("GpuPreference="))
        .map(str::to_string)
        .collect();
    if let Some(p) = pref {
        parts.insert(0, format!("GpuPreference={p}"));
    }
    parts.iter().map(|p| format!("{p};")).collect()
}

/// The graphics preference Windows applies to `exe`, if one is set.
pub fn preference(exe: &Path) -> Option<u32> {
    read(exe).as_deref().and_then(parse)
}

/// Sets (or clears) the graphics preference for `exe`. Takes effect the next
/// time it starts.
pub fn set_preference(exe: &Path, pref: Option<u32>) -> Result<()> {
    let old = read(exe).unwrap_or_default();
    if parse(&old) == pref {
        return Ok(());
    }
    let name = HSTRING::from(exe.as_os_str());
    let new = with_preference(&old, pref);
    let r = if new.is_empty() {
        let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, KEY, &name) };
        if r == ERROR_FILE_NOT_FOUND { Default::default() } else { r }
    } else {
        let data: Vec<u16> = new.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            RegSetKeyValueW(HKEY_CURRENT_USER, KEY, &name, REG_SZ.0, Some(data.as_ptr() as _), (data.len() * 2) as u32)
        }
    };
    r.ok().with_context(|| format!("cannot set the graphics preference for {}", exe.display()))?;
    tracing::info!(exe = %exe.display(), ?pref, "graphics preference set");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preference_string_keeps_other_options() {
        assert_eq!(parse("GpuPreference=2;"), Some(2));
        assert_eq!(parse("SwapEffectUpgradeEnable=1;GpuPreference=1;"), Some(1));
        assert_eq!(parse("SwapEffectUpgradeEnable=1;"), None);
        assert_eq!(with_preference("", Some(2)), "GpuPreference=2;");
        assert_eq!(
            with_preference("SwapEffectUpgradeEnable=1;GpuPreference=1;", Some(2)),
            "GpuPreference=2;SwapEffectUpgradeEnable=1;"
        );
        assert_eq!(with_preference("GpuPreference=2;SwapEffectUpgradeEnable=1;", None), "SwapEffectUpgradeEnable=1;");
        assert_eq!(with_preference("GpuPreference=2;", None), "");
    }
}
