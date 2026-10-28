//! System tray icon with quick controls (StatusNotifierItem: KDE Plasma,
//! GNOME with the AppIndicator extension).
//!
//! The tray runs on its own thread; its clicks come back to the UI thread
//! as [`Action`]s.

use std::rc::Rc;

use ksni::blocking::TrayMethods;
use ksni::menu::{RadioGroup, RadioItem, StandardItem, SubMenu};
use secondscrn_session::snapshot::{HostSnapshot, SessionPhase};
use tokio::sync::mpsc;

use super::Ctx;
use super::widgets::{self, PRESETS};

enum Action {
    Open,
    Preset(usize),
    Resolution(usize),
    FrameRate(usize),
    Disconnect,
    Quit,
}

#[derive(Default, Clone, PartialEq)]
struct State {
    status: String,
    connected: bool,
    streaming: bool,
    preset: usize,
    resolutions: Vec<String>,
    resolution: Option<usize>,
    frame_rates: Vec<String>,
    frame_rate: Option<usize>,
}

struct Item {
    state: State,
    tx: mpsc::UnboundedSender<Action>,
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        "secondscrn".into()
    }
    fn title(&self) -> String {
        "SecondScrn".into()
    }
    fn icon_name(&self) -> String {
        super::APP_ID.into()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: "SecondScrn".into(), description: self.state.status.clone(), ..Default::default() }
    }
    fn status(&self) -> ksni::Status {
        if self.state.connected { ksni::Status::Active } else { ksni::Status::Passive }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(Action::Open);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let s = &self.state;
        let radio = |labels: &[String], selected: Option<usize>, enabled: bool, make: fn(usize) -> Action| {
            RadioGroup {
                selected: selected.unwrap_or(usize::MAX),
                select: Box::new(move |t: &mut Self, i| {
                    let _ = t.tx.send(make(i));
                }),
                options: labels.iter().map(|l| RadioItem { label: l.clone(), enabled, ..Default::default() }).collect(),
            }
            .into()
        };
        let presets: Vec<String> = PRESETS.iter().map(|p| p.to_string()).collect();
        let mut items: Vec<ksni::MenuItem<Self>> = vec![
            StandardItem { label: s.status.clone(), enabled: false, ..Default::default() }.into(),
            ksni::MenuItem::Separator,
        ];
        if s.connected {
            items.push(
                SubMenu {
                    label: "Preset".into(),
                    submenu: vec![radio(&presets, Some(s.preset), true, Action::Preset)],
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                SubMenu {
                    label: "Resolution".into(),
                    enabled: s.streaming && !s.resolutions.is_empty(),
                    submenu: vec![radio(&s.resolutions, s.resolution, true, Action::Resolution)],
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                SubMenu {
                    label: "Frame rate".into(),
                    enabled: s.streaming && !s.frame_rates.is_empty(),
                    submenu: vec![radio(&s.frame_rates, s.frame_rate, true, Action::FrameRate)],
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: "Disconnect".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.send(Action::Disconnect);
                    }),
                    ..Default::default()
                }
                .into(),
            );
            items.push(ksni::MenuItem::Separator);
        }
        items.push(
            StandardItem {
                label: "Open SecondScrn".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(Action::Open);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(Action::Quit);
                }),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

pub struct Tray {
    handle: ksni::blocking::Handle<Item>,
    last: std::cell::RefCell<State>,
}

impl Tray {
    /// `None` when the desktop has no tray.
    pub fn start(ctx: &Rc<Ctx>) -> Option<Tray> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = match (Item { state: State::default(), tx }).spawn() {
            Ok(h) => h,
            Err(e) => {
                tracing::info!(error = %e, "no system tray");
                return None;
            }
        };
        let weak = Rc::downgrade(ctx);
        gtk::glib::spawn_future_local(async move {
            while let Some(action) = rx.recv().await {
                let Some(c) = weak.upgrade() else { break };
                let session = c.snapshot().session;
                match action {
                    Action::Open => c.show_window(),
                    Action::Preset(i) => {
                        if let Some(p) = widgets::preset_at(i as u32) {
                            let _ = c.engine.set_session_preset(p);
                        }
                    }
                    Action::Resolution(i) => {
                        if let Some(r) = session.and_then(|s| s.resolutions.get(i).cloned()) {
                            let _ = c.engine.set_resolution(r.scale_percent);
                        }
                    }
                    Action::FrameRate(i) => {
                        if let Some(f) = session.and_then(|s| s.frame_rates.get(i).copied()) {
                            let _ = c.engine.set_frame_rate(f);
                        }
                    }
                    Action::Disconnect => c.engine.disconnect(),
                    Action::Quit => c.quit(),
                }
            }
        });
        Some(Tray { handle, last: Default::default() })
    }

    pub fn update(&self, s: &HostSnapshot) {
        let session = s.session.as_ref();
        let stream = session.and_then(|x| x.stream.as_ref());
        let state = State {
            status: match session {
                None if s.pairing.is_some() => "Pairing request".into(),
                None => "No tablet connected".into(),
                Some(x) => match (x.phase, stream) {
                    (SessionPhase::Streaming, Some(st)) => {
                        format!("{}: {} × {} at {} fps", x.device_name, st.width, st.height, st.max_fps)
                    }
                    (SessionPhase::Interrupted, _) => format!("{}: connection interrupted", x.device_name),
                    _ => format!("Connecting {}…", x.device_name),
                },
            },
            connected: session.is_some(),
            streaming: session.is_some_and(|x| x.phase == SessionPhase::Streaming),
            preset: widgets::preset_index(session.map(|x| x.preset).unwrap_or(s.settings.preset)) as usize,
            resolutions: session
                .map(|x| {
                    x.resolutions
                        .iter()
                        .map(|r| format!("{} × {} ({} %)", r.width, r.height, r.scale_percent))
                        .collect()
                })
                .unwrap_or_default(),
            resolution: session.and_then(|x| {
                x.resolutions.iter().position(|r| Some(r.scale_percent) == stream.map(|st| st.scale_percent))
            }),
            frame_rates: session
                .map(|x| x.frame_rates.iter().map(|f| format!("{f} fps")).collect())
                .unwrap_or_default(),
            frame_rate: session
                .and_then(|x| x.frame_rates.iter().position(|f| Some(*f) == stream.map(|st| st.max_fps))),
        };
        if *self.last.borrow() == state {
            return;
        }
        *self.last.borrow_mut() = state.clone();
        self.handle.update(move |t| t.state = state);
    }

    pub fn shutdown(&self) {
        self.handle.shutdown().wait();
    }
}
