//! Implementation of the developer CLI (see `src/bin/cli.rs`).

use anyhow::{Result, bail};
use secondscrn_session::settings::EncoderGpu;

/// `SECONDSCRN_ENCODER_GPU=integrated|dedicated` prefers VA-API or NVENC.
fn preferred_gpu() -> Option<EncoderGpu> {
    match std::env::var("SECONDSCRN_ENCODER_GPU").as_deref() {
        Ok("integrated") => Some(EncoderGpu::Integrated),
        Ok("dedicated") => Some(EncoderGpu::Dedicated),
        _ => None,
    }
}

pub fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into())).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("probe") => {
            for (cap, enc) in crate::encoders::probe(preferred_gpu()) {
                println!(
                    "{:5} {:16} {:?} max {}x{}",
                    cap.codec.to_string(),
                    enc.factory,
                    enc.backend,
                    cap.max_width,
                    cap.max_height
                );
            }
            let portal = crate::portal::Portal::start(crate::default_data_dir());
            println!("desktop: {}", portal.status().detail);
        }
        Some("portal") => {
            // Opens the virtual monitor for a few seconds (manual test of the
            // portal dialog and the restore token).
            let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
            let portal = crate::portal::Portal::start(crate::default_data_dir());
            let m = portal.open().map_err(anyhow::Error::msg)?;
            println!("virtual monitor: node {} size {:?}", m.node_id, m.size);
            std::thread::sleep(std::time::Duration::from_secs(secs));
            portal.close();
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        Some("serve") => crate::serve::run(&args[1..])?,
        _ => bail!(
            "usage: secondscrn-linux-cli probe | portal [seconds] | serve [--test-pattern] [--pin-file <path>] [--data-dir <path>]"
        ),
    }
    Ok(())
}
