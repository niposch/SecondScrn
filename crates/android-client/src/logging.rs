//! Logging: tracing → logcat (tag "SecondScrn") plus an in-memory buffer for
//! diagnostic export.

use std::sync::OnceLock;

use secondscrn_diagnostics::LogBuffer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub fn init() -> LogBuffer {
    static LOGS: OnceLock<LogBuffer> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer = LogBuffer::new(5000);
        let filter = tracing_subscriber::EnvFilter::new("info,str0m=warn,is=warn");
        let registry = tracing_subscriber::registry().with(filter).with(buffer.layer::<tracing_subscriber::Registry>());
        #[cfg(target_os = "android")]
        let registry = registry.with(
            tracing_subscriber::fmt::layer().with_ansi(false).without_time().with_writer(|| logcat::Writer),
        );
        let _ = registry.try_init();
        buffer
    })
    .clone()
}

#[cfg(target_os = "android")]
mod logcat {
    use std::ffi::CString;
    use std::io;

    pub struct Writer;

    impl io::Write for Writer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let text = String::from_utf8_lossy(buf);
            let text = text.trim_end();
            if !text.is_empty() {
                let prio = if text.contains("ERROR") {
                    android_log_sys::LogPriority::ERROR
                } else if text.contains("WARN") {
                    android_log_sys::LogPriority::WARN
                } else {
                    android_log_sys::LogPriority::INFO
                };
                let tag = CString::new("SecondScrn").unwrap();
                let msg = CString::new(text.replace('\0', "")).unwrap_or_default();
                unsafe {
                    android_log_sys::__android_log_write(prio as _, tag.as_ptr(), msg.as_ptr());
                }
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
