//! `serve`: runs the host engine headless (development, CI, emulator tests).

use std::io::BufRead;
use std::sync::Arc;

use anyhow::{Context, Result};
use secondscrn_session::{HostConfig, HostEngine};
use secondscrn_transport::InterfacePolicy;

use crate::platform::{DisplaySource, WindowsHostPlatform};

pub fn run(args: &[String]) -> Result<()> {
    let mut source = DisplaySource::Virtual;
    let mut data_dir = crate::default_data_dir();
    let mut pin_file: Option<std::path::PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--existing" => {
                let dev = args.get(i + 1).filter(|a| !a.starts_with("--")).cloned();
                if dev.is_some() {
                    i += 1;
                }
                source = DisplaySource::Existing(dev);
            }
            // Development aid for unattended tests: read pairing codes from a
            // file instead of the console. Equivalent to the local user typing.
            "--pin-file" => {
                pin_file = Some(args.get(i + 1).context("--pin-file needs a path")?.into());
                i += 1;
            }
            "--data-dir" => {
                data_dir = args.get(i + 1).context("--data-dir needs a path")?.into();
                i += 1;
            }
            other => anyhow::bail!("unknown option {other}"),
        }
        i += 1;
    }
    let logs = secondscrn_diagnostics::LogBuffer::new(5000);
    let prefer = secondscrn_session::settings::load::<secondscrn_session::settings::HostSettings>(
        &data_dir.join("settings.json"),
    )
    .encoder_gpu;
    let platform = Arc::new(WindowsHostPlatform::new(source, prefer));
    let engine = HostEngine::start(
        HostConfig {
            data_dir,
            default_host_name: crate::computer_name(),
            listen_addr: [0, 0, 0, 0].into(),
            interfaces: InterfacePolicy::default(),
            enable_mdns: true,
            protector: Arc::new(crate::dpapi::Dpapi),
            logs: Some(logs),
            app_version: env!("CARGO_PKG_VERSION").into(),
        },
        platform.clone(),
    )?;
    let snap = engine.snapshot();
    println!(
        "host {} ({}) listening on port {} — addresses: {}",
        snap.host_name,
        snap.fingerprint_short,
        snap.port,
        snap.addresses.join(", ")
    );
    println!("encoders: {:?}", snap.encoders.iter().map(|e| e.implementation.clone()).collect::<Vec<_>>());
    println!("driver: {:?}", snap.driver);
    println!("type the 6-digit code shown on the tablet when asked; 'q' quits.");

    let mut rx = engine.subscribe();
    let watcher = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let mut last_line = String::new();
            while rx.changed().await.is_ok() {
                let s = rx.borrow_and_update().clone();
                let mut line = String::new();
                if let Some(p) = &s.pairing {
                    line = format!("PAIRING REQUEST #{} from {} ({}) — enter code:", p.id, p.device_name, p.fingerprint_short);
                } else if let Some(sess) = &s.session {
                    let m = &sess.metrics;
                    line = format!(
                        "{:?} {} {} {}x{} | cap {:.0} enc {:.0} fps {:.1} ms | {:.1} Mbit/s (est {:.1}) rtt {} loss {} | tx {} client {:.0} fps dec {:.0} ms q {} rx {} dec {} stut {} | kf {} nack {} pli {} drop {}+{} late {} cdrop {}",
                        sess.phase,
                        sess.device_name,
                        sess.stream.as_ref().map(|c| c.codec.to_string()).unwrap_or_default(),
                        sess.stream.as_ref().map(|c| c.width).unwrap_or(0),
                        sess.stream.as_ref().map(|c| c.height).unwrap_or(0),
                        m.pipeline.capture_fps,
                        m.pipeline.encode_fps,
                        m.pipeline.encode_ms_avg,
                        m.send_rate_bps as f64 / 1e6,
                        m.bitrate_estimate_bps.unwrap_or(0) as f64 / 1e6,
                        m.rtt_ms.map(|r| format!("{r:.0}ms")).unwrap_or("-".into()),
                        m.loss.map(|l| format!("{:.1}%", l * 100.0)).unwrap_or("-".into()),
                        m.pipeline.frames_encoded,
                        m.client.map(|c| c.render_fps).unwrap_or(0.0),
                        m.client.map(|c| c.decode_time_ms_avg).unwrap_or(0.0),
                        m.client.map(|c| c.decode_queue_max).unwrap_or(0),
                        m.client.map(|c| c.frames_received).unwrap_or(0),
                        m.client.map(|c| c.frames_decoded).unwrap_or(0),
                        m.client.map(|c| c.stutters).unwrap_or(0),
                        m.pipeline.keyframes,
                        m.nacks,
                        m.plis,
                        m.frames_dropped_network,
                        m.pipeline.frames_skipped,
                        m.client.map(|c| c.late_frames).unwrap_or(0),
                        m.client.map(|c| c.frames_dropped).unwrap_or(0),
                    );
                } else if let Some(e) = &s.last_error {
                    line = format!("idle — last error: {e}");
                }
                if !line.is_empty() && line != last_line {
                    println!("{line}");
                    last_line = line;
                }
            }
        });
    });

    if let Some(path) = pin_file {
        println!("reading pairing codes from {}", path.display());
        loop {
            std::thread::sleep(std::time::Duration::from_millis(300));
            if let (Some(p), Ok(code)) = (engine.snapshot().pairing, std::fs::read_to_string(&path)) {
                let _ = std::fs::remove_file(&path);
                match engine.submit_pairing_pin(p.id, code.trim()) {
                    Ok(()) => println!("code submitted"),
                    Err(e) => println!("{e}"),
                }
            }
            // Test controls: `<pin-file>.fps` / `.res` hold a frame rate or a
            // resolution percentage for the connected tablet.
            for (ext, set) in [
                ("fps", &(|v| engine.set_frame_rate(v)) as &dyn Fn(u32) -> Result<(), _>),
                ("res", &|v| engine.set_resolution(v)),
            ] {
                if let Ok(v) = std::fs::read_to_string(path.with_extension(ext)) {
                    let _ = std::fs::remove_file(path.with_extension(ext));
                    match v.trim().parse() {
                        Ok(v) => println!("{ext} {v}: {:?}", set(v)),
                        Err(e) => println!("{ext}: {e}"),
                    }
                }
            }
            if path.with_extension("quit").exists() {
                let _ = std::fs::remove_file(path.with_extension("quit"));
                break;
            }
        }
        engine.shutdown();
        platform.release_displays();
        return Ok(());
    }
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let line = line.trim();
        if line == "q" {
            break;
        }
        if let Some(p) = engine.snapshot().pairing {
            match engine.submit_pairing_pin(p.id, line) {
                Ok(()) => println!("code submitted"),
                Err(e) => println!("{e}"),
            }
        }
    }
    engine.shutdown();
    platform.release_displays();
    drop(watcher);
    Ok(())
}
