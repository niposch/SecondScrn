//! Injection of tablet input.
//!
//! * Touch and pen go through synthetic pointer devices
//!   (`InjectSyntheticPointerInput`, Windows 10 1809+). Windows sees a real
//!   touchscreen (tap, press-and-hold, pan, pinch, edge gestures, the touch
//!   keyboard) and a real pen (pressure, tilt, barrel button, eraser, hover).
//! * Mouse events (a mouse on the tablet, or touch in mouse mode) use
//!   `SendInput`, as does touch and pen if the synthetic devices are not
//!   available.
//!
//! Coordinates arrive normalised to the streamed display and are mapped onto
//! that monitor's rectangle within the virtual desktop. Only authenticated
//! sessions can reach this code (see the session engine).
//!
//! Limitation: like any non-elevated process, input cannot reach windows of
//! elevated applications (UIPI).

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use secondscrn_protocol::{InputEvent, PenAction, PointerAction, PointerButton, TouchContact};
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE, POINTER_FEEDBACK_DEFAULT,
    POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::Pointer::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PEN_FLAG_BARREL, PEN_FLAG_ERASER, PEN_FLAG_INVERTED, PEN_FLAG_NONE, PEN_MASK_PRESSURE,
    PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN, PT_TOUCH, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, TOUCH_MASK_CONTACTAREA, TOUCH_MASK_ORIENTATION, TOUCH_MASK_PRESSURE,
};

use crate::capture;

const WHEEL_DELTA: f32 = 120.0;
/// Contacts Windows tracks at once (Android reports up to 10 fingers).
const MAX_CONTACTS: u32 = 10;
/// Held contacts are released if the tablet goes silent this long (the
/// network dropped mid-gesture). Long enough for press-and-hold.
const STALE_CONTACTS: Duration = Duration::from_secs(10);

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

/// Maps normalised display coordinates to desktop pixels.
fn pixel(display_device: &str, x: f32, y: f32) -> Result<POINT> {
    let m = capture::monitor_by_device(display_device).context("streamed display is not part of the desktop")?;
    let r = m.rect;
    Ok(POINT {
        x: (r.left as f32 + x.clamp(0.0, 1.0) * ((r.right - r.left - 1).max(0) as f32)).round() as i32,
        y: (r.top as f32 + y.clamp(0.0, 1.0) * ((r.bottom - r.top - 1).max(0) as f32)).round() as i32,
    })
}

/// Maps normalised display coordinates to absolute virtual-desktop units
/// (0..=65535) as required by `MOUSEEVENTF_VIRTUALDESK`.
fn absolute(display_device: &str, x: f32, y: f32) -> Result<(i32, i32)> {
    let p = pixel(display_device, x, y)?;
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let nx = ((p.x - vx) as f32 * 65535.0 / (vw - 1).max(1) as f32).round() as i32;
    let ny = ((p.y - vy) as f32 * 65535.0 / (vh - 1).max(1) as f32).round() as i32;
    Ok((nx, ny))
}

fn send(inputs: &[INPUT]) -> Result<()> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        anyhow::bail!("SendInput injected {sent} of {} events (blocked by a higher-integrity window?)", inputs.len());
    }
    Ok(())
}

fn inject_mouse(display_device: &str, action: PointerAction, button: PointerButton, x: f32, y: f32) -> Result<()> {
    let abs = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
    let (nx, ny) = absolute(display_device, x, y)?;
    let (down, up) = match button {
        PointerButton::Primary => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        PointerButton::Secondary => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        PointerButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    };
    match action {
        PointerAction::Move => send(&[mouse(nx, ny, 0, abs)]),
        PointerAction::Down => send(&[mouse(nx, ny, 0, abs), mouse(nx, ny, 0, abs | down)]),
        PointerAction::Up | PointerAction::Cancel => send(&[mouse(nx, ny, 0, abs | up)]),
    }
}

/// A synthetic pointer device; destroyed on drop.
struct Device(HSYNTHETICPOINTERDEVICE);

// The handle is a plain user32 handle, usable from any thread.
unsafe impl Send for Device {}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe { DestroySyntheticPointerDevice(self.0) };
    }
}

#[derive(Default)]
enum Lazy {
    #[default]
    Untried,
    Ready(Device),
    /// Creation failed (old Windows); use mouse emulation.
    Unavailable,
}

impl Lazy {
    fn get(&mut self, kind: &str, create: impl FnOnce() -> windows::core::Result<HSYNTHETICPOINTERDEVICE>) -> bool {
        if let Lazy::Untried = self {
            *self = match create() {
                Ok(h) => {
                    tracing::info!(kind, "synthetic pointer device created");
                    Lazy::Ready(Device(h))
                }
                Err(e) => {
                    tracing::warn!(kind, error = %e, "synthetic pointer device unavailable; using mouse emulation");
                    Lazy::Unavailable
                }
            };
        }
        matches!(self, Lazy::Ready(_))
    }

    fn handle(&self) -> Option<HSYNTHETICPOINTERDEVICE> {
        match self {
            Lazy::Ready(d) => Some(d.0),
            _ => None,
        }
    }
}

struct Contact {
    /// Android pointer id.
    id: u32,
    /// Windows pointer id, 0..MAX_CONTACTS.
    slot: u32,
    at: POINT,
    primary: bool,
}

struct PenContact {
    info: POINTER_PEN_INFO,
}

// The info's handle fields (source device, target window) are always null.
unsafe impl Send for PenContact {}

/// Translates tablet input into Windows input; keeps the state real
/// touchscreens and pens would have (which fingers are down, whether the
/// pen touches the screen).
#[derive(Default)]
pub struct Injector {
    touch: Lazy,
    pen: Lazy,
    contacts: Vec<Contact>,
    pen_contact: Option<PenContact>,
    last_event: Option<Instant>,
    /// Mouse emulation of touch: the contact that drives the cursor.
    mouse_contact: Option<u32>,
    /// Mouse emulation of the pen: the button is down.
    pen_mouse_down: bool,
    /// Rejected frames, for rate-limited logging.
    failures: u64,
}

/// Half the side of the reported contact area, in pixels.
const CONTACT_RADIUS: i32 = 4;

fn touch_info(slot: u32, flags: POINTER_FLAGS, at: POINT) -> POINTER_TYPE_INFO {
    // Like a real digitizer (and Microsoft's injection samples): a contact
    // area, orientation and pressure, which gesture recognition relies on.
    let r = CONTACT_RADIUS;
    POINTER_TYPE_INFO {
        r#type: PT_TOUCH,
        Anonymous: POINTER_TYPE_INFO_0 {
            touchInfo: POINTER_TOUCH_INFO {
                pointerInfo: POINTER_INFO {
                    pointerType: PT_TOUCH,
                    pointerId: slot,
                    pointerFlags: flags,
                    ptPixelLocation: at,
                    ..Default::default()
                },
                touchMask: TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION | TOUCH_MASK_PRESSURE,
                rcContact: RECT { left: at.x - r, top: at.y - r, right: at.x + r, bottom: at.y + r },
                orientation: 90,
                pressure: 32000,
                ..Default::default()
            },
        },
    }
}

fn describe(infos: &[POINTER_TYPE_INFO]) -> String {
    infos
        .iter()
        .map(|i| {
            let p = unsafe { i.Anonymous.touchInfo.pointerInfo };
            format!("#{} {:#x} ({},{})", p.pointerId, p.pointerFlags.0, p.ptPixelLocation.x, p.ptPixelLocation.y)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn pen_type_info(info: POINTER_PEN_INFO) -> POINTER_TYPE_INFO {
    POINTER_TYPE_INFO { r#type: PT_PEN, Anonymous: POINTER_TYPE_INFO_0 { penInfo: info } }
}

impl Injector {
    pub fn inject(&mut self, display_device: &str, event: &InputEvent) -> Result<()> {
        self.last_event = Some(Instant::now());
        match event {
            InputEvent::Pointer { action, button, x, y, .. } => inject_mouse(display_device, *action, *button, *x, *y),
            InputEvent::Scroll { x, y, delta_x, delta_y, .. } => {
                let abs = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
                let (nx, ny) = absolute(display_device, *x, *y)?;
                let mut v = vec![mouse(nx, ny, 0, abs)];
                if *delta_y != 0.0 {
                    // Windows: positive wheel = away from the user (scroll up).
                    v.push(mouse(0, 0, (-delta_y * WHEEL_DELTA) as i32, MOUSEEVENTF_WHEEL));
                }
                if *delta_x != 0.0 {
                    v.push(mouse(0, 0, (delta_x * WHEEL_DELTA) as i32, MOUSEEVENTF_HWHEEL));
                }
                send(&v)
            }
            InputEvent::Touch { contacts, .. } => {
                if self.touch.get("touch", || unsafe {
                    CreateSyntheticPointerDevice(PT_TOUCH, MAX_CONTACTS, POINTER_FEEDBACK_DEFAULT)
                }) {
                    self.inject_touch(display_device, contacts)
                } else {
                    self.touch_as_mouse(display_device, contacts)
                }
            }
            &InputEvent::Pen { action, x, y, pressure, tilt_x, tilt_y, barrel, eraser, .. } => {
                if self.pen.get("pen", || unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) })
                {
                    self.inject_pen(display_device, action, x, y, pressure, tilt_x, tilt_y, barrel, eraser)
                } else {
                    let button = if barrel { PointerButton::Secondary } else { PointerButton::Primary };
                    let action = match (action, self.pen_mouse_down) {
                        (PenAction::Down, false) => PointerAction::Down,
                        (PenAction::Up | PenAction::Cancel | PenAction::Leave, true) => PointerAction::Up,
                        (PenAction::Leave, false) => return Ok(()),
                        _ => PointerAction::Move,
                    };
                    self.pen_mouse_down = match action {
                        PointerAction::Down => true,
                        PointerAction::Up => false,
                        _ => self.pen_mouse_down,
                    };
                    inject_mouse(display_device, action, button, x, y)
                }
            }
        }
    }

    fn inject_touch(&mut self, display_device: &str, frame: &[TouchContact]) -> Result<()> {
        let mut infos = Vec::with_capacity(frame.len() + self.contacts.len());
        let mut lifted = Vec::new();
        for c in frame {
            let at = pixel(display_device, c.x, c.y)?;
            let known = self.contacts.iter().position(|k| k.id == c.id);
            let lifting = matches!(c.action, PointerAction::Up | PointerAction::Cancel);
            let (slot, primary, flags) = match known {
                Some(i) => {
                    self.contacts[i].at = at;
                    let flags = match c.action {
                        PointerAction::Up => POINTER_FLAG_UP,
                        PointerAction::Cancel => POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
                        _ => POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
                    };
                    (self.contacts[i].slot, self.contacts[i].primary, flags)
                }
                // Lifting a contact Windows never saw: nothing to do.
                None if lifting => continue,
                None => {
                    let Some(slot) = (0..MAX_CONTACTS).find(|s| self.contacts.iter().all(|k| k.slot != *s)) else {
                        continue; // more fingers than Windows tracks
                    };
                    let primary = self.contacts.is_empty();
                    self.contacts.push(Contact { id: c.id, slot, at, primary });
                    (slot, primary, POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT)
                }
            };
            if lifting {
                lifted.push(c.id);
            }
            let flags = if primary { flags | POINTER_FLAG_PRIMARY } else { flags };
            infos.push(touch_info(slot, flags, at));
        }
        // A frame lists every finger on the screen; one that is missing was
        // withdrawn (palm rejection, overlay gesture) and is cancelled.
        for k in &self.contacts {
            if !frame.iter().any(|c| c.id == k.id) {
                infos.push(touch_info(k.slot, POINTER_FLAG_UP | POINTER_FLAG_CANCELED, k.at));
                lifted.push(k.id);
            }
        }
        self.contacts.retain(|k| !lifted.contains(&k.id));
        self.send_touch(&infos)
    }

    fn send_touch(&mut self, infos: &[POINTER_TYPE_INFO]) -> Result<()> {
        if infos.is_empty() {
            return Ok(());
        }
        let Some(h) = self.touch.handle() else { return Ok(()) };
        let r = unsafe { InjectSyntheticPointerInput(h, infos) };
        if let Err(e) = &r {
            self.failures += 1;
            if self.failures <= 20 || self.failures.is_multiple_of(100) {
                tracing::warn!(error = %e, failures = self.failures, frame = %describe(infos), "touch frame rejected");
            }
            // An invalid sequence would keep failing: lift what Windows may
            // still consider down, then start over.
            let lift: Vec<_> = self
                .contacts
                .iter()
                .map(|k| touch_info(k.slot, POINTER_FLAG_UP | POINTER_FLAG_CANCELED, k.at))
                .collect();
            if !lift.is_empty() {
                let _ = unsafe { InjectSyntheticPointerInput(h, &lift) };
            }
            self.contacts.clear();
        } else {
            tracing::trace!(frame = %describe(infos), "touch frame");
        }
        r.context("touch injection failed")
    }

    /// Touch through `SendInput`: the first finger drives the left button.
    fn touch_as_mouse(&mut self, display_device: &str, frame: &[TouchContact]) -> Result<()> {
        let c = match self.mouse_contact {
            Some(id) => frame.iter().find(|c| c.id == id),
            None => frame.iter().find(|c| c.action == PointerAction::Down),
        };
        let Some(c) = c else { return Ok(()) };
        match c.action {
            PointerAction::Down => self.mouse_contact = Some(c.id),
            PointerAction::Up | PointerAction::Cancel => self.mouse_contact = None,
            PointerAction::Move => {}
        }
        inject_mouse(display_device, c.action, PointerButton::Primary, c.x, c.y)
    }

    #[allow(clippy::too_many_arguments)]
    fn inject_pen(
        &mut self,
        display_device: &str,
        action: PenAction,
        x: f32,
        y: f32,
        pressure: f32,
        tilt_x: f32,
        tilt_y: f32,
        barrel: bool,
        eraser: bool,
    ) -> Result<()> {
        let at = pixel(display_device, x, y)?;
        let touching = self.pen_contact.is_some();
        let contact = POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_FIRSTBUTTON;
        let flags = match (action, touching) {
            (PenAction::Down | PenAction::Move, true) => POINTER_FLAG_UPDATE | contact,
            (PenAction::Down, false) => POINTER_FLAG_DOWN | contact,
            // Without contact these are hovers (also when a down was lost).
            (PenAction::Move | PenAction::Hover | PenAction::Up, false) => POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE,
            (PenAction::Hover | PenAction::Up, true) => POINTER_FLAG_UP | POINTER_FLAG_INRANGE,
            (PenAction::Cancel, true) => POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
            (PenAction::Leave, true) => POINTER_FLAG_UP,
            // Out of range: an update without INRANGE.
            (PenAction::Cancel | PenAction::Leave, false) => POINTER_FLAG_UPDATE,
        };
        let in_contact = flags.0 & POINTER_FLAG_INCONTACT.0 != 0;
        let mut pen_flags = PEN_FLAG_NONE;
        if barrel {
            pen_flags |= PEN_FLAG_BARREL;
        }
        if eraser {
            pen_flags |= PEN_FLAG_INVERTED;
            if in_contact {
                pen_flags |= PEN_FLAG_ERASER;
            }
        }
        let info = POINTER_PEN_INFO {
            pointerInfo: POINTER_INFO {
                pointerType: PT_PEN,
                pointerFlags: flags | POINTER_FLAG_PRIMARY,
                ptPixelLocation: at,
                ..Default::default()
            },
            penFlags: pen_flags,
            penMask: PEN_MASK_PRESSURE | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
            pressure: if in_contact { (pressure.clamp(0.0, 1.0) * 1024.0).round().max(1.0) as u32 } else { 0 },
            rotation: 0,
            tiltX: tilt_x.round().clamp(-90.0, 90.0) as i32,
            tiltY: tilt_y.round().clamp(-90.0, 90.0) as i32,
        };
        self.pen_contact = in_contact.then_some(PenContact { info });
        let Some(h) = self.pen.handle() else { return Ok(()) };
        let r = unsafe { InjectSyntheticPointerInput(h, &[pen_type_info(info)]) };
        if let Err(e) = &r {
            self.failures += 1;
            if self.failures <= 20 || self.failures.is_multiple_of(100) {
                tracing::warn!(error = %e, ?action, flags = flags.0, "pen frame rejected");
            }
            self.pen_contact = None;
        }
        r.context("pen injection failed")
    }

    /// No input for a moment: refresh held contacts as a real digitizer
    /// would, and release them if the tablet went silent.
    pub fn idle(&mut self) {
        let stale = self.last_event.is_none_or(|t| t.elapsed() > STALE_CONTACTS);
        if stale {
            self.release_all();
            return;
        }
        let infos: Vec<_> = self
            .contacts
            .iter()
            .map(|k| {
                let mut flags = POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;
                if k.primary {
                    flags |= POINTER_FLAG_PRIMARY;
                }
                touch_info(k.slot, flags, k.at)
            })
            .collect();
        let _ = self.send_touch(&infos);
        if let (Some(p), Some(h)) = (&self.pen_contact, self.pen.handle()) {
            let mut info = p.info;
            info.pointerInfo.pointerFlags = POINTER_FLAG_UPDATE
                | POINTER_FLAG_INRANGE
                | POINTER_FLAG_INCONTACT
                | POINTER_FLAG_FIRSTBUTTON
                | POINTER_FLAG_PRIMARY;
            let _ = unsafe { InjectSyntheticPointerInput(h, &[pen_type_info(info)]) };
        }
    }

    /// Lifts every finger and the pen (session ended mid-gesture).
    pub fn release_all(&mut self) {
        let infos: Vec<_> =
            self.contacts.iter().map(|k| touch_info(k.slot, POINTER_FLAG_UP | POINTER_FLAG_CANCELED, k.at)).collect();
        let _ = self.send_touch(&infos);
        self.contacts.clear();
        if let (Some(p), Some(h)) = (self.pen_contact.take(), self.pen.handle()) {
            let mut info = p.info;
            info.pointerInfo.pointerFlags = POINTER_FLAG_UP | POINTER_FLAG_CANCELED | POINTER_FLAG_PRIMARY;
            let _ = unsafe { InjectSyntheticPointerInput(h, &[pen_type_info(info)]) };
        }
        self.mouse_contact = None;
        self.pen_mouse_down = false;
    }
}
