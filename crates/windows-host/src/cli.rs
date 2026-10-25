//! Implementation of the developer CLI (see `src/bin/cli.rs`).

use anyhow::{Context, Result, bail};
use secondscrn_protocol::{Codec, ColorInfo, StreamConfig};

use crate::{capture, encoder, pipeline};

fn parse_codec(s: &str) -> Result<Codec> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "h264" | "avc" => Codec::H264,
        "h265" | "hevc" => Codec::H265,
        "av1" => Codec::Av1,
        _ => bail!("unknown codec {s}"),
    })
}

pub fn main() -> Result<()> {
    crate::init_dpi_awareness();
    tracing_subscriber::fmt().with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into())).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("probe") => {
            for (cap, desc) in encoder::probe() {
                println!(
                    "{:5} {:45} max {}x{} adapter {:?}",
                    cap.codec.to_string(),
                    cap.implementation,
                    cap.max_width,
                    cap.max_height,
                    desc.luid.map(crate::d3d::luid_to_u64)
                );
            }
        }
        Some("adapters") => {
            for (_, a) in crate::d3d::adapters()? {
                println!(
                    "{:016x} {:40} vram {:>6} MB software={}",
                    crate::d3d::luid_to_u64(a.luid),
                    a.name,
                    a.dedicated_video_memory >> 20,
                    a.software
                );
            }
            for c in Codec::ALL {
                for d in encoder::enumerate(c) {
                    println!("{c}: {} on {:016x}", d.name, d.luid.map(crate::d3d::luid_to_u64).unwrap_or(0));
                }
            }
        }
        Some("monitors") => {
            for m in capture::monitors() {
                println!(
                    "{:16} {}x{} at ({}, {}){}",
                    m.device,
                    m.rect.right - m.rect.left,
                    m.rect.bottom - m.rect.top,
                    m.rect.left,
                    m.rect.top,
                    if m.primary { " primary" } else { "" }
                );
            }
        }
        Some("record") => {
            let [_, dev, codec, secs, file] = &args[..] else {
                bail!("usage: record <device|primary> <codec> <seconds> <file>");
            };
            let codec = parse_codec(codec)?;
            let monitor = if dev == "primary" {
                capture::monitors().into_iter().find(|m| m.primary)
            } else {
                capture::monitor_by_device(dev)
            }
            .context("monitor not found")?;
            let caps = encoder::probe();
            let desc = encoder::ensure_supported(&caps, codec)?.clone();
            let (w, h) =
                ((monitor.rect.right - monitor.rect.left) as u32, (monitor.rect.bottom - monitor.rect.top) as u32);
            let config = StreamConfig {
                codec,
                width: w,
                height: h,
                max_fps: 60,
                color: ColorInfo::SDR_BT709_LIMITED,
                initial_bitrate_bps: 20_000_000,
                encoder: desc.name.clone(),
                scale_percent: 100,
            };
            let mut f = std::io::BufWriter::new(std::fs::File::create(file)?);
            let n = pipeline::record(monitor, config, desc, secs.parse()?, &mut f)?;
            println!("wrote {n} frames to {file}");
        }
        Some("vdd") => match args.get(1).map(String::as_str) {
            Some("status") => {
                println!("installation: {:?}", crate::vdd::installation());
                println!("settings: {}", crate::vdd::settings_path().display());
                println!("active: {:?}", crate::display::active_vdd()?);
                if let Some(gdi) = crate::display::active_vdd()? {
                    println!("mode: {:?}", crate::display::current_mode(&gdi));
                    println!("modes: {:?}", crate::display::available_modes(&gdi));
                }
            }
            Some("attach") => {
                let spec = args.get(2).context("usage: vdd attach WIDTHxHEIGHT@HZ")?;
                let (wh, hz) = spec.split_once('@').unwrap_or((spec, "60"));
                let (w, h) = wh.split_once('x').context("WIDTHxHEIGHT")?;
                let req = secondscrn_protocol::DisplayRequest {
                    width: w.parse()?,
                    height: h.parse()?,
                    refresh_hz: hz.parse()?,
                    dpi: 96,
                    orientation: secondscrn_protocol::Orientation::Landscape,
                };
                let mgr = crate::virtual_display::VirtualDisplayManager::default();
                let plan = secondscrn_session::platform::DisplayPlan { request: req, alternatives: Vec::new() };
                let lease = mgr.acquire(&plan, std::time::Duration::ZERO).map_err(|e| anyhow::anyhow!(e))?;
                use secondscrn_session::platform::DisplayLease;
                println!("attached: {:?}", lease.info());
                std::mem::forget(lease); // keep attached after exit
            }
            Some("detach") => crate::display::detach_vdd()?,
            // Changes the mode like the Windows display settings would.
            Some("set-mode") => {
                let spec = args.get(2).context("usage: vdd set-mode WIDTHxHEIGHT@HZ")?;
                let (wh, hz) = spec.split_once('@').unwrap_or((spec, "60"));
                let (w, h) = wh.split_once('x').context("WIDTHxHEIGHT")?;
                let gdi = crate::display::active_vdd()?.context("the virtual monitor is not active")?;
                let mode = crate::display::Mode { width: w.parse()?, height: h.parse()?, refresh_hz: hz.parse()? };
                crate::display::set_mode(&gdi, mode)?;
                println!("{gdi}: {:?}", crate::display::current_mode(&gdi));
            }
            Some("attach-record") => {
                let delay: u64 = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(0);
                let req = secondscrn_protocol::DisplayRequest {
                    width: 1920,
                    height: 1200,
                    refresh_hz: 60,
                    dpi: 96,
                    orientation: secondscrn_protocol::Orientation::Landscape,
                };
                let mgr = crate::virtual_display::VirtualDisplayManager::default();
                let plan = secondscrn_session::platform::DisplayPlan { request: req, alternatives: Vec::new() };
                let lease = mgr.acquire(&plan, std::time::Duration::ZERO).map_err(|e| anyhow::anyhow!(e))?;
                use secondscrn_session::platform::DisplayLease;
                let info = lease.info();
                std::thread::sleep(std::time::Duration::from_millis(delay));
                let monitor = capture::monitor_by_device(&info.id).context("monitor")?;
                let gpu = crate::d3d::create(None)?;
                match capture::MonitorCapture::start(&gpu, &monitor, || {}) {
                    Ok(_) => println!("capture OK after {delay} ms"),
                    Err(e) => println!("capture FAILED after {delay} ms: {e:#}"),
                }
                drop(lease);
            }
            _ => eprintln!("usage: vdd status | attach WxH@HZ | detach"),
        },
        Some("targets") => {
            for (path, name, active) in crate::display::targets()? {
                println!("{} {:24} {}", if active { "active  " } else { "inactive" }, name, path);
            }
        }
        Some("activate") => {
            crate::display::activate_target(args.get(1).context("usage: activate <device-path-substring>")?)?
        }
        Some("serve") => crate::serve::run(&args[1..])?,
        _ => {
            eprintln!("commands: probe | monitors | record <device|primary> <codec> <seconds> <file> | serve");
        }
    }
    Ok(())
}
