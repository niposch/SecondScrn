//! Windows host platform for SecondScrn.
//!
//! * [`d3d`], [`capture`], [`convert`], [`encoder`], [`pipeline`]: the GPU
//!   video path (WGC → shader → Media Foundation hardware encoder).
//! * [`vdd`] and [`display`]: Virtual Display Driver control and Windows
//!   display configuration.
//! * [`input`]: injection of authenticated tablet input.
//! * [`dpapi`]: protection of the host identity at rest.
//! * [`platform`]: the `HostPlatform` implementation used by the session engine.
//! * [`api`]: the UniFFI API consumed by the WinUI application.

#[cfg(windows)]
uniffi::setup_scaffolding!();

#[cfg(windows)]
pub mod api;
#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod cli;
#[cfg(windows)]
pub mod convert;
#[cfg(windows)]
pub mod d3d;
#[cfg(windows)]
pub mod display;
#[cfg(windows)]
pub mod dpapi;
#[cfg(windows)]
pub mod encoder;
#[cfg(windows)]
pub mod input;
#[cfg(windows)]
mod logging;
#[cfg(windows)]
pub mod pipeline;
#[cfg(windows)]
pub mod platform;
#[cfg(windows)]
mod serve;
#[cfg(windows)]
pub mod vdd;
#[cfg(windows)]
pub mod virtual_display;

/// Makes desktop coordinates physical pixels on every monitor. Must run
/// before any window or monitor API use.
#[cfg(windows)]
pub fn init_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// `%LOCALAPPDATA%\SecondScrn` — identity, pairings, settings, logs.
/// Velopack installs the app elsewhere, so this survives updates.
#[cfg(windows)]
pub fn default_data_dir() -> std::path::PathBuf {
    std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir).join("SecondScrn")
}

#[cfg(windows)]
pub fn computer_name() -> String {
    std::env::var("COMPUTERNAME").ok().filter(|n| !n.is_empty()).unwrap_or_else(|| "Windows PC".into())
}
