//! Hardware video encoding through Media Foundation transforms (MFTs).
//!
//! Media Foundation exposes the vendors' hardware encoders (NVENC, AMD VCN,
//! Intel Quick Sync) behind one API, accepts D3D11 NV12 textures directly
//! (no GPU→CPU copy), and supports runtime bitrate changes and forced IDRs.
//! Only hardware MFTs are enumerated: there is deliberately no silent software
//! fallback.
//!
//! Hardware MFTs are asynchronous: they announce `METransformNeedInput` and
//! `METransformHaveOutput` events. A small thread blocks on the event queue
//! and forwards events to the owner through a channel.

use std::mem::ManuallyDrop;

use anyhow::{Context, Result, anyhow, bail};
use secondscrn_graphics::bitstream;
use secondscrn_protocol::{Codec, CodecCapability, ColorInfo, ColorRange};
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::core::{GUID, Interface, PWSTR};

use crate::d3d::{self, Gpu};

pub fn startup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
            tracing::error!(error = %e, "MFStartup failed");
        }
    });
}

fn subtype(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => MFVideoFormat_H264,
        Codec::H265 => MFVideoFormat_HEVC,
        Codec::Av1 => MFVideoFormat_AV1,
    }
}

fn variant_u32(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = v;
    }
    var
}

fn variant_bool(v: bool) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal =
            if v { windows::Win32::Foundation::VARIANT_TRUE } else { windows::Win32::Foundation::VARIANT_FALSE };
    }
    var
}

/// A hardware encoder found on the system.
#[derive(Clone)]
pub struct EncoderDescriptor {
    pub codec: Codec,
    pub name: String,
    pub luid: Option<LUID>,
    activate: IMFActivate,
}

unsafe impl Send for EncoderDescriptor {}
unsafe impl Sync for EncoderDescriptor {}

impl std::fmt::Debug for EncoderDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncoderDescriptor").field("codec", &self.codec).field("name", &self.name).finish()
    }
}

/// Lists hardware encoders for `codec`, best candidates first: encoders on
/// discrete GPUs (most dedicated memory) before integrated ones, then Media
/// Foundation's merit order. Each encoder is bound to its GPU by PCI vendor
/// ID; `MFTEnum2`'s adapter filter does not filter in practice.
pub fn enumerate(codec: Codec) -> Vec<EncoderDescriptor> {
    startup();
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype(codec) };
    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    let r = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut ptr,
            &mut count,
        )
    };
    if r.is_err() || ptr.is_null() {
        return Vec::new();
    }
    let activates: Vec<IMFActivate> =
        unsafe { (0..count as usize).filter_map(|i| std::ptr::read(ptr.add(i))).collect() };
    unsafe { CoTaskMemFree(Some(ptr as _)) };

    let adapters: Vec<d3d::AdapterInfo> =
        d3d::adapters().unwrap_or_default().into_iter().map(|(_, a)| a).filter(|a| !a.software).collect();
    let mut list: Vec<((std::cmp::Reverse<usize>, usize), EncoderDescriptor)> = Vec::new();
    for (i, activate) in activates.into_iter().enumerate() {
        let name = unsafe { string_attr(&activate, &MFT_FRIENDLY_NAME_Attribute) }
            .unwrap_or_else(|| format!("{codec} hardware encoder"));
        // "VEN_10DE" -> 0x10de
        let vendor = unsafe { string_attr(&activate, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute) }
            .and_then(|v| u32::from_str_radix(v.trim_start_matches("VEN_"), 16).ok());
        let adapter = match vendor {
            Some(v) => adapters.iter().find(|a| a.vendor_id == v),
            None => adapters.first(),
        };
        let Some(adapter) = adapter else {
            tracing::debug!(encoder = %name, ?vendor, "encoder has no matching GPU");
            continue;
        };
        list.push((
            (std::cmp::Reverse(adapter.dedicated_video_memory), i),
            EncoderDescriptor { codec, name, luid: Some(adapter.luid), activate },
        ));
    }
    list.sort_by_key(|(k, _)| *k);
    list.into_iter().map(|(_, d)| d).collect()
}

unsafe fn string_attr(attrs: &IMFActivate, key: &GUID) -> Option<String> {
    unsafe {
        let mut s = PWSTR::null();
        let mut len = 0;
        attrs.GetAllocatedString(key, &mut s, &mut len).ok()?;
        let out = s.to_string().ok();
        CoTaskMemFree(Some(s.0 as _));
        out
    }
}

#[derive(Debug, Clone)]
pub struct EncoderSettings {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u64,
    pub color: ColorInfo,
}

pub enum EncoderEvent {
    NeedInput,
    HaveOutput,
    Error(String),
}

#[derive(Debug)]
pub struct EncodedOutput {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub time_100ns: i64,
}

pub struct Encoder {
    transform: IMFTransform,
    /// Shut down on drop: otherwise the next `ActivateObject` on the same
    /// descriptor returns this (dead) transform again.
    activate: IMFActivate,
    codec_api: ICodecAPI,
    pub settings: EncoderSettings,
    pub name: String,
    provides_samples: bool,
    output_buffer_size: u32,
    headers: Vec<u8>,
    event_thread: Option<std::thread::JoinHandle<()>>,
    _device_manager: IMFDXGIDeviceManager,
}

unsafe impl Send for Encoder {}

struct SendPtr<T>(T);
unsafe impl<T> Send for SendPtr<T> {}

impl Encoder {
    /// Creates and configures the encoder. `on_event` is called from an
    /// event thread; returning false stops event delivery.
    pub fn new(
        gpu: &Gpu,
        desc: &EncoderDescriptor,
        settings: EncoderSettings,
        on_event: impl Fn(EncoderEvent) -> bool + Send + 'static,
    ) -> Result<Self> {
        startup();
        let transform: IMFTransform = unsafe { desc.activate.ActivateObject() }.context("activate encoder")?;
        let attrs = unsafe { transform.GetAttributes() }?;
        unsafe {
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).context("async unlock")?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }

        let mut token = 0u32;
        let mut manager = None;
        unsafe {
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        }
        let manager = manager.context("device manager")?;
        unsafe {
            manager.ResetDevice(&gpu.device, token)?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .context("encoder rejected the D3D11 device")?;
        }

        let codec_api: ICodecAPI = transform.cast().context("ICodecAPI")?;
        let set = |guid: &GUID, v: VARIANT, what: &str| {
            if let Err(e) = unsafe { codec_api.SetValue(guid, &v) } {
                tracing::debug!(setting = what, error = %e, "encoder setting not supported");
            }
        };
        set(
            &CODECAPI_AVEncCommonRateControlMode,
            variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
            "rate control",
        );
        set(&CODECAPI_AVEncCommonMeanBitRate, variant_u32(settings.bitrate_bps.min(u32::MAX as u64) as u32), "bitrate");
        set(&CODECAPI_AVLowLatencyMode, variant_bool(true), "low latency");
        set(&CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0), "b-frames");
        // Long GOP: IDRs are sent on demand (start, loss, decoder reset).
        set(&CODECAPI_AVEncMPVGOPSize, variant_u32(settings.fps.max(1) * 300), "gop");
        // Favour quality: desktop frames are mostly small deltas.
        set(&CODECAPI_AVEncCommonQualityVsSpeed, variant_u32(70), "quality vs speed");
        // VBV of ~2 frames keeps big updates from bursting the network.
        let vbv = (settings.bitrate_bps / settings.fps.max(1) as u64 * 2).min(u32::MAX as u64) as u32;
        set(&CODECAPI_AVEncCommonBufferSize, variant_u32(vbv), "vbv");

        let out_type = unsafe { MFCreateMediaType() }?;
        let in_type = unsafe { MFCreateMediaType() }?;
        let size = ((settings.width as u64) << 32) | settings.height as u64;
        let rate = ((settings.fps as u64) << 32) | 1;
        unsafe {
            for t in [&out_type, &in_type] {
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                t.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
                t.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
                t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
                t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
                t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
                t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
                let range = match settings.color.range {
                    ColorRange::Limited => MFNominalRange_16_235,
                    ColorRange::Full => MFNominalRange_0_255,
                };
                t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, range.0 as u32)?;
            }
            out_type.SetGUID(&MF_MT_SUBTYPE, &subtype(settings.codec))?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, settings.bitrate_bps.min(u32::MAX as u64) as u32)?;
            match settings.codec {
                Codec::H264 => out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?,
                Codec::H265 => out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?,
                Codec::Av1 => {}
            }
            transform.SetOutputType(0, &out_type, 0).context("encoder rejected output type")?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            transform.SetInputType(0, &in_type, 0).context("encoder rejected NV12 input")?;
        }

        let info = unsafe { transform.GetOutputStreamInfo(0) }?;
        let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        let headers = unsafe { sequence_header(&transform) };

        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        // The event thread must start blocking only after streaming began:
        // the NVIDIA MFT never delivers events to a GetEvent call that was
        // already waiting before START_OF_STREAM.
        let generator: IMFMediaEventGenerator = transform.cast()?;
        let gen_ptr = SendPtr(generator);
        let event_thread = std::thread::Builder::new().name("secondscrn-mf-events".into()).spawn(move || {
            let generator = gen_ptr;
            loop {
                let ev = match unsafe { generator.0.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) } {
                    Ok(e) => e,
                    Err(e) if e.code() == MF_E_SHUTDOWN => {
                        tracing::trace!("encoder event queue shut down");
                        return;
                    }
                    Err(e) => {
                        on_event(EncoderEvent::Error(e.to_string()));
                        return;
                    }
                };
                let kind = unsafe { ev.GetType() }.unwrap_or(0);
                tracing::trace!(kind, "encoder event");
                let mapped = match MF_EVENT_TYPE(kind as i32) {
                    t if t == METransformNeedInput => EncoderEvent::NeedInput,
                    t if t == METransformHaveOutput => EncoderEvent::HaveOutput,
                    _ => {
                        let status = unsafe { ev.GetStatus() }.unwrap_or_default();
                        if status.is_err() {
                            EncoderEvent::Error(format!("encoder event {kind}: {status:?}"))
                        } else {
                            continue;
                        }
                    }
                };
                if !on_event(mapped) {
                    return;
                }
            }
        })?;

        tracing::info!(encoder = %desc.name, codec = %settings.codec, width = settings.width, height = settings.height, fps = settings.fps, "encoder started");
        Ok(Encoder {
            transform,
            activate: desc.activate.clone(),
            codec_api,
            name: desc.name.clone(),
            settings,
            provides_samples,
            output_buffer_size: info.cbSize.max(1 << 20),
            headers,
            event_thread: Some(event_thread),
            _device_manager: manager,
        })
    }

    pub fn set_bitrate(&mut self, bps: u64) {
        let v = variant_u32(bps.min(u32::MAX as u64) as u32);
        match unsafe { self.codec_api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &v) } {
            Ok(()) => self.settings.bitrate_bps = bps,
            Err(e) => tracing::debug!(error = %e, "dynamic bitrate change rejected"),
        }
    }

    /// Submits an NV12 texture. Only call after a `NeedInput` event.
    pub fn encode(&mut self, texture: &ID3D11Texture2D, time_100ns: i64, keyframe: bool) -> Result<()> {
        unsafe {
            if keyframe {
                let _ = self.codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
            }
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                buffer.SetCurrentLength(b2.GetContiguousLength()?)?;
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time_100ns)?;
            sample.SetSampleDuration(10_000_000 / self.settings.fps.max(1) as i64)?;
            self.transform.ProcessInput(0, &sample, 0).context("ProcessInput")?;
        }
        Ok(())
    }

    /// Collects one output after a `HaveOutput` event.
    pub fn collect(&mut self) -> Result<Option<EncodedOutput>> {
        let provided = if self.provides_samples {
            None
        } else {
            unsafe {
                let s = MFCreateSample()?;
                let b = MFCreateMemoryBuffer(self.output_buffer_size)?;
                s.AddBuffer(&b)?;
                Some(s)
            }
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(provided),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let r = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
        let sample = unsafe { ManuallyDrop::take(&mut buffers[0].pSample) };
        let _events = unsafe { ManuallyDrop::take(&mut buffers[0].pEvents) };
        match r {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                unsafe {
                    let t = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &t, 0)?;
                    self.headers = sequence_header(&self.transform);
                }
                return Ok(None);
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
            Err(e) => return Err(anyhow!("ProcessOutput: {e}")),
        }
        let Some(sample) = sample else { return Ok(None) };
        let (data, time, clean) = unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;
            let time = sample.GetSampleTime().unwrap_or(0);
            let clean = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            (data, time, clean)
        };
        if self.headers.is_empty() {
            self.headers = unsafe { sequence_header(&self.transform) };
        }
        let codec = self.settings.codec;
        let keyframe = clean || bitstream::is_keyframe(codec, &data);
        let data = bitstream::ensure_parameter_sets(codec, data, &self.headers);
        Ok(Some(EncodedOutput { data, keyframe, time_100ns: time }))
    }
}

unsafe fn sequence_header(transform: &IMFTransform) -> Vec<u8> {
    unsafe {
        let Ok(t) = transform.GetOutputCurrentType(0) else { return Vec::new() };
        let Ok(size) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else { return Vec::new() };
        let mut buf = vec![0u8; size as usize];
        if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None).is_ok() { buf } else { Vec::new() }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(s) = self.transform.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
        }
        if let Some(t) = self.event_thread.take() {
            let _ = t.join();
        }
    }
}

/// Resolutions tried when probing an encoder's maximum size.
const PROBE_SIZES: [(u32, u32); 3] = [(3840, 2160), (2560, 1600), (1920, 1080)];

/// Verifies which codecs work by actually configuring the best encoder per
/// codec. Advertised-but-broken encoders are excluded.
pub fn probe() -> Vec<(CodecCapability, EncoderDescriptor)> {
    let mut out = Vec::new();
    for codec in Codec::ALL {
        'enc: for desc in enumerate(codec) {
            let gpu = match d3d::create(desc.luid) {
                Ok(g) => g,
                Err(e) => {
                    tracing::debug!(encoder = %desc.name, error = %e, "no device for encoder adapter");
                    continue;
                }
            };
            for (w, h) in PROBE_SIZES {
                let settings = EncoderSettings {
                    codec,
                    width: w,
                    height: h,
                    fps: 60,
                    bitrate_bps: 20_000_000,
                    color: ColorInfo::SDR_BT709_LIMITED,
                };
                match Encoder::new(&gpu, &desc, settings, |_| true) {
                    Ok(enc) => {
                        drop(enc);
                        out.push((
                            CodecCapability {
                                codec,
                                implementation: desc.name.clone(),
                                hardware: true,
                                max_width: w,
                                max_height: h,
                                max_fps: 60,
                            },
                            desc.clone(),
                        ));
                        break 'enc;
                    }
                    Err(e) => tracing::debug!(encoder = %desc.name, w, h, error = %e, "probe failed"),
                }
            }
        }
    }
    out
}

pub fn ensure_supported(caps: &[(CodecCapability, EncoderDescriptor)], codec: Codec) -> Result<&EncoderDescriptor> {
    match caps.iter().find(|(c, _)| c.codec == codec) {
        Some((_, d)) => Ok(d),
        None => bail!("no working hardware {codec} encoder"),
    }
}
