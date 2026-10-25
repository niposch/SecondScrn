//! Zero-copy presentation with `ASurfaceControl` (Android 10+).
//!
//! When the decoder renders straight into the `SurfaceView`, it can only
//! decode into the few buffers that window has, and has to wait until the
//! compositor gives each one back; on the test tablet that capped native
//! resolution at ~75-80 fps although the decoder alone kept up with 120.
//! Here the decoder renders into an `AImageReader` with a deeper queue and
//! never waits; a presenter thread hands the newest picture's hardware
//! buffer to a child surface layer of the `SurfaceView` (no GPU copy; the
//! compositor or display hardware scales it). A buffer goes back to the
//! reader when the compositor reports it is done with it.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ndk::hardware_buffer::HardwareBufferUsage;
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::native_window::NativeWindow;

/// Pictures between decoder and display: the decoder never waits as long as
/// fewer than this are held by the compositor.
const READER_IMAGES: i32 = 8;
/// `ADATASPACE_BT709`: BT.709 primaries and matrix, limited range (what the
/// encoder signals).
const ADATASPACE_BT709: i32 = 281_083_904;
const VISIBILITY_SHOW: i8 = 1;
const VISIBILITY_HIDE: i8 = 0;

#[repr(C)]
struct ARect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[allow(non_camel_case_types)]
type ASurfaceControl = c_void;
#[allow(non_camel_case_types)]
type ASurfaceTransaction = c_void;
#[allow(non_camel_case_types)]
type ASurfaceTransactionStats = c_void;
type OnComplete = unsafe extern "C" fn(context: *mut c_void, stats: *mut ASurfaceTransactionStats);

#[link(name = "android")]
unsafe extern "C" {
    fn ASurfaceControl_createFromWindow(
        parent: *mut c_void,
        debug_name: *const std::ffi::c_char,
    ) -> *mut ASurfaceControl;
    fn ASurfaceControl_release(sc: *mut ASurfaceControl);
    fn ASurfaceTransaction_create() -> *mut ASurfaceTransaction;
    fn ASurfaceTransaction_delete(t: *mut ASurfaceTransaction);
    fn ASurfaceTransaction_apply(t: *mut ASurfaceTransaction);
    fn ASurfaceTransaction_setOnComplete(t: *mut ASurfaceTransaction, context: *mut c_void, func: OnComplete);
    fn ASurfaceTransaction_setVisibility(t: *mut ASurfaceTransaction, sc: *mut ASurfaceControl, visibility: i8);
    fn ASurfaceTransaction_setBuffer(
        t: *mut ASurfaceTransaction,
        sc: *mut ASurfaceControl,
        buffer: *mut c_void,
        acquire_fence_fd: i32,
    );
    fn ASurfaceTransaction_setGeometry(
        t: *mut ASurfaceTransaction,
        sc: *mut ASurfaceControl,
        source: *const ARect,
        destination: *const ARect,
        transform: i32,
    );
    fn ASurfaceTransaction_setBufferDataSpace(t: *mut ASurfaceTransaction, sc: *mut ASurfaceControl, dataspace: i32);
    fn ASurfaceTransactionStats_getPreviousReleaseFenceFd(
        stats: *mut ASurfaceTransactionStats,
        sc: *mut ASurfaceControl,
    ) -> i32;
}

/// Android API level of the device.
fn api_level() -> u32 {
    let mut buf = [0u8; 92];
    let n = unsafe { libc::__system_property_get(c"ro.build.version.sdk".as_ptr(), buf.as_mut_ptr() as *mut _) };
    std::str::from_utf8(&buf[..n.max(0) as usize]).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

pub fn supported() -> bool {
    api_level() >= 29
}

/// State shared with the compositor's completion callbacks.
struct Shared {
    sc: *mut ASurfaceControl,
    /// Pictures handed to the compositor, oldest first. Each completed
    /// transaction releases the picture shown before it.
    in_flight: Mutex<VecDeque<Image>>,
}

// The surface control is used from the presenter thread and callbacks only
// to read the release fence, which the NDK allows from any thread.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

unsafe extern "C" fn on_complete(context: *mut c_void, stats: *mut ASurfaceTransactionStats) {
    let shared = unsafe { Arc::from_raw(context as *const Shared) };
    let fence = unsafe { ASurfaceTransactionStats_getPreviousReleaseFenceFd(stats, shared.sc) };
    let mut q = shared.in_flight.lock().unwrap_or_else(|e| e.into_inner());
    if q.len() >= 2 {
        let previous = q.pop_front().expect("len >= 2");
        drop(q);
        if fence >= 0 {
            previous.delete_async(unsafe { OwnedFd::from_raw_fd(fence) });
        } else {
            drop(previous);
        }
    } else if fence >= 0 {
        drop(unsafe { OwnedFd::from_raw_fd(fence) });
    }
}

pub struct Presenter {
    running: Arc<AtomicBool>,
    wake: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub presented: Arc<AtomicU64>,
}

impl Presenter {
    /// Starts presenting `width`x`height` pictures on `target`. Returns the
    /// window the decoder must render into.
    pub fn start(target: NativeWindow, width: u32, height: u32) -> Result<(Self, NativeWindow), String> {
        let running = Arc::new(AtomicBool::new(true));
        let wake = Arc::new((Mutex::new(false), Condvar::new()));
        let presented = Arc::new(AtomicU64::new(0));
        let (tx, rx) = std::sync::mpsc::channel();
        let (r, w, p) = (running.clone(), wake.clone(), presented.clone());
        let thread = std::thread::Builder::new()
            .name("secondscrn-present".into())
            .spawn(move || {
                let mut reader = match ImageReader::new_with_usage(
                    width as i32,
                    height as i32,
                    ImageFormat::PRIVATE,
                    HardwareBufferUsage::GPU_SAMPLED_IMAGE | HardwareBufferUsage::COMPOSER_OVERLAY,
                    READER_IMAGES,
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx.send(Err(format!("image reader: {e:?}")));
                        return;
                    }
                };
                let w2 = w.clone();
                let _ = reader.set_image_listener(Box::new(move |_| {
                    *w2.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
                    w2.1.notify_one();
                }));
                let sc = unsafe {
                    ASurfaceControl_createFromWindow(target.ptr().as_ptr() as *mut c_void, c"secondscrn-video".as_ptr())
                };
                if sc.is_null() {
                    let _ = tx.send(Err("ASurfaceControl_createFromWindow failed".into()));
                    return;
                }
                let shared = Arc::new(Shared { sc, in_flight: Mutex::new(VecDeque::new()) });
                match reader.window() {
                    Ok(win) => {
                        let _ = tx.send(Ok(win));
                    }
                    Err(e) => {
                        let _ = tx.send(Err(format!("reader window: {e:?}")));
                        unsafe { ASurfaceControl_release(sc) };
                        return;
                    }
                }
                while r.load(Ordering::Relaxed) {
                    {
                        let mut ready = w.0.lock().unwrap_or_else(|e| e.into_inner());
                        if !*ready {
                            ready =
                                w.1.wait_timeout(ready, Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()).0;
                        }
                        *ready = false;
                    }
                    // Newest picture only; the compositor waits on its fence.
                    let (image, fence) = match unsafe { reader.acquire_latest_image_async() } {
                        Ok(AcquireResult::Image(i)) => i,
                        Ok(_) => continue,
                        Err(e) => {
                            tracing::debug!(error = ?e, "acquire image");
                            continue;
                        }
                    };
                    let Ok(buffer) = image.hardware_buffer() else { continue };
                    let crop = image.crop_rect().ok();
                    let (out_w, out_h) = (target.width(), target.height());
                    unsafe {
                        let t = ASurfaceTransaction_create();
                        ASurfaceTransaction_setBuffer(
                            t,
                            sc,
                            buffer.as_ptr() as *mut c_void,
                            fence.map_or(-1, |f| f.into_raw_fd()),
                        );
                        let src =
                            crop.map_or(ARect { left: 0, top: 0, right: width as i32, bottom: height as i32 }, |c| {
                                ARect { left: c.left, top: c.top, right: c.right, bottom: c.bottom }
                            });
                        let dst = ARect { left: 0, top: 0, right: out_w, bottom: out_h };
                        ASurfaceTransaction_setGeometry(t, sc, &src, &dst, 0);
                        ASurfaceTransaction_setBufferDataSpace(t, sc, ADATASPACE_BT709);
                        ASurfaceTransaction_setVisibility(t, sc, VISIBILITY_SHOW);
                        shared.in_flight.lock().unwrap_or_else(|e| e.into_inner()).push_back(image);
                        let ctx = Arc::into_raw(shared.clone()) as *mut c_void;
                        ASurfaceTransaction_setOnComplete(t, ctx, on_complete);
                        ASurfaceTransaction_apply(t);
                        ASurfaceTransaction_delete(t);
                    }
                    p.fetch_add(1, Ordering::Relaxed);
                }
                unsafe {
                    let t = ASurfaceTransaction_create();
                    ASurfaceTransaction_setVisibility(t, sc, VISIBILITY_HIDE);
                    ASurfaceTransaction_apply(t);
                    ASurfaceTransaction_delete(t);
                    ASurfaceControl_release(sc);
                }
                // Deleting the reader invalidates its images: let the
                // compositor finish with the hidden layer, release what it
                // held, and only then drop the reader. Late callbacks find
                // the queue empty.
                std::thread::sleep(Duration::from_millis(50));
                shared.in_flight.lock().unwrap_or_else(|e| e.into_inner()).clear();
                drop(reader);
            })
            .map_err(|e| e.to_string())?;
        let window = rx.recv_timeout(Duration::from_secs(3)).map_err(|_| "presenter did not start".to_string())??;
        tracing::info!(width, height, "zero-copy presentation active");
        Ok((Presenter { running, wake, thread: Some(thread), presented }, window))
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        self.wake.1.notify_one();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
