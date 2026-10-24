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
pub mod capture;
#[cfg(windows)]
pub mod cli;
#[cfg(windows)]
pub mod convert;
#[cfg(windows)]
pub mod d3d;
#[cfg(windows)]
pub mod encoder;
#[cfg(windows)]
pub mod pipeline;
#[cfg(windows)]
mod serve;

/// Makes desktop coordinates physical pixels on every monitor. Must run
/// before any window or monitor API use.
#[cfg(windows)]
pub fn init_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}
