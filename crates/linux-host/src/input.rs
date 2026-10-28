//! Tablet input → the desktop: mouse, scroll and touch through the portal
//! session ([`crate::portal`]), the pen as a virtual tablet ([`crate::pen`]).
//!
//! Coordinates arrive normalised to the virtual monitor; the portal wants
//! them in the stream's logical size. Without portal touch (older desktops)
//! the first finger drives the pointer; without `/dev/uinput` the pen does.

use std::collections::HashMap;

use secondscrn_protocol::{InputEvent, PenAction, PointerAction, PointerButton, TouchContact};

use crate::pen::{Pen, PenSample};
use crate::portal::{InputCmd, Portal};

const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;

/// Portal touch slots are small integers; tablets report arbitrary ids.
const MAX_SLOTS: u32 = 10;

#[derive(Default)]
pub struct Injector {
    portal: Option<Portal>,
    /// The stream's logical size (portal coordinates).
    size: (f64, f64),
    touch: bool,
    /// Tablet contact id → portal slot.
    slots: HashMap<u32, u32>,
    /// Finger driving the pointer when the portal has no touch.
    emulated: Option<u32>,
    pen: PenState,
    buttons: Vec<i32>,
    scroll: (f32, f32),
    last_seq: u64,
}

#[derive(Default)]
enum PenState {
    #[default]
    Untried,
    Ready(Box<Pen>),
    /// No uinput access: pointer emulation.
    Pointer {
        down: bool,
        barrel: bool,
    },
}

impl Injector {
    /// A new virtual monitor (or a new size): where input goes from now on.
    pub fn attach(&mut self, portal: Portal, size: (f64, f64), touch: bool, output: Option<&str>) {
        self.release_all();
        self.portal = Some(portal);
        self.size = size;
        self.touch = touch;
        self.last_seq = 0;
        if let PenState::Ready(pen) = &mut self.pen {
            pen.map_to(output, crate::kscreen::outputs().as_deref());
        } else if matches!(self.pen, PenState::Untried) {
            self.pen = match Pen::new() {
                Ok(mut pen) => {
                    pen.map_to(output, crate::kscreen::outputs().as_deref());
                    PenState::Ready(Box::new(pen))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "no virtual pen (no access to /dev/uinput); the pen moves the pointer");
                    PenState::Pointer { down: false, barrel: false }
                }
            };
        }
    }

    pub fn has_pen_tablet(&self) -> bool {
        matches!(self.pen, PenState::Ready(_))
    }

    fn send(&self, cmd: InputCmd) {
        if let Some(p) = &self.portal {
            p.input(cmd);
        }
    }

    fn pos(&self, x: f32, y: f32) -> (f64, f64) {
        let x = (x.clamp(0.0, 1.0) as f64 * self.size.0).min(self.size.0 - 1.0).max(0.0);
        let y = (y.clamp(0.0, 1.0) as f64 * self.size.1).min(self.size.1 - 1.0).max(0.0);
        (x, y)
    }

    fn motion(&self, x: f32, y: f32) {
        let (x, y) = self.pos(x, y);
        self.send(InputCmd::Motion { x, y });
    }

    fn button(&mut self, button: i32, pressed: bool) {
        let held = self.buttons.contains(&button);
        if held == pressed {
            return;
        }
        if pressed {
            self.buttons.push(button);
        } else {
            self.buttons.retain(|b| *b != button);
        }
        self.send(InputCmd::Button { button, pressed });
    }

    pub fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        if self.portal.is_none() {
            anyhow::bail!("no virtual monitor session");
        }
        // Out-of-order datagrams are dropped (as on Windows).
        let seq = event.seq();
        if seq != 0 && seq <= self.last_seq {
            return Ok(());
        }
        self.last_seq = seq;
        match event {
            InputEvent::Pointer { action, button, x, y, .. } => {
                let code = match button {
                    PointerButton::Primary => BTN_LEFT,
                    PointerButton::Secondary => BTN_RIGHT,
                    PointerButton::Middle => BTN_MIDDLE,
                };
                self.motion(*x, *y);
                match action {
                    PointerAction::Down => self.button(code, true),
                    PointerAction::Up | PointerAction::Cancel => self.button(code, false),
                    PointerAction::Move => {}
                }
            }
            InputEvent::Scroll { x, y, delta_x, delta_y, .. } => {
                self.motion(*x, *y);
                self.scroll.0 += delta_x;
                self.scroll.1 += delta_y;
                for (vertical, acc) in [(true, &mut self.scroll.1), (false, &mut self.scroll.0)] {
                    let steps = acc.trunc() as i32;
                    if steps != 0 {
                        *acc -= steps as f32;
                        if let Some(p) = &self.portal {
                            p.input(InputCmd::Scroll { vertical, steps });
                        }
                    }
                }
            }
            InputEvent::Touch { contacts, .. } => self.touch_frame(contacts),
            InputEvent::Pen { action, x, y, pressure, tilt_x, tilt_y, barrel, eraser, .. } => {
                let sample = PenSample {
                    action: *action,
                    x: *x,
                    y: *y,
                    pressure: *pressure,
                    tilt_x: *tilt_x,
                    tilt_y: *tilt_y,
                    barrel: *barrel,
                    eraser: *eraser,
                };
                self.pen(sample)?;
            }
        }
        Ok(())
    }

    fn pen(&mut self, s: PenSample) -> anyhow::Result<()> {
        let (was_down, was_barrel) = match &mut self.pen {
            PenState::Ready(pen) => return pen.send(s).map_err(|e| anyhow::anyhow!("virtual pen: {e}")),
            PenState::Untried => return Ok(()),
            PenState::Pointer { down, barrel } => {
                let was = (*down, *barrel);
                *down = matches!(s.action, PenAction::Down | PenAction::Move);
                // The button chosen at touch-down is held until lift.
                *barrel = *down && if was.0 { was.1 } else { s.barrel };
                was
            }
        };
        let touching = matches!(s.action, PenAction::Down | PenAction::Move);
        if !matches!(s.action, PenAction::Leave | PenAction::Cancel) {
            self.motion(s.x, s.y);
        }
        // Touching with the barrel button held is a right click.
        if touching && !was_down {
            self.button(if s.barrel { BTN_RIGHT } else { BTN_LEFT }, true);
        } else if !touching && was_down {
            self.button(if was_barrel { BTN_RIGHT } else { BTN_LEFT }, false);
        }
        Ok(())
    }

    fn touch_frame(&mut self, contacts: &[TouchContact]) {
        if !self.touch {
            return self.emulate_touch(contacts);
        }
        // Fingers missing from the frame are cancelled.
        let gone: Vec<u32> = self.slots.keys().filter(|id| !contacts.iter().any(|c| c.id == **id)).copied().collect();
        for id in gone {
            if let Some(slot) = self.slots.remove(&id) {
                self.send(InputCmd::TouchUp { slot });
            }
        }
        for c in contacts {
            let (x, y) = self.pos(c.x, c.y);
            match (c.action, self.slots.get(&c.id).copied()) {
                (PointerAction::Up | PointerAction::Cancel, Some(slot)) => {
                    self.slots.remove(&c.id);
                    self.send(InputCmd::TouchUp { slot });
                }
                (PointerAction::Up | PointerAction::Cancel, None) => {}
                (_, Some(slot)) => self.send(InputCmd::TouchMotion { slot, x, y }),
                (_, None) => {
                    let Some(slot) = (0..MAX_SLOTS).find(|s| !self.slots.values().any(|v| v == s)) else { continue };
                    self.slots.insert(c.id, slot);
                    self.send(InputCmd::TouchDown { slot, x, y });
                }
            }
        }
    }

    /// The first finger is the mouse: down → left button.
    fn emulate_touch(&mut self, contacts: &[TouchContact]) {
        let current = self.emulated.and_then(|id| contacts.iter().find(|c| c.id == id));
        match current {
            Some(c) => {
                self.motion(c.x, c.y);
                if matches!(c.action, PointerAction::Up | PointerAction::Cancel) {
                    self.button(BTN_LEFT, false);
                    self.emulated = None;
                }
            }
            None => {
                if self.emulated.take().is_some() {
                    self.button(BTN_LEFT, false);
                }
                if let Some(c) = contacts.iter().find(|c| c.action == PointerAction::Down) {
                    self.motion(c.x, c.y);
                    self.button(BTN_LEFT, true);
                    self.emulated = Some(c.id);
                }
            }
        }
    }

    /// The session ended: lift fingers, release buttons, remove the pen.
    pub fn release_all(&mut self) {
        let slots: Vec<u32> = self.slots.drain().map(|(_, s)| s).collect();
        for slot in slots {
            self.send(InputCmd::TouchUp { slot });
        }
        self.emulated = None;
        for b in std::mem::take(&mut self.buttons) {
            self.send(InputCmd::Button { button: b, pressed: false });
        }
        match &mut self.pen {
            PenState::Ready(pen) => pen.release(),
            PenState::Pointer { down, barrel } => {
                *down = false;
                *barrel = false;
            }
            PenState::Untried => {}
        }
        self.scroll = (0.0, 0.0);
    }
}
