//! The virtual monitor and remote input through the XDG desktop portal.
//!
//! One RemoteDesktop session with a ScreenCast "virtual" source: the
//! compositor (GNOME's Mutter, KDE's KWin) creates a monitor for the stream,
//! whose size follows what the PipeWire stream negotiates, and the same
//! session injects pointer, scroll and touch input relative to that stream.
//! The user is asked once; the permission is remembered with a restore token
//! (`PersistMode::ExplicitlyRevoked`) saved in the data directory.
//!
//! Portal calls are asynchronous D-Bus calls; they run on a worker thread
//! with its own runtime, so the synchronous platform interface can use them.

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;

use ashpd::desktop::PersistMode;
use ashpd::desktop::Session;
use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use tokio::sync::{mpsc, oneshot};

/// What the desktop offers.
#[derive(Debug, Clone, Default)]
pub struct PortalStatus {
    /// The ScreenCast portal offers virtual monitors.
    pub virtual_monitors: bool,
    /// The RemoteDesktop portal offers touch input.
    pub touch: bool,
    /// Why not, or what was found.
    pub detail: String,
}

/// The virtual monitor of an open session.
#[derive(Debug, Clone)]
pub struct VirtualMonitor {
    pub node_id: u32,
    /// The PipeWire connection for the stream (dup it for each consumer).
    pub fd: Arc<OwnedFd>,
    /// Stream size in the compositor's coordinates, if the portal says.
    pub size: Option<(i32, i32)>,
    /// Session generation: changes when a new session (monitor) is opened.
    pub generation: u64,
}

/// Input relative to the virtual monitor (stream coordinates).
#[derive(Debug, Clone, Copy)]
pub enum InputCmd {
    Motion {
        x: f64,
        y: f64,
    },
    /// Linux input event code (BTN_LEFT, …).
    Button {
        button: i32,
        pressed: bool,
    },
    /// Wheel steps; positive is down / right.
    Scroll {
        vertical: bool,
        steps: i32,
    },
    TouchDown {
        slot: u32,
        x: f64,
        y: f64,
    },
    TouchMotion {
        slot: u32,
        x: f64,
        y: f64,
    },
    TouchUp {
        slot: u32,
    },
}

enum Cmd {
    Status(oneshot::Sender<PortalStatus>),
    Open(oneshot::Sender<Result<VirtualMonitor, String>>),
    Close,
    Input(InputCmd),
}

#[derive(Clone)]
pub struct Portal {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl Portal {
    pub fn start(data_dir: PathBuf) -> Portal {
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("secondscrn-portal".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("portal runtime");
                rt.block_on(worker(rx, data_dir.join("portal-restore-token")));
            })
            .expect("spawn portal thread");
        Portal { tx }
    }

    pub fn status(&self) -> PortalStatus {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Cmd::Status(reply)).is_err() {
            return PortalStatus { detail: "the portal worker stopped".into(), ..Default::default() };
        }
        rx.blocking_recv().unwrap_or_default()
    }

    /// The virtual monitor, opening a session (and asking the user, the
    /// first time) if none is open.
    pub fn open(&self) -> Result<VirtualMonitor, String> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Cmd::Open(reply)).map_err(|_| "the portal worker stopped".to_string())?;
        rx.blocking_recv().map_err(|_| "the portal worker stopped".to_string())?
    }

    /// Ends the session: the virtual monitor disappears.
    pub fn close(&self) {
        let _ = self.tx.send(Cmd::Close);
    }

    pub fn input(&self, cmd: InputCmd) {
        let _ = self.tx.send(Cmd::Input(cmd));
    }
}

struct Active {
    session: Arc<Session<RemoteDesktop>>,
    monitor: VirtualMonitor,
    watcher: tokio::task::JoinHandle<()>,
}

async fn worker(mut rx: mpsc::UnboundedReceiver<Cmd>, token_path: PathBuf) {
    let mut active: Option<Active> = None;
    let mut generation = 0u64;
    let (closed_tx, mut closed_rx) = mpsc::unbounded_channel::<u64>();
    let mut remote: Option<RemoteDesktop> = None;
    let mut rejected = 0u32;
    loop {
        let cmd = tokio::select! {
            cmd = rx.recv() => match cmd { Some(c) => c, None => break },
            Some(g) = closed_rx.recv() => {
                if active.as_ref().is_some_and(|a| a.monitor.generation == g) {
                    tracing::info!("the desktop closed the virtual monitor session");
                    active = None;
                }
                continue;
            }
        };
        match cmd {
            Cmd::Status(reply) => {
                let _ = reply.send(status().await);
            }
            Cmd::Open(reply) => {
                if let Some(a) = &active {
                    let _ = reply.send(Ok(a.monitor.clone()));
                    continue;
                }
                generation += 1;
                match open(&token_path, generation).await {
                    Ok((rd, session, monitor)) => {
                        // Notice when the desktop (or the user) ends it.
                        let session = Arc::new(session);
                        let (watched, tx, g) = (session.clone(), closed_tx.clone(), generation);
                        let watcher = tokio::spawn(async move {
                            use futures_util::StreamExt;
                            if let Ok(mut closed) = watched.receive_closed().await
                                && closed.next().await.is_some()
                            {
                                let _ = tx.send(g);
                            }
                        });
                        let _ = reply.send(Ok(monitor.clone()));
                        remote = Some(rd);
                        active = Some(Active { session, monitor, watcher });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot open the virtual monitor");
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Cmd::Close => {
                if let Some(a) = active.take() {
                    a.watcher.abort();
                    let _ = a.session.close().await;
                    tracing::info!("virtual monitor session closed");
                }
            }
            Cmd::Input(input) => {
                if let (Some(a), Some(rd)) = (&active, &remote)
                    && let Err(e) = inject(rd, a, input).await
                {
                    rejected += 1;
                    if rejected <= 5 {
                        tracing::warn!(error = %e, ?input, "input rejected by the portal");
                    }
                }
            }
        }
    }
    if let Some(a) = active {
        a.watcher.abort();
        let _ = a.session.close().await;
    }
}

async fn status() -> PortalStatus {
    let screencast = match Screencast::new().await {
        Ok(s) => s,
        Err(e) => {
            return PortalStatus {
                detail: format!("The desktop portal's screen cast service is not available ({e})."),
                ..Default::default()
            };
        }
    };
    let sources = screencast.available_source_types().await.unwrap_or_default();
    let touch = match RemoteDesktop::new().await {
        Ok(rd) => rd.available_device_types().await.unwrap_or_default().contains(DeviceType::Touchscreen),
        Err(_) => false,
    };
    let virtual_monitors = sources.contains(SourceType::Virtual);
    let detail = if virtual_monitors {
        format!("Virtual monitors through the desktop portal (screen cast version {})", screencast.version())
    } else {
        "This desktop doesn't offer virtual monitors through its portal. GNOME or KDE Plasma (6) on Wayland is \
         needed."
            .into()
    };
    PortalStatus { virtual_monitors, touch, detail }
}

async fn open(
    token_path: &PathBuf,
    generation: u64,
) -> Result<(RemoteDesktop, Session<RemoteDesktop>, VirtualMonitor), String> {
    let err = |what: &str, e: ashpd::Error| format!("{what}: {e}");
    let rd = RemoteDesktop::new().await.map_err(|e| err("remote desktop portal", e))?;
    let screencast = Screencast::new().await.map_err(|e| err("screen cast portal", e))?;
    let session = rd.create_session(Default::default()).await.map_err(|e| err("create session", e))?;
    let available = rd.available_device_types().await.unwrap_or_default();
    let mut devices = DeviceType::Pointer | DeviceType::Keyboard;
    if available.contains(DeviceType::Touchscreen) {
        devices |= DeviceType::Touchscreen;
    }
    let token = std::fs::read_to_string(token_path).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    rd.select_devices(
        &session,
        SelectDevicesOptions::default()
            .set_devices(devices)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(token.as_deref()),
    )
    .await
    .and_then(|r| r.response())
    .map_err(|e| err("select input devices", e))?;
    let cursor = match screencast.available_cursor_modes().await {
        Ok(m) if m.contains(CursorMode::Embedded) => CursorMode::Embedded,
        _ => CursorMode::Hidden,
    };
    screencast
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(cursor)
                .set_sources(Some(SourceType::Virtual.into()))
                .set_multiple(false),
        )
        .await
        .and_then(|r| r.response())
        .map_err(|e| err("select the virtual monitor", e))?;
    let started = rd
        .start(&session, None, Default::default())
        .await
        .and_then(|r| r.response())
        .map_err(|e| err("start (permission denied?)", e))?;
    if let Some(t) = started.restore_token() {
        if let Some(dir) = token_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(token_path, t);
    }
    let stream = started.streams().first().ok_or("the desktop started no stream")?;
    let fd = screencast
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(|e| err("open the PipeWire connection", e))?;
    let monitor =
        VirtualMonitor { node_id: stream.pipe_wire_node_id(), fd: Arc::new(fd), size: stream.size(), generation };
    tracing::info!(node = monitor.node_id, size = ?monitor.size, ?devices, "virtual monitor session started");
    Ok((rd, session, monitor))
}

async fn inject(rd: &RemoteDesktop, a: &Active, input: InputCmd) -> Result<(), ashpd::Error> {
    let (s, node) = (&a.session, a.monitor.node_id);
    match input {
        InputCmd::Motion { x, y } => rd.notify_pointer_motion_absolute(s, node, x, y, Default::default()).await,
        InputCmd::Button { button, pressed } => {
            let state = if pressed { KeyState::Pressed } else { KeyState::Released };
            rd.notify_pointer_button(s, button, state, Default::default()).await
        }
        InputCmd::Scroll { vertical, steps } => {
            let axis = if vertical { Axis::Vertical } else { Axis::Horizontal };
            rd.notify_pointer_axis_discrete(s, axis, steps, Default::default()).await
        }
        InputCmd::TouchDown { slot, x, y } => rd.notify_touch_down(s, node, slot, x, y, Default::default()).await,
        InputCmd::TouchMotion { slot, x, y } => rd.notify_touch_motion(s, node, slot, x, y, Default::default()).await,
        InputCmd::TouchUp { slot } => rd.notify_touch_up(s, slot, Default::default()).await,
    }
}
