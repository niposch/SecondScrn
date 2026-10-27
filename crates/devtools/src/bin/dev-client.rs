//! Headless client: connects to a host, pairs (printing the PIN to type on
//! the PC), and records the stream.
//!
//!   secondscrn-dev-client <host> [--port N] [--out file.h265] [--seconds N]
//!                         [--size WxH] [--codec h264|h265|av1] [--data-dir DIR]

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use secondscrn_devtools::RecordingPlatform;
use secondscrn_protocol::{Codec, DEFAULT_CONTROL_PORT, DisplayRequest, Orientation};
use secondscrn_security::UnprotectedForTests;
use secondscrn_session::snapshot::ClientPhase;
use secondscrn_session::{ClientConfig, ClientEngine, ConnectTarget};
use secondscrn_transport::InterfacePolicy;

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into())).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let host = args.first().context("usage: secondscrn-dev-client <host> [options]")?.clone();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let port: u16 = opt("--port").map(|p| p.parse()).transpose()?.unwrap_or(DEFAULT_CONTROL_PORT);
    let seconds: f64 = opt("--seconds").map(|s| s.parse()).transpose()?.unwrap_or(10.0);
    let (w, h) = match opt("--size") {
        Some(s) => {
            let (w, h) = s.split_once('x').context("--size WxH")?;
            (w.parse()?, h.parse()?)
        }
        None => (1920, 1200),
    };
    let data_dir =
        opt("--data-dir").map(Into::into).unwrap_or_else(|| std::env::temp_dir().join("secondscrn-dev-client"));
    let mut platform = RecordingPlatform::new(opt("--out").map(Into::into));
    if let Some(c) = opt("--codec") {
        platform.codecs = vec![match c.as_str() {
            "h264" => Codec::H264,
            "h265" | "hevc" => Codec::H265,
            "av1" => Codec::Av1,
            _ => bail!("unknown codec {c}"),
        }];
    }
    let stats = platform.stats.clone();

    let engine = ClientEngine::start(
        ClientConfig {
            data_dir,
            interfaces: InterfacePolicy::default(),
            // Development tool only: the identity file is not encrypted.
            protector: Arc::new(UnprotectedForTests),
            logs: None,
            app_version: env!("CARGO_PKG_VERSION").into(),
        },
        Arc::new(platform),
    )?;
    engine.set_display(DisplayRequest {
        width: w,
        height: h,
        refresh_hz: 60,
        dpi: 220,
        orientation: Orientation::Landscape,
    });
    let known = engine.snapshot().known_hosts.into_iter().find(|k| k.last_address == host && k.port == port);
    engine.connect(ConnectTarget { address: host, port, fingerprint: known.map(|k| k.fingerprint) });

    let start = Instant::now();
    let mut streaming_since = None;
    let mut last = String::new();
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let s = engine.snapshot();
        let line = match &s.phase {
            ClientPhase::Pairing { pin } => format!("PAIRING: enter code {pin} on the PC"),
            ClientPhase::Streaming => {
                streaming_since.get_or_insert_with(Instant::now);
                let m = &s.metrics;
                format!(
                    "streaming {} {}x{}: {:.0} fps, {:.1} Mbit/s, rtt {} loss {}, frames {} (key {})",
                    s.stream.as_ref().map(|c| c.codec.to_string()).unwrap_or_default(),
                    s.stream.as_ref().map(|c| c.width).unwrap_or(0),
                    s.stream.as_ref().map(|c| c.height).unwrap_or(0),
                    m.render_fps,
                    m.receive_rate_bps as f64 / 1e6,
                    m.rtt_ms.map(|r| format!("{r:.1}ms")).unwrap_or("-".into()),
                    m.loss.map(|l| format!("{:.2}%", l * 100.0)).unwrap_or("-".into()),
                    stats.frames.load(std::sync::atomic::Ordering::Relaxed),
                    stats.keyframes.load(std::sync::atomic::Ordering::Relaxed),
                )
            }
            ClientPhase::Failed { message, .. } => {
                eprintln!("failed: {message}");
                std::process::exit(2);
            }
            other => format!("{other:?}"),
        };
        if line != last {
            println!("{line}");
            last = line;
        }
        if streaming_since.is_some_and(|t| t.elapsed().as_secs_f64() >= seconds) {
            break;
        }
        if streaming_since.is_none() && start.elapsed() > Duration::from_secs(180) {
            bail!("did not start streaming within 3 minutes");
        }
    }
    engine.disconnect();
    std::thread::sleep(Duration::from_millis(300));
    println!(
        "done: {} frames, {} keyframes, {:.1} MB",
        stats.frames.load(std::sync::atomic::Ordering::Relaxed),
        stats.keyframes.load(std::sync::atomic::Ordering::Relaxed),
        stats.bytes.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1e6
    );
    Ok(())
}
