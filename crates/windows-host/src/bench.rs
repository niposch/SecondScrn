//! Encoder benchmark: how each GPU converts and encodes a stream of a given
//! size, so users can choose between the integrated and the dedicated GPU.
//!
//! The content is the test desktop pattern scrolling vertically (motion
//! everywhere, like scrolling a document: harder than typical desktop use).
//! It is generated on the encoding GPU, so screen capture and the network are
//! not part of the measurement. Each GPU first runs paced at the requested
//! frame rate (can it keep up, and how long does a frame take), then flat
//! out (how much headroom is left).
//!
//! Hybrid laptops only expose the dedicated GPU's encoder to processes that
//! Windows runs on that GPU (see [`crate::gpu`]), so the app runs this in the
//! helper executable, not in its own process.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use secondscrn_graphics::color::RgbToYuv;
use secondscrn_protocol::{Codec, ColorInfo, QualityPreset};
use secondscrn_session::adapt::BitratePolicy;
use secondscrn_session::settings::EncoderGpu;
use serde::{Deserialize, Serialize};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::convert::{self, Converter};
use crate::d3d;
use crate::encoder::{self, Encoder, EncoderDescriptor, EncoderEvent, EncoderSettings};

/// Frame rate of the headroom measurement (the highest a stream offers).
const MAX_RATE: u32 = 240;
/// Pixels the content moves per frame.
const SCROLL: u32 = 12;
const RING: usize = 4;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchResult {
    pub gpu: String,
    pub kind: EncoderGpu,
    pub encoder: String,
    pub codec: Codec,
    /// Frames per second encoded when frames arrive at the requested rate.
    pub paced_fps: f32,
    /// Average time from submitting a frame to its encoded output.
    pub latency_ms: f32,
    pub latency_p95_ms: f32,
    /// Frames per second when the GPU runs flat out.
    pub max_fps: f32,
    /// Set when the GPU could not be measured.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchReport {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub results: Vec<BenchResult>,
}

/// Encoders per GPU of `kind` (all GPUs when `None`): H.265 (what tablets
/// normally use) first, then H.264.
fn candidates(kind: Option<EncoderGpu>) -> Vec<EncoderDescriptor> {
    let mut out: Vec<EncoderDescriptor> = Vec::new();
    for codec in [Codec::H265, Codec::H264] {
        out.extend(encoder::enumerate(codec).into_iter().filter(|d| kind.is_none_or(|k| d.kind == k)));
    }
    out
}

/// Measures the GPUs of `kind`. An encoder runs at its best only in a
/// process Windows runs on its GPU (the integrated GPU's encoder was three
/// times slower in a process set to the dedicated GPU), so callers measure
/// each kind in a process started with the matching graphics preference.
pub fn run(width: u32, height: u32, fps: u32, seconds: f32, kind: Option<EncoderGpu>) -> BenchReport {
    let (width, height, fps) = (width & !1, height & !1, fps.clamp(10, 240));
    let mut results = Vec::new();
    let mut measured: Vec<u64> = Vec::new();
    for desc in candidates(kind) {
        let luid = desc.luid.map(d3d::luid_to_u64).unwrap_or(0);
        // A GPU whose H.265 encoder fails is retried with H.264.
        if measured.contains(&luid) {
            continue;
        }
        tracing::info!(gpu = %desc.gpu, encoder = %desc.name, width, height, fps, "benchmark");
        match run_one(&desc, width, height, fps, seconds) {
            Ok(r) => {
                measured.push(luid);
                results.retain(|x: &BenchResult| x.gpu != r.gpu);
                results.push(r);
            }
            Err(e) => {
                tracing::warn!(gpu = %desc.gpu, error = format!("{e:#}"), "benchmark failed");
                if !results.iter().any(|x| x.gpu == desc.gpu) {
                    results.push(BenchResult {
                        gpu: desc.gpu.clone(),
                        kind: desc.kind,
                        encoder: desc.name.clone(),
                        codec: desc.codec,
                        paced_fps: 0.0,
                        latency_ms: 0.0,
                        latency_p95_ms: 0.0,
                        max_fps: 0.0,
                        error: Some(format!("{e:#}")),
                    });
                }
            }
        }
    }
    // Integrated first: the default.
    results.sort_by_key(|r| r.kind != EncoderGpu::Integrated);
    BenchReport { width, height, fps, results }
}

fn texture(gpu: &d3d::Gpu, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let mut t = None;
    unsafe {
        gpu.device.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            },
            None,
            Some(&mut t),
        )?;
    }
    t.context("frame texture")
}

fn run_one(desc: &EncoderDescriptor, width: u32, height: u32, fps: u32, seconds: f32) -> Result<BenchResult> {
    let gpu = d3d::create(desc.luid)?;
    // Twice the height, so a window of it can scroll.
    let pattern = secondscrn_graphics::pattern::desktop(width as usize, 2 * height as usize);
    let source = convert::upload_bgra(&gpu, width, 2 * height, &pattern.bgra)?;
    let frame = texture(&gpu, width, height)?;
    let color = ColorInfo::SDR_BT709_LIMITED;
    let converter = Converter::new(&gpu, &RgbToYuv::for_color(&color))?;
    let srv = converter.source_view(&frame)?;
    let targets = (0..RING).map(|_| converter.create_target(width, height)).collect::<Result<Vec<_>>>()?;
    let bitrate = BitratePolicy::for_stream(desc.codec, width, height, fps, QualityPreset::Automatic, None).initial;
    let (tx, rx) = mpsc::channel();
    let mut enc = Encoder::new(
        &gpu,
        desc,
        EncoderSettings { codec: desc.codec, width, height, fps, bitrate_bps: bitrate, color },
        move |e| tx.send(e).is_ok(),
    )?;

    let start = Instant::now();
    let mut state = Run { need_input: 0, frame_no: 0, submitted: Vec::new(), latencies: Vec::new(), outputs: 0 };
    let submit = |enc: &mut Encoder, state: &mut Run| -> Result<()> {
        let offset = (state.frame_no * SCROLL) % height;
        let slot = (state.frame_no as usize) % RING;
        unsafe {
            let b = D3D11_BOX { left: 0, top: offset, front: 0, right: width, bottom: offset + height, back: 1 };
            gpu.context.CopySubresourceRegion(&frame, 0, 0, 0, 0, &source, 0, Some(&b));
        }
        converter.convert(&srv, &targets[slot]);
        let ts = (start.elapsed().as_nanos() / 100) as i64;
        enc.encode(&targets[slot].texture, ts, state.frame_no == 0)?;
        state.need_input -= 1;
        state.submitted.push((ts, Instant::now()));
        state.frame_no += 1;
        Ok(())
    };

    // Paced like the stream: a new picture every interval; one that is due
    // while the encoder is busy goes out as soon as it takes input, and
    // pictures it could not take in time are dropped (only the newest waits).
    let paced = |state: &mut Run, enc: &mut Encoder, rate: u32, duration: Duration| -> Result<f32> {
        let interval = Duration::from_secs(1) / rate;
        let (t0, before) = (Instant::now(), state.outputs);
        let mut next = t0;
        let mut due = false;
        while t0.elapsed() < duration {
            let now = Instant::now();
            if now >= next {
                due = true;
                next += interval;
                if next < now {
                    next = now + interval;
                }
            }
            if due && state.need_input > 0 {
                submit(enc, state)?;
                due = false;
            }
            let wait = if due { Duration::from_millis(1) } else { next.saturating_duration_since(Instant::now()) };
            state.pump(&rx, enc, wait.max(Duration::from_millis(1)))?;
        }
        Ok((state.outputs - before) as f32 / t0.elapsed().as_secs_f32())
    };
    let paced_fps = paced(&mut state, &mut enc, fps, Duration::from_secs_f32(seconds.max(0.5)))?;
    let warmup = state.latencies.len().min(fps as usize / 4);
    let mut lat: Vec<f32> = state.latencies[warmup..].to_vec();
    // Headroom: the same at the highest frame rate offered. Not flooding the
    // encoder with frames: Intel's low-latency encoder got slower that way,
    // and the stream never does it.
    let max_fps = paced(&mut state, &mut enc, MAX_RATE, Duration::from_secs_f32((seconds * 0.75).max(0.5)))?;
    drop(enc);

    if lat.is_empty() {
        bail!("the encoder produced no frames");
    }
    lat.sort_by(f32::total_cmp);
    let avg = lat.iter().sum::<f32>() / lat.len() as f32;
    let p95 = lat[((lat.len() as f32 * 0.95) as usize).min(lat.len() - 1)];
    Ok(BenchResult {
        gpu: desc.gpu.clone(),
        kind: desc.kind,
        encoder: desc.name.clone(),
        codec: desc.codec,
        paced_fps,
        latency_ms: avg,
        latency_p95_ms: p95,
        max_fps,
        error: None,
    })
}

struct Run {
    need_input: u32,
    frame_no: u32,
    submitted: Vec<(i64, Instant)>,
    latencies: Vec<f32>,
    outputs: u64,
}

impl Run {
    /// Handles encoder events for up to `wait`.
    fn pump(&mut self, rx: &mpsc::Receiver<EncoderEvent>, enc: &mut Encoder, wait: Duration) -> Result<()> {
        let mut timeout = wait;
        loop {
            match rx.recv_timeout(timeout) {
                Ok(EncoderEvent::NeedInput) => self.need_input += 1,
                Ok(EncoderEvent::HaveOutput) => {
                    if let Some(out) = enc.collect()? {
                        self.outputs += 1;
                        if let Some(i) = self.submitted.iter().position(|(ts, _)| *ts == out.time_100ns) {
                            let (_, t) = self.submitted.swap_remove(i);
                            self.latencies.push(t.elapsed().as_secs_f32() * 1000.0);
                        }
                    }
                }
                Ok(EncoderEvent::Error(e)) => bail!("encoder error: {e}"),
                Err(RecvTimeoutError::Timeout) => return Ok(()),
                Err(RecvTimeoutError::Disconnected) => bail!("encoder stopped"),
            }
            // Drain what is already queued, then return to the caller.
            timeout = Duration::ZERO;
        }
    }
}
