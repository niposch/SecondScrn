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
            "--data-dir" => {
                data_dir = args.get(i + 1).context("--data-dir needs a path")?.into();
                i += 1;
            }
            other => anyhow::bail!("unknown option {other}"),
        }
        i += 1;
    }
    let logs = secondscrn_diagnostics::LogBuffer::new(5000);
    let platform = Arc::new(WindowsHostPlatform::new(source));
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
                        "{:?} {} {} {}x{} | enc {:.0} fps {:.1} ms | {:.1} Mbit/s (est {:.1}) rtt {} loss {} | client {:.0} fps",
                        sess.phase,
                        sess.device_name,
                        sess.stream.as_ref().map(|c| c.codec.to_string()).unwrap_or_default(),
                        sess.stream.as_ref().map(|c| c.width).unwrap_or(0),
                        sess.stream.as_ref().map(|c| c.height).unwrap_or(0),
                        m.pipeline.encode_fps,
                        m.pipeline.encode_ms_avg,
                        m.send_rate_bps as f64 / 1e6,
                        m.bitrate_estimate_bps.unwrap_or(0) as f64 / 1e6,
                        m.rtt_ms.map(|r| format!("{r:.0}ms")).unwrap_or("-".into()),
                        m.loss.map(|l| format!("{:.1}%", l * 100.0)).unwrap_or("-".into()),
                        m.client.map(|c| c.render_fps).unwrap_or(0.0),
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
