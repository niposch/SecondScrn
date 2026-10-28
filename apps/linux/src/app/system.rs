//! Desktop integration: logging, autostart, update notices, display
//! settings and status texts shared by several pages.

use std::path::Path;
use std::rc::Rc;
use std::sync::OnceLock;

use gtk::prelude::*;
use secondscrn_diagnostics::LogBuffer;
use secondscrn_protocol::{Codec, RELEASES_URL, compare_versions};
use secondscrn_session::snapshot::{HostSnapshot, VersionMismatch};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use super::{Ctx, VERSION};

/// File logging (daily files, 7 kept) plus the buffer for exports.
pub fn init_logging(dir: &Path, verbose: bool) -> LogBuffer {
    static LOGS: OnceLock<LogBuffer> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer = LogBuffer::new(5000);
        let level = if verbose { "debug,str0m=info,is=info,zbus=info" } else { "info,str0m=warn,is=warn,zbus=warn" };
        let filter = tracing_subscriber::EnvFilter::try_from_env("SECONDSCRN_LOG").unwrap_or_else(|_| level.into());
        let _ = std::fs::create_dir_all(dir);
        let file = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("secondscrn-host")
            .filename_suffix("log")
            .max_log_files(7)
            .build(dir)
            .ok();
        let registry = tracing_subscriber::registry()
            .with(filter)
            .with(buffer.layer::<tracing_subscriber::Registry>())
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr));
        let _ = match file {
            Some(f) => registry.with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(f)).try_init(),
            None => registry.try_init(),
        };
        buffer
    })
    .clone()
}

// ---- Autostart --------------------------------------------------------------

fn autostart_file() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("autostart").join(format!("{}.desktop", super::APP_ID)))
}

/// Adds or removes the XDG autostart entry.
pub fn set_autostart(on: bool) -> std::io::Result<()> {
    let path = autostart_file().ok_or_else(|| std::io::Error::other("no home directory"))?;
    if !on {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    // Installed packages put `secondscrn` on PATH; otherwise this binary.
    let exe = if Path::new("/usr/bin/secondscrn").exists() {
        "secondscrn".to_string()
    } else {
        std::env::current_exe()?.display().to_string()
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        &path,
        format!(
            "[Desktop Entry]\nType=Application\nName=SecondScrn\nComment=Use your tablet as a second screen\n\
             Exec={exe} --autostart\nIcon={}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            super::APP_ID
        ),
    )
}

// ---- Updates ----------------------------------------------------------------

/// Release page of exactly `version` (before 1.0 the PC and the tablet are
/// updated together to the same version, not necessarily the newest).
pub fn release_page(version: &str) -> String {
    format!("{RELEASES_URL}/tag/v{version}")
}

/// Asks GitHub for the newest release (notify only: packages are updated
/// by the user or the distribution, never by the app).
pub fn check_for_updates(ctx: Rc<Ctx>, manual: bool) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let api = RELEASES_URL.replace("https://github.com/", "https://api.github.com/repos/") + "/latest";
        let out = std::process::Command::new("curl")
            .args(["-fsSL", "--max-time", "15", "-H", "Accept: application/vnd.github+json", &api])
            .output();
        let result = match out {
            Ok(o) if o.status.success() => serde_json::from_slice::<serde_json::Value>(&o.stdout)
                .ok()
                .and_then(|v| v.get("tag_name").and_then(|t| t.as_str()).map(|t| t.trim_start_matches('v').to_string()))
                .ok_or_else(|| "unexpected answer".to_string()),
            Ok(_) => Err("no release found".into()),
            Err(_) => Err("curl is not installed".into()),
        };
        let _ = tx.send(result);
    });
    gtk::glib::spawn_future_local(async move {
        let result = rx.await.unwrap_or_else(|_| Err("stopped".into()));
        match result {
            Ok(latest) if compare_versions(&latest, VERSION) == Some(std::cmp::Ordering::Greater) => {
                *ctx.update.borrow_mut() = Some(latest.clone());
                let n = gtk::gio::Notification::new("Update available");
                n.set_body(Some(&format!(
                    "SecondScrn {latest} is available. Update the tablet app to {latest} at the same time."
                )));
                ctx.app.send_notification(Some("update"), &n);
                if manual {
                    ctx.toast(&format!("SecondScrn {latest} is available"));
                }
                ctx.apply(&ctx.snapshot());
            }
            Ok(_) if manual => ctx.toast("SecondScrn is up to date"),
            Err(e) if manual => ctx.toast(&format!("Couldn't check for updates: {e}")),
            _ => {}
        }
    });
}

/// Banner text and button label for a tablet running another version.
pub fn version_mismatch_text(m: &VersionMismatch) -> (String, String) {
    let tablet_older = compare_versions(&m.tablet_version, &m.host_version) == Some(std::cmp::Ordering::Less);
    if tablet_older {
        (
            format!(
                "{} runs SecondScrn {}, this PC {}. Both must run the same version: install SecondScrn {} on the tablet.",
                m.device_name, m.tablet_version, m.host_version, m.host_version
            ),
            format!("Tablet app {}", m.host_version),
        )
    } else {
        (
            format!(
                "{} runs SecondScrn {}, this PC {}. Both must run the same version: install SecondScrn {} on this PC.",
                m.device_name, m.tablet_version, m.host_version, m.tablet_version
            ),
            format!("Get SecondScrn {}", m.tablet_version),
        )
    }
}

pub fn version_mismatch_url(m: &VersionMismatch) -> String {
    if compare_versions(&m.tablet_version, &m.host_version) == Some(std::cmp::Ordering::Less) {
        secondscrn_protocol::android_apk_url(&m.host_version)
    } else {
        release_page(&m.tablet_version)
    }
}

// ---- Desktop ----------------------------------------------------------------

/// Opens the desktop's display arrangement.
pub fn open_display_settings() {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_lowercase();
    let candidates: &[&[&str]] = if desktop.contains("kde") {
        &[&["systemsettings", "kcm_kscreen"], &["kcmshell6", "kcm_kscreen"]]
    } else {
        &[&["gnome-control-center", "display"], &["systemsettings", "kcm_kscreen"]]
    };
    for c in candidates {
        if std::process::Command::new(c[0]).args(&c[1..]).spawn().is_ok() {
            return;
        }
    }
    tracing::warn!("no display settings application found");
}

/// The desktop remembered the screen sharing permission.
pub fn portal_permission_saved(data_dir: &Path) -> bool {
    std::fs::metadata(data_dir.join("portal-restore-token")).is_ok_and(|m| m.len() > 0)
}

pub fn uinput_accessible() -> bool {
    std::fs::OpenOptions::new().write(true).open("/dev/uinput").is_ok()
}

pub fn encoder_name(s: &HostSnapshot, codec: Codec) -> String {
    s.encoders.iter().find(|e| e.codec == codec).map(|e| e.implementation.clone()).unwrap_or_default()
}

pub fn session_details(s: &HostSnapshot) -> String {
    let Some(x) = &s.session else { return "No active session.".into() };
    let m = &x.metrics;
    let opt = |v: Option<f32>, f: &dyn Fn(f32) -> String| v.map(f).unwrap_or("–".into());
    [
        format!("Tablet: {} ({}) at {}, ID {}", x.device_name, x.device_model, x.remote_address, x.fingerprint_short),
        format!("Phase: {:?}", x.phase),
        match &x.display {
            Some(d) => {
                format!("Display: {} {}×{}{}", d.id, d.width, d.height, if d.is_virtual { " (virtual)" } else { "" })
            }
            None => "Display: –".into(),
        },
        match &x.stream {
            Some(st) => format!("Codec: {} via {}", st.codec, encoder_name(s, st.codec)),
            None => "Codec: –".into(),
        },
        format!(
            "Capture: {:.1} fps · Encode: {:.1} fps, {:.1} ms per frame",
            m.pipeline.capture_fps, m.pipeline.encode_fps, m.pipeline.encode_ms_avg
        ),
        format!(
            "Bitrate: sending {:.2} Mbit/s · target {:.1} Mbit/s · estimate {}",
            m.send_rate_bps as f64 / 1e6,
            m.pipeline.bitrate_target_bps as f64 / 1e6,
            m.bitrate_estimate_bps.map(|b| format!("{:.1} Mbit/s", b as f64 / 1e6)).unwrap_or("–".into())
        ),
        format!(
            "Network: RTT {} · queue {} · loss {} · NACKs {} · PLIs {}",
            opt(m.rtt_ms, &|r| format!("{r:.0} ms")),
            opt(m.queue_delay_ms, &|q| format!("{q:.0} ms")),
            opt(m.loss, &|l| format!("{:.1} %", l * 100.0)),
            m.nacks,
            m.plis
        ),
        format!(
            "Tablet: {} · decode {}",
            opt(m.client.map(|c| c.render_fps), &|f| format!("{f:.0} fps rendered")),
            opt(m.client.map(|c| c.decode_time_ms_avg), &|d| format!("{d:.1} ms"))
        ),
        format!(
            "Frames dropped: {} network, {} skipped · keyframes sent: {}",
            m.frames_dropped_network, m.pipeline.frames_skipped, m.pipeline.keyframes
        ),
    ]
    .join("\n")
}
