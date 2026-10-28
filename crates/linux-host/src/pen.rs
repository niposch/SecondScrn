//! The tablet's pen as a virtual Linux tablet (uinput): pressure, tilt,
//! hover, eraser and barrel button, which the desktop portal's input methods
//! can't express.
//!
//! A tablet's absolute range covers the whole desktop unless the compositor
//! maps it to one monitor. On KDE Plasma it is mapped to the virtual monitor
//! through KWin's input device interface (`outputName`); otherwise positions
//! are placed within the desktop's bounding box from `kscreen-doctor`.
//! Without access to `/dev/uinput` (packages install a udev rule granting it
//! to the logged-in user) the pen falls back to pointer input.

use evdev::uinput::VirtualDevice;
use evdev::{AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode, PropType, UinputAbsSetup};
use secondscrn_protocol::PenAction;

use crate::kscreen;

/// Resolution of the absolute axes.
const RANGE: i32 = 32767;
const PRESSURE_MAX: i32 = 4095;

pub struct Pen {
    device: VirtualDevice,
    sysname: Option<String>,
    /// Where `0..1` coordinates of the virtual monitor land in the device's
    /// range: (x, y, width, height) as fractions of the range.
    area: (f64, f64, f64, f64),
    in_range: bool,
    touching: bool,
    tool: Option<KeyCode>,
    barrel: bool,
}

/// A pen sample in the virtual monitor's `0..1` coordinates.
#[derive(Debug, Clone, Copy)]
pub struct PenSample {
    pub action: PenAction,
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub tilt_x: f32,
    pub tilt_y: f32,
    pub barrel: bool,
    pub eraser: bool,
}

impl Pen {
    pub fn new() -> std::io::Result<Pen> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for k in [KeyCode::BTN_TOOL_PEN, KeyCode::BTN_TOOL_RUBBER, KeyCode::BTN_TOUCH, KeyCode::BTN_STYLUS] {
            keys.insert(k);
        }
        let mut props = AttributeSet::<PropType>::new();
        props.insert(PropType::DIRECT);
        let abs = |code, min, max, res| UinputAbsSetup::new(code, AbsInfo::new(0, min, max, 0, 0, res));
        let mut device = VirtualDevice::builder()?
            .name("SecondScrn Pen")
            .with_keys(&keys)?
            .with_properties(&props)?
            .with_absolute_axis(&abs(AbsoluteAxisCode::ABS_X, 0, RANGE, 100))?
            .with_absolute_axis(&abs(AbsoluteAxisCode::ABS_Y, 0, RANGE, 100))?
            .with_absolute_axis(&abs(AbsoluteAxisCode::ABS_PRESSURE, 0, PRESSURE_MAX, 0))?
            .with_absolute_axis(&abs(AbsoluteAxisCode::ABS_TILT_X, -90, 90, 57))?
            .with_absolute_axis(&abs(AbsoluteAxisCode::ABS_TILT_Y, -90, 90, 57))?
            .build()?;
        let sysname = device.enumerate_dev_nodes_blocking().ok().and_then(|mut nodes| {
            nodes.next().and_then(|n| n.ok()).and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
        });
        tracing::info!(device = ?sysname, "virtual pen tablet created");
        Ok(Pen {
            device,
            sysname,
            area: (0.0, 0.0, 1.0, 1.0),
            in_range: false,
            touching: false,
            tool: None,
            barrel: false,
        })
    }

    /// Maps the pen to the virtual monitor `output` (as named by the
    /// compositor; `geometry` its place in the desktop when known).
    pub fn map_to(&mut self, output: Option<&str>, all: Option<&[kscreen::Output]>) {
        self.area = (0.0, 0.0, 1.0, 1.0);
        if let (Some(name), Some(sys)) = (output, &self.sysname)
            && kwin_map_output(sys, name)
        {
            tracing::info!(output = name, "pen mapped to the virtual monitor (KWin)");
            return;
        }
        // Unmapped: the range spans the desktop's bounding box.
        if let (Some(name), Some(all)) = (output, all)
            && let (Some(ws), Some(o)) = (kscreen::workspace(all), all.iter().find(|o| o.name == name))
        {
            self.area = ((o.x - ws.x) / ws.width, (o.y - ws.y) / ws.height, o.width / ws.width, o.height / ws.height);
            tracing::info!(output = name, area = ?self.area, "pen placed within the desktop's bounding box");
        }
    }

    pub fn send(&mut self, s: PenSample) -> std::io::Result<()> {
        let x = ((self.area.0 + s.x.clamp(0.0, 1.0) as f64 * self.area.2) * RANGE as f64).round() as i32;
        let y = ((self.area.1 + s.y.clamp(0.0, 1.0) as f64 * self.area.3) * RANGE as f64).round() as i32;
        let tool = if s.eraser { KeyCode::BTN_TOOL_RUBBER } else { KeyCode::BTN_TOOL_PEN };
        let key = |k: KeyCode, v: bool| InputEvent::new(EventType::KEY.0, k.0, v as i32);
        let abs = |a: AbsoluteAxisCode, v: i32| InputEvent::new(EventType::ABSOLUTE.0, a.0, v);
        let mut ev = Vec::with_capacity(10);
        let leaving = matches!(s.action, PenAction::Leave | PenAction::Cancel);
        // Switching between pen and eraser: the old tool leaves first.
        if let Some(old) = self.tool.filter(|t| *t != tool || leaving) {
            if self.touching {
                ev.push(key(KeyCode::BTN_TOUCH, false));
                ev.push(abs(AbsoluteAxisCode::ABS_PRESSURE, 0));
                self.touching = false;
            }
            ev.push(key(old, false));
            self.tool = None;
            self.in_range = false;
        }
        if leaving {
            if self.barrel {
                ev.push(key(KeyCode::BTN_STYLUS, false));
                self.barrel = false;
            }
            return if ev.is_empty() { Ok(()) } else { self.device.emit(&ev) };
        }
        ev.push(abs(AbsoluteAxisCode::ABS_X, x));
        ev.push(abs(AbsoluteAxisCode::ABS_Y, y));
        ev.push(abs(AbsoluteAxisCode::ABS_TILT_X, s.tilt_x.round().clamp(-90.0, 90.0) as i32));
        ev.push(abs(AbsoluteAxisCode::ABS_TILT_Y, s.tilt_y.round().clamp(-90.0, 90.0) as i32));
        if !self.in_range {
            ev.push(key(tool, true));
            self.tool = Some(tool);
            self.in_range = true;
        }
        if s.barrel != self.barrel {
            ev.push(key(KeyCode::BTN_STYLUS, s.barrel));
            self.barrel = s.barrel;
        }
        let touching = matches!(s.action, PenAction::Down | PenAction::Move);
        let pressure =
            if touching { (s.pressure.clamp(0.0, 1.0) * PRESSURE_MAX as f32).round().max(1.0) as i32 } else { 0 };
        ev.push(abs(AbsoluteAxisCode::ABS_PRESSURE, pressure));
        if touching != self.touching {
            ev.push(key(KeyCode::BTN_TOUCH, touching));
            self.touching = touching;
        }
        self.device.emit(&ev)
    }

    /// Lifts and removes the pen (session ended).
    pub fn release(&mut self) {
        let s = PenSample {
            action: PenAction::Leave,
            x: 0.0,
            y: 0.0,
            pressure: 0.0,
            tilt_x: 0.0,
            tilt_y: 0.0,
            barrel: false,
            eraser: false,
        };
        let _ = self.send(s);
    }
}

/// Sets KWin's output mapping of the input device `sysname` (`eventN`).
/// KWin registers new devices asynchronously, so this retries briefly.
fn kwin_map_output(sysname: &str, output: &str) -> bool {
    let Ok(conn) = zbus::blocking::Connection::session() else { return false };
    let path = format!("/org/kde/KWin/InputDevice/{sysname}");
    for _ in 0..20 {
        if let Ok(proxy) = zbus::blocking::Proxy::new(&conn, "org.kde.KWin", path.as_str(), "org.kde.KWin.InputDevice")
            && proxy.set_property("outputName", output).is_ok()
        {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    tracing::debug!(sysname, output, "KWin didn't accept the pen's output mapping");
    false
}
