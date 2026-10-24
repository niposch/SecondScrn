//! Injection of tablet input as mouse events.
//!
//! Coordinates arrive normalised to the streamed display and are mapped onto
//! that monitor's rectangle within the virtual desktop. Only authenticated
//! sessions can reach this code (see the session engine).
//!
//! Limitation: like any non-elevated process, input cannot reach windows of
//! elevated applications (UIPI).

use anyhow::{Context, Result};
use secondscrn_protocol::{InputEvent, PointerAction, PointerButton};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::capture;

const WHEEL_DELTA: f32 = 120.0;

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

/// Maps normalised display coordinates to absolute virtual-desktop units
/// (0..=65535) as required by `MOUSEEVENTF_VIRTUALDESK`.
fn absolute(display_device: &str, x: f32, y: f32) -> Result<(i32, i32)> {
    let m = capture::monitor_by_device(display_device).context("streamed display is not part of the desktop")?;
    let r = m.rect;
    let px = r.left as f32 + x.clamp(0.0, 1.0) * ((r.right - r.left - 1).max(0) as f32);
    let py = r.top as f32 + y.clamp(0.0, 1.0) * ((r.bottom - r.top - 1).max(0) as f32);
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let nx = ((px - vx as f32) * 65535.0 / (vw - 1).max(1) as f32).round() as i32;
    let ny = ((py - vy as f32) * 65535.0 / (vh - 1).max(1) as f32).round() as i32;
    Ok((nx, ny))
}

pub fn inject(display_device: &str, event: &InputEvent) -> Result<()> {
    let abs = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
    let inputs: Vec<INPUT> = match *event {
        InputEvent::Pointer { action, button, x, y, .. } => {
            let (nx, ny) = absolute(display_device, x, y)?;
            let (down, up) = match button {
                PointerButton::Primary => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
                PointerButton::Secondary => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
                PointerButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
            };
            match action {
                PointerAction::Move => vec![mouse(nx, ny, 0, abs)],
                PointerAction::Down => vec![mouse(nx, ny, 0, abs), mouse(nx, ny, 0, abs | down)],
                PointerAction::Up | PointerAction::Cancel => vec![mouse(nx, ny, 0, abs | up)],
            }
        }
        InputEvent::Scroll { x, y, delta_x, delta_y, .. } => {
            let (nx, ny) = absolute(display_device, x, y)?;
            let mut v = vec![mouse(nx, ny, 0, abs)];
            if delta_y != 0.0 {
                // Windows: positive wheel = away from the user (scroll up).
                v.push(mouse(0, 0, (-delta_y * WHEEL_DELTA) as i32, MOUSEEVENTF_WHEEL));
            }
            if delta_x != 0.0 {
                v.push(mouse(0, 0, (delta_x * WHEEL_DELTA) as i32, MOUSEEVENTF_HWHEEL));
            }
            v
        }
    };
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        anyhow::bail!("SendInput injected {sent} of {} events (blocked by a higher-integrity window?)", inputs.len());
    }
    Ok(())
}
