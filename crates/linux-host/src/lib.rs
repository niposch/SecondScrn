//! Linux host platform for GNOME and KDE Plasma on Wayland.
//!
//! * **Virtual monitor:** the desktop portal's ScreenCast "virtual" source
//!   ([`portal`]), in a RemoteDesktop session so the same session also
//!   carries input. The compositor creates a monitor of the size the stream
//!   negotiates; the user arranges it in the system display settings. No
//!   driver, no root.
//! * **Capture and encoding:** PipeWire through GStreamer ([`pipeline`]):
//!   VA-API (Intel, AMD), NVENC (NVIDIA) or, as a last resort, software
//!   H.264 ([`encoders`]).
//! * **Input:** mouse, scroll and touch through the portal; the pen as a
//!   virtual tablet (uinput) with pressure and tilt, mapped to the virtual
//!   monitor on KDE Plasma ([`pen`]), pointer emulation elsewhere.

#[cfg(target_os = "linux")]
pub mod cli;
#[cfg(target_os = "linux")]
pub mod encoders;
#[cfg(target_os = "linux")]
pub mod input;
#[cfg(target_os = "linux")]
pub mod kscreen;
#[cfg(target_os = "linux")]
pub mod pen;
#[cfg(target_os = "linux")]
pub mod pipeline;
#[cfg(target_os = "linux")]
pub mod platform;
#[cfg(target_os = "linux")]
pub mod portal;
pub mod secrets;
#[cfg(target_os = "linux")]
pub mod serve;

use std::path::PathBuf;

/// `$XDG_DATA_HOME/SecondScrn` (identity, pairings, settings, logs).
pub fn default_data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("SecondScrn")
}

/// The name tablets see by default: the machine's host name.
pub fn host_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "Linux PC".into())
}
