//! Monitor capture with Windows Graphics Capture (WGC).
//!
//! WGC delivers frames only when the monitor content changes, as D3D11
//! textures on our device (no CPU copy). It includes the cursor, works for
//! virtual (IddCx) monitors and handles cross-adapter composition.

use anyhow::{Context, Result};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::core::{BOOL, Interface};

use crate::d3d::Gpu;

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub handle: isize,
    /// GDI device name, e.g. `\\.\DISPLAY5`.
    pub device: String,
    /// Desktop coordinates (physical pixels for per-monitor-DPI-aware processes).
    pub rect: RECT,
    pub primary: bool,
}

pub fn monitors() -> Vec<MonitorInfo> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let list = unsafe { &mut *(data.0 as *mut Vec<MonitorInfo>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(h, &mut info as *mut _ as *mut MONITORINFO) }.as_bool() {
            let device = String::from_utf16_lossy(&info.szDevice).trim_end_matches('\0').to_string();
            list.push(MonitorInfo {
                handle: h.0 as isize,
                device,
                rect: info.monitorInfo.rcMonitor,
                primary: info.monitorInfo.dwFlags & 1 != 0,
            });
        }
        BOOL(1)
    }
    let mut list: Vec<MonitorInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

pub fn monitor_by_device(device: &str) -> Option<MonitorInfo> {
    monitors().into_iter().find(|m| m.device.eq_ignore_ascii_case(device))
}

pub fn ensure_winrt() {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
}

pub struct CapturedFrame {
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    // Keeps the pool buffer alive until the frame is converted.
    _frame: Direct3D11CaptureFrame,
}

pub struct MonitorCapture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    device: IDirect3DDevice,
    size: SizeInt32,
}

impl MonitorCapture {
    /// Starts capturing `monitor`. `on_frame` is invoked from a WGC thread
    /// whenever a new frame is available; fetch it with [`Self::latest`].
    pub fn start(gpu: &Gpu, monitor: &MonitorInfo, on_frame: impl Fn() + Send + 'static) -> Result<Self> {
        ensure_winrt();
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        // A monitor that was just added to the desktop is rejected by WGC
        // (E_INVALIDARG) for a moment; retry with a freshly resolved handle.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut handle = monitor.handle;
        let item: GraphicsCaptureItem = loop {
            match unsafe { interop.CreateForMonitor(HMONITOR(handle as _)) } {
                Ok(item) => break item,
                Err(e) if std::time::Instant::now() < deadline => {
                    tracing::debug!(monitor = %monitor.device, error = %e, "monitor not capturable yet, retrying");
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    if let Some(m) = monitor_by_device(&monitor.device) {
                        handle = m.handle;
                    }
                }
                Err(e) => return Err(e).context("CreateForMonitor"),
            }
        };
        let dxgi: IDXGIDevice = gpu.device.cast()?;
        let device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }?.cast()?;
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
            on_frame();
            Ok(())
        }))?;
        let session = pool.CreateCaptureSession(&item)?;
        // Optional APIs (newer Windows builds): no yellow border, include cursor.
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetIsCursorCaptureEnabled(true);
        session.StartCapture()?;
        tracing::info!(monitor = %monitor.device, width = size.Width, height = size.Height, "capture started");
        Ok(MonitorCapture { pool, session, device, size })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.size.Width as u32, self.size.Height as u32)
    }

    /// Returns the newest available frame, discarding older ones.
    pub fn latest(&mut self) -> Result<Option<CapturedFrame>> {
        let mut newest = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            newest = Some(frame);
        }
        let Some(frame) = newest else { return Ok(None) };
        let content = frame.ContentSize()?;
        if content.Width != self.size.Width || content.Height != self.size.Height {
            // The monitor mode changed; resize the pool. The caller decides
            // whether the encoder must restart.
            self.size = content;
            self.pool.Recreate(&self.device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, content)?;
        }
        let surface = frame.Surface()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        let texture: ID3D11Texture2D = unsafe { access.GetInterface() }?;
        Ok(Some(CapturedFrame { texture, width: content.Width as u32, height: content.Height as u32, _frame: frame }))
    }
}

impl Drop for MonitorCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
