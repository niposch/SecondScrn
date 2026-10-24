//! File logging (daily rotation, 7 files kept) plus the in-memory buffer
//! shown in the Diagnostics page.

use std::path::Path;
use std::sync::OnceLock;

use secondscrn_diagnostics::LogBuffer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub fn init(dir: &Path, verbose: bool) -> LogBuffer {
    static LOGS: OnceLock<LogBuffer> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer = LogBuffer::new(5000);
        let level = if verbose { "debug,str0m=info,is=info" } else { "info,str0m=warn,is=warn" };
        let filter = tracing_subscriber::EnvFilter::try_from_env("SECONDSCRN_LOG").unwrap_or_else(|_| level.into());
        let _ = std::fs::create_dir_all(dir);
        let file = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("secondscrn-host")
            .filename_suffix("log")
            .max_log_files(7)
            .build(dir)
            .ok();
        let registry = tracing_subscriber::registry().with(filter).with(buffer.layer::<tracing_subscriber::Registry>());
        let result = match file {
            Some(f) => registry.with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(f)).try_init(),
            None => registry.try_init(),
        };
        if result.is_err() {
            // Already initialised (e.g. inside tests); keep the buffer anyway.
        }
        buffer
    })
    .clone()
}
