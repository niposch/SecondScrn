//! Hardware encoders through GStreamer, verified by encoding a test picture.
//!
//! Per codec the candidates are VA-API (Intel, AMD: the low-power variant
//! first), NVENC (NVIDIA) and, for H.264 only, software encoders as a last
//! resort. The user's encoder GPU choice orders VA-API (integrated GPUs, or
//! the GPU driving the screens) and NVENC (dedicated NVIDIA GPUs).

use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use secondscrn_protocol::{Codec, CodecCapability};
use secondscrn_session::settings::EncoderGpu;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    VaApi,
    Nvenc,
    Software,
}

/// One encoder element and how to feed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoder {
    pub codec: Codec,
    pub factory: &'static str,
    pub backend: Backend,
}

const CANDIDATES: &[(Codec, Backend, &str)] = &[
    (Codec::H265, Backend::VaApi, "vah265lpenc"),
    (Codec::H265, Backend::VaApi, "vah265enc"),
    (Codec::H265, Backend::Nvenc, "nvcudah265enc"),
    (Codec::H265, Backend::Nvenc, "nvh265enc"),
    (Codec::H264, Backend::VaApi, "vah264lpenc"),
    (Codec::H264, Backend::VaApi, "vah264enc"),
    (Codec::H264, Backend::Nvenc, "nvcudah264enc"),
    (Codec::H264, Backend::Nvenc, "nvh264enc"),
    (Codec::H264, Backend::Software, "x264enc"),
    (Codec::H264, Backend::Software, "openh264enc"),
    (Codec::Av1, Backend::VaApi, "vaav1lpenc"),
    (Codec::Av1, Backend::VaApi, "vaav1enc"),
    (Codec::Av1, Backend::Nvenc, "nvcudaav1enc"),
    (Codec::Av1, Backend::Nvenc, "nvav1enc"),
];

/// Sizes tried when verifying an encoder (largest first).
const PROBE_SIZES: [(u32, u32); 2] = [(3840, 2160), (1920, 1080)];

impl Encoder {
    /// Elements from raw video (any format the converter accepts) to the
    /// encoder, named `enc`, and its parser producing access units.
    pub fn chain(&self) -> String {
        let convert = match self.backend {
            // Converted on the GPU; BT.709 limited range is what the tablet
            // expects (and what the encoder then signals).
            Backend::VaApi => "vapostproc ! video/x-raw(memory:VAMemory),format=NV12,colorimetry=bt709".to_string(),
            Backend::Nvenc => {
                "cudaupload ! cudaconvert ! video/x-raw(memory:CUDAMemory),format=NV12,colorimetry=bt709".to_string()
            }
            Backend::Software => "videoconvert ! video/x-raw,format=I420,colorimetry=bt709".to_string(),
        };
        let parse = match self.codec {
            Codec::H264 => "h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au",
            Codec::H265 => "h265parse config-interval=-1 ! video/x-h265,stream-format=byte-stream,alignment=au",
            Codec::Av1 => "av1parse ! video/x-av1,stream-format=obu-stream,alignment=tu",
        };
        format!("{convert} ! {} name=enc ! {parse}", self.factory)
    }

    /// Low-latency settings: constant bitrate, no B-frames, keyframes only on
    /// request (the session asks after loss), fastest presets.
    pub fn configure(&self, enc: &gst::Element, bitrate_bps: u64, fps: u32) {
        let kbps = (bitrate_bps / 1000).max(100).to_string();
        let gop = (fps.max(1) * 300).to_string();
        let settings: &[(&str, &str)] = match self.backend {
            Backend::VaApi => &[
                ("rate-control", "cbr"),
                ("bitrate", &kbps),
                ("key-int-max", &gop),
                ("b-frames", "0"),
                ("ref-frames", "1"),
                // 1 = best quality … 7 = fastest.
                ("target-usage", "6"),
            ],
            Backend::Nvenc => &[
                ("preset", "p1"),
                ("tune", "ultra-low-latency"),
                ("rate-control", "cbr"),
                ("bitrate", &kbps),
                ("gop-size", "-1"),
                ("b-frames", "0"),
                ("zero-reorder-delay", "true"),
            ],
            Backend::Software => &[
                ("tune", "zerolatency"),
                ("speed-preset", "ultrafast"),
                ("bitrate", &kbps),
                ("key-int-max", &gop),
                ("bframes", "0"),
                ("sliced-threads", "true"),
                // openh264enc
                ("rate-control", "bitrate"),
                ("complexity", "low"),
            ],
        };
        for (name, value) in settings {
            set_if_exists(enc, name, value);
        }
    }

    /// Changes the bitrate of a running encoder.
    pub fn set_bitrate(&self, enc: &gst::Element, bitrate_bps: u64) {
        set_if_exists(enc, "bitrate", &(bitrate_bps / 1000).max(100).to_string());
    }
}

/// Sets `name` from its string form if the element has such a property
/// (encoders of different versions and vendors differ).
pub fn set_if_exists(element: &gst::Element, name: &str, value: &str) {
    let Some(pspec) = element.find_property(name) else { return };
    match gst::glib::Value::deserialize_with_pspec(value, &pspec) {
        Ok(mut v) => {
            // Out-of-range values (VA-API caps the keyframe interval at
            // 1024) are clamped to the property's range instead.
            use gst::glib::translate::{ToGlibPtr, ToGlibPtrMut};
            // SAFETY: both pointers are valid for the call; the value has
            // the property's type (deserialized with its spec).
            let clamped =
                unsafe { gst::glib::gobject_ffi::g_param_value_validate(pspec.to_glib_none().0, v.to_glib_none_mut().0) };
            if clamped != 0 {
                tracing::debug!(element = %element.name(), name, value, "encoder setting clamped");
            }
            element.set_property_from_value(name, &v)
        }
        Err(_) => tracing::debug!(element = %element.name(), name, value, "encoder setting not applied"),
    }
}

/// Encodes a few test pictures at `width`×`height` with `encoder`.
fn try_encode(encoder: &Encoder, width: u32, height: u32) -> Result<(), String> {
    let desc = format!(
        "videotestsrc num-buffers=3 ! video/x-raw,format=BGRx,width={width},height={height},framerate=60/1 ! {} ! fakesink",
        encoder.chain()
    );
    let pipeline = gst::parse::launch(&desc).map_err(|e| e.to_string())?;
    let bin = pipeline.clone().downcast::<gst::Bin>().map_err(|_| "not a bin".to_string())?;
    if let Some(enc) = bin.by_name("enc") {
        encoder.configure(&enc, 8_000_000, 60);
    }
    let result = (|| {
        pipeline.set_state(gst::State::Playing).map_err(|e| e.to_string())?;
        let bus = pipeline.bus().ok_or("no bus")?;
        match bus.timed_pop_filtered(gst::ClockTime::from_seconds(8), &[gst::MessageType::Eos, gst::MessageType::Error])
        {
            Some(m) => match m.view() {
                gst::MessageView::Eos(_) => Ok(()),
                gst::MessageView::Error(e) => Err(e.error().to_string()),
                _ => Err("unexpected message".into()),
            },
            None => Err("timed out".into()),
        }
    })();
    let _ = pipeline.set_state(gst::State::Null);
    result
}

/// The verified encoders, one per codec, preferring `prefer`'s backend:
/// integrated → VA-API, dedicated → NVENC. Without a preference VA-API comes
/// first (the GPU the desktop runs on, usually), then NVENC. Software only if
/// no hardware encoder works for H.264.
pub fn probe(prefer: Option<EncoderGpu>) -> Vec<(CodecCapability, Encoder)> {
    if let Err(e) = gst::init() {
        tracing::error!(error = %e, "GStreamer is not available");
        return Vec::new();
    }
    let rank = |b: Backend| match (prefer, b) {
        (_, Backend::Software) => 3,
        (Some(EncoderGpu::Dedicated), Backend::Nvenc) => 0,
        (Some(EncoderGpu::Dedicated), Backend::VaApi) => 1,
        (_, Backend::VaApi) => 0,
        (_, Backend::Nvenc) => 1,
    };
    // Development override: `SECONDSCRN_ENCODERS=x264enc,vah265enc` limits
    // the candidates (e.g. software only where the GPU is virtualised).
    let only = std::env::var("SECONDSCRN_ENCODERS").ok().filter(|s| !s.trim().is_empty());
    let allowed = |f: &str| only.as_ref().is_none_or(|o| o.split(',').any(|x| x.trim() == f));
    let mut out = Vec::new();
    for codec in Codec::ALL {
        let mut candidates: Vec<&(Codec, Backend, &str)> =
            CANDIDATES.iter().filter(|(c, _, f)| *c == codec && allowed(f)).collect();
        candidates.sort_by_key(|(_, b, _)| rank(*b));
        for (codec, backend, factory) in candidates {
            if gst::ElementFactory::find(factory).is_none() {
                continue;
            }
            let encoder = Encoder { codec: *codec, factory, backend: *backend };
            let size = PROBE_SIZES.iter().find(|(w, h)| match try_encode(&encoder, *w, *h) {
                Ok(()) => true,
                Err(e) => {
                    tracing::debug!(encoder = factory, w, h, error = %e, "probe failed");
                    false
                }
            });
            if let Some(&(w, h)) = size {
                tracing::info!(codec = %codec, encoder = factory, max = format!("{w}x{h}"), "encoder verified");
                out.push((
                    CodecCapability {
                        codec: *codec,
                        implementation: factory.to_string(),
                        hardware: *backend != Backend::Software,
                        max_width: w,
                        max_height: h,
                        max_fps: 240,
                    },
                    encoder,
                ));
                break;
            }
        }
    }
    if out.is_empty() {
        tracing::error!("no working video encoder found (install GStreamer's VA-API or NVIDIA plugins)");
    }
    out
}

/// Waits briefly for a GStreamer message (helper for pipelines).
pub fn poll_error(bus: &gst::Bus, wait: Duration) -> Option<String> {
    let msg =
        bus.timed_pop_filtered(gst::ClockTime::from_nseconds(wait.as_nanos() as u64), &[gst::MessageType::Error])?;
    match msg.view() {
        gst::MessageView::Error(e) => {
            Some(format!("{}: {}", msg.src().map(|s| s.name().to_string()).unwrap_or_default(), e.error()))
        }
        _ => None,
    }
}
