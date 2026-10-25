//! Upscaling of the decoded video with Snapdragon Game Super Resolution 1
//! (SGSR, BSD-3-Clause, `shaders/`), used when the PC streams below the
//! panel's resolution.
//!
//! Without it the decoder renders straight into the `SurfaceView` and the
//! compositor scales bilinearly. With it the decoder renders into an
//! `AImageReader`; a GL thread takes the newest picture, converts it to RGB
//! (pass 1, through the driver's YUV sampler) and runs the SGSR shader at the
//! panel's resolution into the window (pass 2). About a millisecond of GPU
//! time per frame on recent Adreno GPUs.
//!
//! SGSR 2 is not used: it is temporal and needs a game engine's motion
//! vectors and depth, which a video stream doesn't have.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use glow::HasContext;
use khronos_egl as egl;
use ndk::hardware_buffer::HardwareBufferUsage;
use ndk::media::image_reader::{AcquireResult, ImageFormat, ImageReader};
use ndk::native_window::NativeWindow;

const SGSR: &str = include_str!("shaders/sgsr1_shader_mobile.frag");
const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;
const EGL_NATIVE_BUFFER_ANDROID: egl::Int = 0x3140;
const EGL_IMAGE_PRESERVED_KHR: egl::Int = 0x30D2;
/// Pictures the decoder can have in flight to the reader.
const READER_IMAGES: i32 = 4;

/// Set by the UI; read when the decoder is (re)created.
static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// True on GPUs SGSR is made for (Adreno, OpenGL ES 3.1+). Probed once.
pub fn supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| match probe() {
        Ok(renderer) => {
            let ok = renderer.contains("Adreno");
            tracing::info!(%renderer, supported = ok, "super resolution");
            ok
        }
        Err(e) => {
            tracing::warn!(error = %e, "super resolution unavailable");
            false
        }
    })
}

type Egl = egl::Instance<egl::Static>;

// `khronos-egl` declares the functions; the NDK's libEGL provides them
// (pkg-config isn't available when cross-compiling). GLES entry points are
// resolved at run time through eglGetProcAddress.
#[link(name = "EGL")]
unsafe extern "C" {}

fn egl_setup(egl: &Egl) -> Result<(egl::Display, egl::Config), String> {
    let display = unsafe { egl.get_display(egl::DEFAULT_DISPLAY) }.ok_or("no EGL display")?;
    egl.initialize(display).map_err(|e| format!("eglInitialize: {e}"))?;
    let attrs = [
        egl::RENDERABLE_TYPE,
        egl::OPENGL_ES3_BIT,
        egl::SURFACE_TYPE,
        egl::WINDOW_BIT | egl::PBUFFER_BIT,
        egl::RED_SIZE,
        8,
        egl::GREEN_SIZE,
        8,
        egl::BLUE_SIZE,
        8,
        egl::NONE,
    ];
    let config = egl
        .choose_first_config(display, &attrs)
        .map_err(|e| format!("eglChooseConfig: {e}"))?
        .ok_or("no suitable EGL config")?;
    Ok((display, config))
}

fn probe() -> Result<String, String> {
    let egl = Egl::new(egl::Static);
    let (display, config) = egl_setup(&egl)?;
    let ctx = egl
        .create_context(display, config, None, &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE])
        .map_err(|e| format!("eglCreateContext: {e}"))?;
    let surface = egl
        .create_pbuffer_surface(display, config, &[egl::WIDTH, 1, egl::HEIGHT, 1, egl::NONE])
        .map_err(|e| format!("pbuffer: {e}"))?;
    egl.make_current(display, Some(surface), Some(surface), Some(ctx)).map_err(|e| format!("make current: {e}"))?;
    let gl = unsafe { glow::Context::from_loader_function(|s| loader(&egl, s)) };
    let renderer = unsafe { gl.get_parameter_string(glow::RENDERER) };
    let version = gl.version().clone();
    let _ = egl.make_current(display, None, None, None);
    let _ = egl.destroy_surface(display, surface);
    let _ = egl.destroy_context(display, ctx);
    if (version.major, version.minor) < (3, 1) {
        return Err(format!("OpenGL ES {}.{} (3.1 needed)", version.major, version.minor));
    }
    Ok(renderer)
}

fn loader(egl: &Egl, name: &str) -> *const c_void {
    egl.get_proc_address(name).map_or(std::ptr::null(), |f| f as *const c_void)
}

/// Runs the GL thread; dropping it stops the thread.
pub struct Upscaler {
    running: Arc<AtomicBool>,
    wake: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub presented: Arc<AtomicU64>,
}

impl Upscaler {
    /// Starts upscaling `width`x`height` pictures into `target`. Returns the
    /// window the decoder must render into.
    pub fn start(target: NativeWindow, width: u32, height: u32) -> Result<(Self, NativeWindow), String> {
        let running = Arc::new(AtomicBool::new(true));
        let wake = Arc::new((Mutex::new(false), Condvar::new()));
        let presented = Arc::new(AtomicU64::new(0));
        let (tx, rx) = std::sync::mpsc::channel();
        let (r, w, p) = (running.clone(), wake.clone(), presented.clone());
        let thread = std::thread::Builder::new()
            .name("secondscrn-upscale".into())
            .spawn(move || {
                // The reader lives on this thread: its callbacks only notify.
                let mut reader = match ImageReader::new_with_usage(
                    width as i32,
                    height as i32,
                    ImageFormat::PRIVATE,
                    HardwareBufferUsage::GPU_SAMPLED_IMAGE,
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
                let mut gl = match Renderer::new(&target) {
                    Ok(g) => g,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                match reader.window() {
                    Ok(win) => {
                        let _ = tx.send(Ok(win));
                    }
                    Err(e) => {
                        let _ = tx.send(Err(format!("reader window: {e:?}")));
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
                    // Only the newest picture matters (lowest latency).
                    let image = match reader.acquire_latest_image() {
                        Ok(AcquireResult::Image(i)) => i,
                        Ok(_) => continue,
                        Err(e) => {
                            tracing::debug!(error = ?e, "acquire image");
                            continue;
                        }
                    };
                    match gl.draw(&image) {
                        Ok(()) => {
                            p.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => tracing::warn!(error = %e, "upscaling failed"),
                    }
                }
                drop(gl);
                drop(reader);
            })
            .map_err(|e| e.to_string())?;
        let window = rx.recv_timeout(Duration::from_secs(3)).map_err(|_| "upscaler did not start".to_string())??;
        tracing::info!(width, height, "super resolution active");
        Ok((Upscaler { running, wake, thread: Some(thread), presented }, window))
    }
}

impl Drop for Upscaler {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        self.wake.1.notify_one();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

type GetNativeClientBuffer = unsafe extern "C" fn(*const c_void) -> *mut c_void;
type CreateImage =
    unsafe extern "C" fn(*mut c_void, *mut c_void, egl::Enum, *mut c_void, *const egl::Int) -> *mut c_void;
type DestroyImage = unsafe extern "C" fn(*mut c_void, *mut c_void) -> egl::Boolean;
type ImageTargetTexture = unsafe extern "C" fn(u32, *mut c_void);

struct Renderer {
    egl: Egl,
    display: egl::Display,
    context: egl::Context,
    surface: egl::Surface,
    gl: glow::Context,
    convert: glow::Program,
    sgsr: glow::Program,
    vao: glow::VertexArray,
    external: glow::Texture,
    rgb: Option<(glow::Texture, glow::Framebuffer, i32, i32)>,
    get_client_buffer: GetNativeClientBuffer,
    create_image: CreateImage,
    destroy_image: DestroyImage,
    image_target: ImageTargetTexture,
    // Keeps the target window referenced while its surface exists.
    _window: NativeWindow,
}

const VERTEX: &str = r#"#version 310 es
layout(location = 0) out highp vec4 in_TEXCOORD0;
uniform highp float flip;
void main() {
    // Full-screen triangle.
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    in_TEXCOORD0 = vec4(p.x, flip > 0.5 ? 1.0 - p.y : p.y, 0.0, 0.0);
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const CONVERT: &str = r#"#version 310 es
#extension GL_OES_EGL_image_external_essl3 : require
precision mediump float;
uniform samplerExternalOES src;
uniform highp vec2 uv_scale;
layout(location = 0) in highp vec4 in_TEXCOORD0;
layout(location = 0) out vec4 color;
void main() {
    color = vec4(texture(src, in_TEXCOORD0.xy * uv_scale).rgb, 1.0);
}
"#;

fn compile(gl: &glow::Context, vertex: &str, fragment: &str) -> Result<glow::Program, String> {
    unsafe {
        let program = gl.create_program()?;
        for (kind, src) in [(glow::VERTEX_SHADER, vertex), (glow::FRAGMENT_SHADER, fragment)] {
            let s = gl.create_shader(kind)?;
            gl.shader_source(s, src);
            gl.compile_shader(s);
            if !gl.get_shader_compile_status(s) {
                return Err(format!("shader: {}", gl.get_shader_info_log(s)));
            }
            gl.attach_shader(program, s);
        }
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            return Err(format!("program: {}", gl.get_program_info_log(program)));
        }
        Ok(program)
    }
}

impl Renderer {
    fn new(window: &NativeWindow) -> Result<Self, String> {
        let egl = Egl::new(egl::Static);
        let (display, config) = egl_setup(&egl)?;
        let context = egl
            .create_context(display, config, None, &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE])
            .map_err(|e| format!("eglCreateContext: {e}"))?;
        let surface = unsafe { egl.create_window_surface(display, config, window.ptr().as_ptr() as _, None) }
            .map_err(|e| format!("window surface: {e}"))?;
        egl.make_current(display, Some(surface), Some(surface), Some(context))
            .map_err(|e| format!("make current: {e}"))?;
        // Present as soon as a frame is drawn; don't wait for vsync here.
        let _ = egl.swap_interval(display, 0);
        let gl = unsafe { glow::Context::from_loader_function(|s| loader(&egl, s)) };
        let ext = |name: &str| egl.get_proc_address(name).ok_or(format!("{name} missing"));
        let get_client_buffer: GetNativeClientBuffer =
            unsafe { std::mem::transmute(ext("eglGetNativeClientBufferANDROID")?) };
        let create_image: CreateImage = unsafe { std::mem::transmute(ext("eglCreateImageKHR")?) };
        let destroy_image: DestroyImage = unsafe { std::mem::transmute(ext("eglDestroyImageKHR")?) };
        let image_target: ImageTargetTexture = unsafe { std::mem::transmute(ext("glEGLImageTargetTexture2DOES")?) };
        // The SGSR source declares ES 3.0 but uses textureGather (ES 3.1).
        let sgsr_src = SGSR.replacen("#version 300 es", "#version 310 es", 1);
        let convert = compile(&gl, VERTEX, CONVERT)?;
        let sgsr = compile(&gl, VERTEX, &sgsr_src)?;
        let (vao, external) = unsafe { (gl.create_vertex_array()?, gl.create_texture()?) };
        unsafe {
            gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(external));
            gl.tex_parameter_i32(GL_TEXTURE_EXTERNAL_OES, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(GL_TEXTURE_EXTERNAL_OES, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        }
        Ok(Renderer {
            egl,
            display,
            context,
            surface,
            gl,
            convert,
            sgsr,
            vao,
            external,
            rgb: None,
            get_client_buffer,
            create_image,
            destroy_image,
            image_target,
            _window: window.clone(),
        })
    }

    /// RGB texture + framebuffer at the picture's size.
    fn rgb_target(&mut self, w: i32, h: i32) -> Result<(glow::Texture, glow::Framebuffer), String> {
        if let Some((t, f, tw, th)) = self.rgb
            && (tw, th) == (w, h)
        {
            return Ok((t, f));
        }
        let gl = &self.gl;
        unsafe {
            if let Some((t, f, _, _)) = self.rgb.take() {
                gl.delete_texture(t);
                gl.delete_framebuffer(f);
            }
            let t = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(t));
            gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::RGBA8, w, h);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            let f = gl.create_framebuffer()?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(f));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(t), 0);
            self.rgb = Some((t, f, w, h));
            Ok((t, f))
        }
    }

    fn draw(&mut self, image: &ndk::media::image_reader::Image) -> Result<(), String> {
        let buffer = image.hardware_buffer().map_err(|e| format!("hardware buffer: {e:?}"))?;
        let desc = buffer.describe();
        let crop = image.crop_rect().map_err(|e| format!("crop: {e:?}"))?;
        let (w, h) = (crop.right - crop.left, crop.bottom - crop.top);
        let out_w = self.egl.query_surface(self.display, self.surface, egl::WIDTH).unwrap_or(w);
        let out_h = self.egl.query_surface(self.display, self.surface, egl::HEIGHT).unwrap_or(h);
        let attrs = [EGL_IMAGE_PRESERVED_KHR, egl::TRUE as egl::Int, egl::NONE];
        let egl_image = unsafe {
            let client = (self.get_client_buffer)(buffer.as_ptr() as *const c_void);
            (self.create_image)(
                self.display.as_ptr(),
                std::ptr::null_mut(),
                EGL_NATIVE_BUFFER_ANDROID as egl::Enum,
                client,
                attrs.as_ptr(),
            )
        };
        if egl_image.is_null() {
            return Err("eglCreateImageKHR failed".into());
        }
        let (rgb, fbo) = self.rgb_target(w, h)?;
        let gl = &self.gl;
        unsafe {
            gl.bind_vertex_array(Some(self.vao));
            // Pass 1: YUV picture -> RGB at its own size (flipped so the
            // texture has GL's bottom-up orientation).
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(self.external));
            (self.image_target)(GL_TEXTURE_EXTERNAL_OES, egl_image);
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.viewport(0, 0, w, h);
            gl.use_program(Some(self.convert));
            gl.uniform_1_i32(gl.get_uniform_location(self.convert, "src").as_ref(), 0);
            gl.uniform_1_f32(gl.get_uniform_location(self.convert, "flip").as_ref(), 1.0);
            gl.uniform_2_f32(
                gl.get_uniform_location(self.convert, "uv_scale").as_ref(),
                (crop.right as f32) / desc.width as f32,
                (crop.bottom as f32) / desc.height as f32,
            );
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            // Pass 2: SGSR to the window at panel resolution.
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.viewport(0, 0, out_w, out_h);
            gl.use_program(Some(self.sgsr));
            gl.bind_texture(glow::TEXTURE_2D, Some(rgb));
            gl.uniform_1_i32(gl.get_uniform_location(self.sgsr, "ps0").as_ref(), 0);
            gl.uniform_1_f32(gl.get_uniform_location(self.sgsr, "flip").as_ref(), 0.0);
            gl.uniform_4_f32(
                gl.get_uniform_location(self.sgsr, "ViewportInfo[0]").as_ref(),
                1.0 / w as f32,
                1.0 / h as f32,
                w as f32,
                h as f32,
            );
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
        }
        let r = self.egl.swap_buffers(self.display, self.surface).map_err(|e| format!("swap: {e}"));
        unsafe { (self.destroy_image)(self.display.as_ptr(), egl_image) };
        r
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            self.gl.delete_program(self.convert);
            self.gl.delete_program(self.sgsr);
            if let Some((t, f, _, _)) = self.rgb.take() {
                self.gl.delete_texture(t);
                self.gl.delete_framebuffer(f);
            }
        }
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_surface(self.display, self.surface);
        let _ = self.egl.destroy_context(self.display, self.context);
    }
}
