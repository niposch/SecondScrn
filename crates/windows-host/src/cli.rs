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
            };
            let mut f = std::io::BufWriter::new(std::fs::File::create(file)?);
            let n = pipeline::record(monitor, config, desc, secs.parse()?, &mut f)?;
            println!("wrote {n} frames to {file}");
        }
        Some("serve") => crate::serve::run(&args[1..])?,
        _ => {
            eprintln!("commands: probe | monitors | record <device|primary> <codec> <seconds> <file> | serve");
        }
    }
    Ok(())
}
