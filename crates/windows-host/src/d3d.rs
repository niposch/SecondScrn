//! Direct3D 11 device creation.

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

#[derive(Clone)]
pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub luid: LUID,
    pub adapter_name: String,
}

// D3D11 devices are free-threaded; the immediate context is protected with
// SetMultithreadProtected because Media Foundation also uses it.
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

#[derive(Debug, Clone)]
pub struct AdapterInfo {
    pub luid: LUID,
    pub name: String,
    pub software: bool,
    pub dedicated_video_memory: usize,
    pub vendor_id: u32,
    pub device_id: u32,
    /// The GPU shares system memory (D3D11 `UnifiedMemoryArchitecture`).
    pub unified_memory: Option<bool>,
}

impl AdapterInfo {
    /// Integrated GPUs share system memory. Their dedicated memory is only a
    /// carve-out, but that is not reliably small (AMD APUs: 2 GB and more
    /// depending on the BIOS), so it is only the fallback when the driver
    /// doesn't answer.
    pub fn kind(&self) -> secondscrn_session::settings::EncoderGpu {
        use secondscrn_session::settings::EncoderGpu;
        let integrated = self.unified_memory.unwrap_or(self.dedicated_video_memory <= 512 << 20);
        if integrated { EncoderGpu::Integrated } else { EncoderGpu::Dedicated }
    }
}

/// Asks the driver whether the adapter has a unified memory architecture.
/// Cached per adapter: answering takes a device.
fn unified_memory(adapter: &IDXGIAdapter1, luid: LUID) -> Option<bool> {
    static CACHE: std::sync::Mutex<Vec<(u64, Option<bool>)>> = std::sync::Mutex::new(Vec::new());
    let key = luid_to_u64(luid);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, uma)) = cache.iter().find(|(k, _)| *k == key) {
        return *uma;
    }
    let uma = (|| {
        let mut device = None;
        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .ok()?;
        }
        let device: ID3D11Device = device?;
        let mut options = D3D11_FEATURE_DATA_D3D11_OPTIONS2::default();
        unsafe {
            device
                .CheckFeatureSupport(
                    D3D11_FEATURE_D3D11_OPTIONS2,
                    &mut options as *mut _ as *mut _,
                    std::mem::size_of_val(&options) as u32,
                )
                .ok()?;
        }
        Some(options.UnifiedMemoryArchitecture.as_bool())
    })();
    cache.push((key, uma));
    uma
}

pub fn luid_to_u64(l: LUID) -> u64 {
    ((l.HighPart as u32 as u64) << 32) | l.LowPart as u64
}

/// Physical adapters. Indirect-display (IddCx) drivers such as virtual
/// monitors appear as extra adapters carrying the name and IDs of the GPU
/// that renders them; only the first adapter per (vendor, device) is kept,
/// which DXGI lists before its IddCx shadows.
pub fn adapters() -> Result<Vec<(IDXGIAdapter1, AdapterInfo)>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    let mut out: Vec<(IDXGIAdapter1, AdapterInfo)> = Vec::new();
    for i in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else { break };
        let desc = unsafe { adapter.GetDesc1() }?;
        let name = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').to_string();
        let software = (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0;
        if !software && out.iter().any(|(_, a)| a.vendor_id == desc.VendorId && a.device_id == desc.DeviceId) {
            continue;
        }
        let unified_memory = if software { None } else { unified_memory(&adapter, desc.AdapterLuid) };
        out.push((
            adapter,
            AdapterInfo {
                luid: desc.AdapterLuid,
                name,
                software,
                dedicated_video_memory: desc.DedicatedVideoMemory,
                vendor_id: desc.VendorId,
                device_id: desc.DeviceId,
                unified_memory,
            },
        ));
    }
    Ok(out)
}

/// The GPU Windows runs this process on: DXGI lists it first (on hybrid
/// laptops according to the per-app graphics preference).
pub fn default_adapter() -> Option<LUID> {
    adapters().ok()?.into_iter().find(|(_, a)| !a.software).map(|(_, a)| a.luid)
}

/// Creates a device on the adapter with `luid`, or on the first hardware
/// adapter when `None`.
pub fn create(luid: Option<LUID>) -> Result<Gpu> {
    let list = adapters()?;
    let (adapter, info) = list
        .into_iter()
        .filter(|(_, i)| !i.software)
        .find(|(_, i)| luid.is_none_or(|l| l.LowPart == i.luid.LowPart && l.HighPart == i.luid.HighPart))
        .context("no matching hardware GPU adapter")?;
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice")?;
    }
    let device: ID3D11Device = device.context("no device")?;
    let context: ID3D11DeviceContext = context.context("no context")?;
    let mt: ID3D11Multithread = context.cast()?;
    unsafe {
        let _ = mt.SetMultithreadProtected(true);
    }
    tracing::debug!(adapter = %info.name, "created D3D11 device");
    Ok(Gpu { device, context, luid: info.luid, adapter_name: info.name })
}
