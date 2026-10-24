//! Diagnostics shared by host and client.
//!
//! * [`LogBuffer`]: an in-memory ring of recent log lines, fed by a tracing
//!   layer, for the UI's log view and for diagnostic export.
//! * [`RateCounter`] / [`Ewma`]: cheap, lock-free-friendly measurement helpers
//!   for hot paths (no allocation, no logging per frame).
//! * [`redact`]: strips secrets and stable identifiers from exported text.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Bounded ring of formatted log lines.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<VecDeque<String>>>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        LogBuffer { inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))), capacity }
    }

    pub fn push(&self, line: String) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() == self.capacity {
            q.pop_front();
        }
        q.push_back(line);
    }

    /// The most recent `max` lines, oldest first.
    pub fn recent(&self, max: usize) -> Vec<String> {
        let q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        q.iter().skip(q.len().saturating_sub(max)).cloned().collect()
    }

    /// A tracing layer that records INFO and above into this buffer.
    pub fn layer<S>(&self) -> LogBufferLayer
    where
        S: tracing::Subscriber,
    {
        LogBufferLayer { buffer: self.clone(), started: Instant::now() }
    }
}

pub struct LogBufferLayer {
    buffer: LogBuffer,
    started: Instant,
}

struct FieldWriter<'a>(&'a mut String);

impl tracing::field::Visit for FieldWriter<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if field.name() == "message" {
            let _ = write!(self.0, " {value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for LogBufferLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if *meta.level() > tracing::Level::INFO {
            return;
        }
        let t = self.started.elapsed().as_secs_f64();
        let mut line = format!("[{t:9.3}] {:5} {}:", meta.level(), meta.target());
        event.record(&mut FieldWriter(&mut line));
        self.buffer.push(line);
    }
}

/// Counts events and reports a rate over a sliding one-second window.
#[derive(Debug)]
pub struct RateCounter {
    window_start: Instant,
    count: u64,
    last_rate: f32,
    total: u64,
}

impl Default for RateCounter {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

impl RateCounter {
    pub fn new(now: Instant) -> Self {
        RateCounter { window_start: now, count: 0, last_rate: 0.0, total: 0 }
    }

    pub fn tick(&mut self, now: Instant) {
        self.count += 1;
        self.total += 1;
        self.roll(now);
    }

    fn roll(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.window_start);
        if elapsed >= Duration::from_secs(1) {
            self.last_rate = self.count as f32 / elapsed.as_secs_f32();
            self.count = 0;
            self.window_start = now;
        }
    }

    /// Rate over the last completed window. Decays to 0 when ticks stop.
    pub fn rate(&mut self, now: Instant) -> f32 {
        self.roll(now);
        if now.saturating_duration_since(self.window_start) > Duration::from_secs(2) {
            0.0
        } else {
            self.last_rate
        }
    }

    pub fn total(&self) -> u64 {
        self.total
    }
}

/// Exponentially weighted moving average.
#[derive(Debug, Clone, Copy)]
pub struct Ewma {
    alpha: f32,
    value: Option<f32>,
}

impl Ewma {
    pub fn new(alpha: f32) -> Self {
        Ewma { alpha, value: None }
    }

    pub fn add(&mut self, sample: f32) {
        self.value = Some(match self.value {
            None => sample,
            Some(v) => v + self.alpha * (sample - v),
        });
    }

    pub fn get(&self) -> f32 {
        self.value.unwrap_or(0.0)
    }
}

/// Removes data that should not leave the device in a diagnostic export:
/// full certificate fingerprints / key material (long hex runs), base64 blobs
/// that may contain SPAKE2 messages, and PINs.
pub fn redact(text: &str) -> String {
    use std::sync::OnceLock;
    static HEX: OnceLock<regex_lite::Regex> = OnceLock::new();
    static B64: OnceLock<regex_lite::Regex> = OnceLock::new();
    static PIN: OnceLock<regex_lite::Regex> = OnceLock::new();
    let hex = HEX.get_or_init(|| regex_lite::Regex::new(r"\b[0-9a-fA-F]{32,}\b").unwrap());
    let b64 = B64.get_or_init(|| regex_lite::Regex::new(r"[A-Za-z0-9+/]{40,}={0,2}").unwrap());
    let pin = PIN.get_or_init(|| regex_lite::Regex::new(r"(?i)(pin\s*[=:]\s*)\d{4,8}").unwrap());
    let s = hex.replace_all(text, "<redacted-hex>");
    let s = b64.replace_all(&s, "<redacted-b64>");
    pin.replace_all(&s, "${1}<redacted>").into_owned()
}

/// Assembles a JSON diagnostic report from named sections, redacting every
/// string value.
pub fn export_report(sections: Vec<(&str, serde_json::Value)>, logs: &[String]) -> String {
    fn scrub(v: serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::String(s) => serde_json::Value::String(redact(&s)),
            serde_json::Value::Array(a) => serde_json::Value::Array(a.into_iter().map(scrub).collect()),
            serde_json::Value::Object(o) => {
                serde_json::Value::Object(o.into_iter().map(|(k, v)| (k, scrub(v))).collect())
            }
            other => other,
        }
    }
    let mut root = serde_json::Map::new();
    root.insert("format".into(), "secondscrn-diagnostics-v1".into());
    for (name, value) in sections {
        root.insert(name.to_string(), scrub(value));
    }
    root.insert("logs".into(), logs.iter().map(|l| serde_json::Value::String(redact(l))).collect());
    serde_json::to_string_pretty(&serde_json::Value::Object(root)).expect("serialisable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_buffer_is_bounded() {
        let b = LogBuffer::new(3);
        for i in 0..5 {
            b.push(i.to_string());
        }
        assert_eq!(b.recent(10), vec!["2", "3", "4"]);
        assert_eq!(b.recent(1), vec!["4"]);
    }

    #[test]
    fn rate_counter_measures_and_decays() {
        let t0 = Instant::now();
        let mut r = RateCounter::new(t0);
        for i in 0..60 {
            r.tick(t0 + Duration::from_millis(i * 1000 / 60));
        }
        let t1 = t0 + Duration::from_millis(1001);
        r.tick(t1);
        assert!((r.rate(t1) - 60.0).abs() < 2.0, "{}", r.rate(t1));
        assert_eq!(r.rate(t1 + Duration::from_secs(5)), 0.0);
        assert_eq!(r.total(), 61);
    }

    #[test]
    fn ewma_converges() {
        let mut e = Ewma::new(0.5);
        assert_eq!(e.get(), 0.0);
        e.add(10.0);
        assert_eq!(e.get(), 10.0);
        e.add(20.0);
        assert_eq!(e.get(), 15.0);
    }

    #[test]
    fn redaction_removes_secrets_but_keeps_context() {
        let fp = "a".repeat(64);
        let s = format!("peer fingerprint={fp} pin=123456 rtt=12ms spake=QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9w");
        let r = redact(&s);
        assert!(!r.contains(&fp));
        assert!(!r.contains("123456"));
        assert!(!r.contains("QUJDREVG"));
        assert!(r.contains("rtt=12ms"));
    }

    #[test]
    fn export_scrubs_nested_values() {
        let out = export_report(
            vec![("peer", serde_json::json!({"fp": "b".repeat(64), "name": "Tab"}))],
            &["pin: 654321".to_string()],
        );
        assert!(!out.contains(&"b".repeat(64)));
        assert!(!out.contains("654321"));
        assert!(out.contains("Tab"));
    }
}
