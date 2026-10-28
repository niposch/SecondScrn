//! SecondScrn for Linux: the desktop app around the host engine.
//!
//! The same options as the Windows app (Home, Tablets, Settings,
//! Diagnostics, quick controls in the tray), in GNOME / KDE style.

#[cfg(target_os = "linux")]
mod app;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    app::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("This is the Linux app; on Windows use apps/windows.");
}
