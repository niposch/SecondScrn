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

/// `SECONDSCRN_ENCODER_GPU=integrated|dedicated` prefers that GPU's
/// encoders; the GPU Windows runs the CLI on otherwise.
fn preferred_gpu() -> Option<secondscrn_session::settings::EncoderGpu> {
    use secondscrn_session::settings::EncoderGpu;
    match std::env::var("SECONDSCRN_ENCODER_GPU").as_deref() {
        Ok("integrated") => Some(EncoderGpu::Integrated),
        Ok("dedicated") => Some(EncoderGpu::Dedicated),
        _ => None,
    }
}

pub fn main() -> Result<()> {
    crate::init_dpi_awareness();
    tracing_subscriber::fmt().with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into())).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("probe") => {
            let inv = encoder::probe(preferred_gpu());
            for g in &inv.gpus {
                let codecs: Vec<String> = g.codecs.iter().map(|c| c.to_string()).collect();
                println!("{:?} GPU {}: {}", g.kind, g.name, codecs.join(", "));
                if let Some(p) = &g.problem {
                    println!("    {p}");
                }
            }
            for (cap, desc) in &inv.selected {
                println!(
                    "{:5} {:45} max {}x{} on {}",
                    cap.codec.to_string(),
                    cap.implementation,
                    cap.max_width,
                    cap.max_height,
                    desc.gpu
                );
            }
        }
        Some("bench") => {
            let usage = "usage: bench <width>x<height> <fps> [seconds] [--gpu integrated|dedicated] [--json]";
            let (size, fps) = (args.get(1).context(usage)?, args.get(2).context(usage)?);
            let (w, h) = size.split_once('x').context(usage)?;
            let seconds = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2.0);
            let kind = match args.iter().position(|a| a == "--gpu").and_then(|i| args.get(i + 1)).map(String::as_str) {
                Some("integrated") => Some(secondscrn_session::settings::EncoderGpu::Integrated),
                Some("dedicated") => Some(secondscrn_session::settings::EncoderGpu::Dedicated),
                Some(_) => bail!(usage),
                None => None,
            };
            let report = crate::bench::run(w.parse()?, h.parse()?, fps.parse()?, seconds, kind);
            if report.results.is_empty() {
                eprintln!("no GPU with a hardware encoder found");
            }
            if args.iter().any(|a| a == "--json") {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                for r in &report.results {
                    match &r.error {
                        Some(e) => println!("{:?} {}: {e}", r.kind, r.gpu),
                        None => println!(
                            "{:?} {} ({}): {:.0} of {} fps, {:.1} ms (p95 {:.1} ms), up to {:.0} fps",
                            r.kind,
                            r.gpu,
                            r.encoder,
                            r.paced_fps,
                            report.fps,
                            r.latency_ms,
                            r.latency_p95_ms,
                            r.max_fps
                        ),
                    }
                }
            }
        }
        Some("ltr-check") => {
            // Recovery with long-term references, verified with ffmpeg.
            let dir = std::env::temp_dir().join("secondscrn-ltr-check");
            std::fs::create_dir_all(&dir)?;
            let inv = encoder::probe(preferred_gpu());
            for (_, desc) in &inv.selected {
                let (full, lossy, ltr) = crate::bench::reference_check(desc, 1920, 1080)?;
                let ext = match desc.codec {
                    Codec::H264 => "h264",
                    Codec::H265 => "hevc",
                    Codec::Av1 => "obu",
                };
                let (fp, lp) = (dir.join(format!("full.{ext}")), dir.join(format!("lossy.{ext}")));
                std::fs::write(&fp, &full)?;
                std::fs::write(&lp, &lossy)?;
                let hashes = |p: &std::path::Path| -> Result<(Vec<String>, String)> {
                    let out = std::process::Command::new("ffmpeg")
                        .args(["-v", "error", "-i"])
                        .arg(p)
                        .args(["-f", "framemd5", "-"])
                        .output()
                        .context("ffmpeg")?;
                    let md5s = String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .filter(|l| !l.starts_with('#'))
                        .filter_map(|l| l.rsplit(',').next().map(|h| h.trim().to_string()))
                        .collect();
                    Ok((md5s, String::from_utf8_lossy(&out.stderr).trim().to_string()))
                };
                let ((full_md5, _), (lossy_md5, errors)) = (hashes(&fp)?, hashes(&lp)?);
                let lost = crate::bench::REF_LOST.len();
                let recovered = lossy_md5.len() + lost == full_md5.len()
                    && lossy_md5[crate::bench::REF_LOST.start as usize..]
                        == full_md5[crate::bench::REF_LOST.end as usize..];
                let tail = lossy_md5.iter().rev().zip(full_md5.iter().rev()).take_while(|(a, b)| a == b).count();
                println!("    identical: last {tail} pictures (recovery at {})", crate::bench::REF_LOST.end);
                println!(
                    "{} ({}): references {}, lossy stream {} frames (of {}), decode errors: {}, pictures after recovery {}",
                    desc.name,
                    desc.codec,
                    if ltr { "accepted" } else { "not supported" },
                    lossy_md5.len(),
                    full_md5.len(),
                    if errors.is_empty() {
                        "none".to_string()
                    } else {
                        errors.lines().take(3).collect::<Vec<_>>().join(" | ")
                    },
                    if recovered { "IDENTICAL" } else { "DIFFERENT" }
                );
            }
            println!("streams in {}", dir.display());
        }
        Some("codecapi") => {
            // Optional encoder controls per selected encoder.
            let inv = encoder::probe(preferred_gpu());
            for (_, desc) in &inv.selected {
                let gpu = crate::d3d::create(desc.luid)?;
                let settings = encoder::EncoderSettings {
                    codec: desc.codec,
                    width: 1920,
                    height: 1080,
                    fps: 60,
                    bitrate_bps: 20_000_000,
                    color: secondscrn_protocol::ColorInfo::SDR_BT709_LIMITED,
                };
                let enc = encoder::Encoder::new(&gpu, desc, settings, |_| true)?;
                println!("{} ({}):", desc.name, desc.codec);
                for (name, ok) in enc.supported_controls() {
                    println!("    {name:30} {}", if ok { "yes" } else { "no" });
                }
            }
        }
        Some("adapters") => {
            for (_, a) in crate::d3d::adapters()? {
                println!(
                    "{:016x} {:40} vram {:>6} MB software={} {:?} (unified memory: {:?})",
                    crate::d3d::luid_to_u64(a.luid),
                    a.name,
                    a.dedicated_video_memory >> 20,
                    a.software,
                    a.kind(),
                    a.unified_memory
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
            let inv = encoder::probe(preferred_gpu());
            let desc = encoder::ensure_supported(&inv.selected, codec)?.clone();
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
            // Reads or moves the virtual monitor in the desktop arrangement.
            Some("position") => {
                let gdi = crate::display::active_vdd()?.context("the virtual monitor is not active")?;
                if let (Some(x), Some(y)) = (args.get(2), args.get(3)) {
                    crate::display::set_position(&gdi, x.parse()?, y.parse()?)?;
                }
                println!("{:?}", crate::display::placement(&gdi));
            }
            // Reads or sets the Windows scale factor of the virtual monitor.
            Some("scale") => {
                let gdi = crate::display::active_vdd()?.context("the virtual monitor is not active")?;
                match args.get(2) {
                    Some(p) => println!("{:?}", crate::display::set_ui_scale(&gdi, p.parse()?)?),
                    None => println!("{:?}", crate::display::ui_scale(&gdi)),
                }
            }
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
            eprintln!(
                "commands: probe | adapters | bench <w>x<h> <fps> [seconds] [--json] | monitors | record <device|primary> <codec> <seconds> <file> | serve"
            );
        }
    }
    Ok(())
}
