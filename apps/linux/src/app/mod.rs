//! Application shell: startup, the main window, the snapshot loop and the
//! pairing prompt.

mod diagnostics;
mod home;
mod settings;
mod system;
mod tablets;
mod tray;
mod widgets;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use secondscrn_linux_host::platform::{DisplaySource, LinuxHostPlatform};
use secondscrn_session::settings::{HostSettings, Theme};
use secondscrn_session::snapshot::HostSnapshot;
use secondscrn_session::{HostConfig, HostEngine};
use secondscrn_transport::InterfacePolicy;

pub const APP_ID: &str = "dev.secondscrn.SecondScrn";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared by all pages. Lives as long as the application.
pub struct Ctx {
    pub engine: Arc<HostEngine>,
    pub platform: Arc<LinuxHostPlatform>,
    pub data_dir: PathBuf,
    pub log_dir: PathBuf,
    pub app: adw::Application,
    pub window: adw::ApplicationWindow,
    pub stack: adw::ViewStack,
    toasts: adw::ToastOverlay,
    /// Set while pages apply a snapshot, so their change handlers don't
    /// write the values back.
    pub updating: Cell<bool>,
    listeners: RefCell<Vec<Listener>>,
    /// The open pairing request and its dialog (`None` once dismissed).
    pairing: RefCell<Option<(u64, Option<adw::AlertDialog>)>>,
    /// A newer release (checked at startup or from Settings).
    pub update: RefCell<Option<String>>,
    pub tray: RefCell<Option<tray::Tray>>,
    pub quitting: Cell<bool>,
    /// Settings that apply from the next connection, as they were when the
    /// current one started.
    session_start: RefCell<Option<(u64, SessionKeys)>>,
}

type Listener = Box<dyn Fn(&HostSnapshot)>;

type SessionKeys =
    (Option<secondscrn_protocol::Codec>, u32, Option<u32>, Option<secondscrn_session::settings::EncoderGpu>);

fn session_keys(s: &HostSettings) -> SessionKeys {
    (s.preferred_codec, s.max_bitrate_mbps, s.fps_limit, s.encoder_gpu_choice)
}

impl Ctx {
    pub fn snapshot(&self) -> HostSnapshot {
        self.engine.snapshot()
    }

    /// Changes settings: the engine saves them and publishes a snapshot.
    pub fn change(&self, f: impl FnOnce(&mut HostSettings)) {
        if self.updating.get() {
            return;
        }
        let mut s = self.engine.snapshot().settings;
        let before = s.clone();
        f(&mut s);
        if s != before {
            self.engine.update_settings(s);
        }
    }

    /// Runs `f` with every new snapshot (and once now).
    pub fn on_snapshot(&self, f: impl Fn(&HostSnapshot) + 'static) {
        self.updating.set(true);
        f(&self.engine.snapshot());
        self.updating.set(false);
        self.listeners.borrow_mut().push(Box::new(f));
    }

    fn apply(&self, s: &HostSnapshot) {
        self.updating.set(true);
        for f in self.listeners.borrow().iter() {
            f(s);
        }
        self.updating.set(false);
    }

    pub fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    /// A setting that applies to new connections changed during this one.
    pub fn needs_reconnect(&self, s: &HostSnapshot) -> bool {
        let mut start = self.session_start.borrow_mut();
        match &s.session {
            None => {
                *start = None;
                false
            }
            Some(sess) => {
                let keys = session_keys(&s.settings);
                match &*start {
                    Some((since, k)) if *since == sess.connected_since => *k != keys,
                    _ => {
                        *start = Some((sess.connected_since, keys));
                        false
                    }
                }
            }
        }
    }

    /// Shows the pairing dialog again (dismissed, or a mistyped code).
    pub fn reopen_pairing(self: &Rc<Self>) {
        *self.pairing.borrow_mut() = None;
        pairing_prompt(self, &self.snapshot());
    }

    pub fn show_window(&self) {
        self.window.present();
    }

    pub fn quit(&self) {
        if self.quitting.replace(true) {
            return;
        }
        tracing::info!("quitting");
        self.engine.shutdown();
        self.platform.release_displays();
        if let Some(t) = self.tray.borrow_mut().take() {
            t.shutdown();
        }
        self.app.quit();
    }
}

pub fn run() -> std::process::ExitCode {
    let data_dir = secondscrn_linux_host::default_data_dir();
    let settings: HostSettings = secondscrn_session::settings::load(&data_dir.join("settings.json"));
    let log_dir = data_dir.join("logs");
    let logs = system::init_logging(&log_dir, settings.verbose_logging);
    tracing::info!(version = VERSION, "SecondScrn starting");

    let app = adw::Application::builder().application_id(APP_ID).build();
    let autostarted = std::env::args().any(|a| a == "--autostart");
    let ctx: Rc<RefCell<Option<Rc<Ctx>>>> = Rc::default();
    let hold: Rc<RefCell<Option<gtk::gio::ApplicationHoldGuard>>> = Rc::default();
    {
        let (ctx, hold) = (ctx.clone(), hold.clone());
        app.connect_activate(move |app| {
            if let Some(c) = ctx.borrow().as_ref() {
                c.show_window();
                return;
            }
            if hold.borrow().is_some() {
                return; // still starting
            }
            *hold.borrow_mut() = Some(app.hold());
            start(app.clone(), data_dir.clone(), log_dir.clone(), logs.clone(), autostarted, ctx.clone());
        });
    }
    // `--autostart` is ours, not GTK's.
    let args: Vec<String> = std::env::args().filter(|a| a != "--autostart").collect();
    let code = app.run_with_args(&args);
    drop(hold);
    if code == gtk::glib::ExitCode::SUCCESS { std::process::ExitCode::SUCCESS } else { std::process::ExitCode::FAILURE }
}

/// Probes encoders and starts the engine off the main thread (a few
/// seconds), then builds the window.
fn start(
    app: adw::Application,
    data_dir: PathBuf,
    log_dir: PathBuf,
    logs: secondscrn_diagnostics::LogBuffer,
    autostarted: bool,
    slot: Rc<RefCell<Option<Rc<Ctx>>>>,
) {
    let splash = adw::ApplicationWindow::builder()
        .application(&app)
        .title("SecondScrn")
        .default_width(420)
        .default_height(300)
        .content(
            &adw::StatusPage::builder()
                .icon_name("video-display-symbolic")
                .title("SecondScrn")
                .description("Checking video encoders…")
                .build(),
        )
        .build();
    if !autostarted {
        splash.present();
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let dir = data_dir.clone();
    std::thread::spawn(move || {
        let settings: HostSettings = secondscrn_session::settings::load(&dir.join("settings.json"));
        // Development aid: a moving test pattern instead of a virtual monitor.
        let source = if std::env::var_os("SECONDSCRN_TEST_PATTERN").is_some() {
            DisplaySource::TestPattern
        } else {
            DisplaySource::Virtual
        };
        let platform = Arc::new(LinuxHostPlatform::new(source, settings.encoder_gpu_choice, dir.clone()));
        let engine = HostEngine::start(
            HostConfig {
                data_dir: dir.clone(),
                default_host_name: secondscrn_linux_host::host_name(),
                listen_addr: [0, 0, 0, 0].into(),
                interfaces: InterfacePolicy::default(),
                enable_mdns: true,
                protector: Arc::new(secondscrn_linux_host::secrets::FileKeyProtector::new(&dir)),
                logs: Some(logs),
                app_version: VERSION.into(),
            },
            platform.clone(),
        );
        // Desktop capabilities are read once, here rather than on the UI thread.
        let _ = platform.portal_status();
        let _ = tx.send(engine.map(|e| (Arc::new(e), platform)));
    });
    gtk::glib::spawn_future_local(async move {
        let result = rx.await;
        splash.destroy();
        match result {
            Ok(Ok((engine, platform))) => {
                let ctx = build(app.clone(), engine, platform, data_dir, log_dir);
                *slot.borrow_mut() = Some(ctx.clone());
                let settings = ctx.snapshot().settings;
                let hidden = autostarted && settings.start_minimized && ctx.tray.borrow().is_some();
                if !hidden {
                    ctx.show_window();
                }
                if settings.check_for_updates {
                    system::check_for_updates(ctx.clone(), false);
                }
            }
            Ok(Err(e)) => fatal(&app, &format!("SecondScrn couldn't start: {e}")),
            Err(_) => fatal(&app, "SecondScrn couldn't start."),
        }
    });
}

fn fatal(app: &adw::Application, text: &str) {
    tracing::error!("{text}");
    let w = adw::ApplicationWindow::builder().application(app).title("SecondScrn").default_width(420).build();
    let page =
        adw::StatusPage::builder().icon_name("dialog-error-symbolic").title("Not started").description(text).build();
    w.set_content(Some(&page));
    w.present();
    let app = app.clone();
    w.connect_close_request(move |_| {
        app.quit();
        gtk::glib::Propagation::Proceed
    });
}

fn build(
    app: adw::Application,
    engine: Arc<HostEngine>,
    platform: Arc<LinuxHostPlatform>,
    data_dir: PathBuf,
    log_dir: PathBuf,
) -> Rc<Ctx> {
    let window = adw::ApplicationWindow::builder()
        .application(&app)
        .title("SecondScrn")
        .icon_name(APP_ID)
        .default_width(760)
        .default_height(780)
        .width_request(360)
        .height_request(400)
        .build();
    let stack = adw::ViewStack::new();
    let toasts = adw::ToastOverlay::new();
    let ctx = Rc::new(Ctx {
        engine,
        platform,
        data_dir,
        log_dir,
        app: app.clone(),
        window: window.clone(),
        stack: stack.clone(),
        toasts: toasts.clone(),
        updating: Cell::new(false),
        listeners: RefCell::default(),
        pairing: RefCell::default(),
        update: RefCell::default(),
        tray: RefCell::default(),
        quitting: Cell::new(false),
        session_start: RefCell::default(),
    });

    stack.add_titled_with_icon(&home::page(&ctx), Some("home"), "Home", "user-home-symbolic");
    stack.add_titled_with_icon(&tablets::page(&ctx), Some("tablets"), "Tablets", "tablet-symbolic");
    stack.add_titled_with_icon(&settings::page(&ctx), Some("settings"), "Settings", "emblem-system-symbolic");
    stack.add_titled_with_icon(
        &diagnostics::page(&ctx),
        Some("diagnostics"),
        "Diagnostics",
        "utilities-system-monitor-symbolic",
    );

    let header = adw::HeaderBar::new();
    let switcher = adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build();
    header.set_title_widget(Some(&switcher));
    let menu = gtk::gio::Menu::new();
    menu.append(Some("Quit SecondScrn"), Some("app.quit"));
    header.pack_end(&gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).build());
    let bottom = adw::ViewSwitcherBar::builder().stack(&stack).build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    toasts.set_child(Some(&stack));
    view.set_content(Some(&toasts));
    view.add_bottom_bar(&bottom);
    window.set_content(Some(&view));
    // Narrow windows: the switcher moves to the bottom.
    let bp = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        550.0,
        adw::LengthUnit::Sp,
    ));
    bp.add_setter(&switcher, "visible", Some(&false.to_value()));
    bp.add_setter(&bottom, "reveal", Some(&true.to_value()));
    window.add_breakpoint(bp);

    let quit = gtk::gio::SimpleAction::new("quit", None);
    {
        let ctx = Rc::downgrade(&ctx);
        quit.connect_activate(move |_, _| {
            if let Some(c) = ctx.upgrade() {
                c.quit();
            }
        });
    }
    app.add_action(&quit);
    app.set_accels_for_action("app.quit", &["<Control>q"]);

    *ctx.tray.borrow_mut() = tray::Tray::start(&ctx);
    {
        let ctx = Rc::downgrade(&ctx);
        window.connect_close_request(move |w| {
            let Some(c) = ctx.upgrade() else { return gtk::glib::Propagation::Proceed };
            if c.snapshot().settings.close_to_tray && c.tray.borrow().is_some() && !c.quitting.get() {
                w.set_visible(false);
                return gtk::glib::Propagation::Stop;
            }
            c.quit();
            gtk::glib::Propagation::Proceed
        });
    }

    // Theme and pairing prompt follow the snapshot.
    {
        let c = Rc::downgrade(&ctx);
        ctx.on_snapshot(move |s| {
            let Some(c) = c.upgrade() else { return };
            adw::StyleManager::default().set_color_scheme(match s.settings.theme {
                Theme::System => adw::ColorScheme::Default,
                Theme::Light => adw::ColorScheme::ForceLight,
                Theme::Dark => adw::ColorScheme::ForceDark,
            });
            pairing_prompt(&c, s);
            if let Some(t) = c.tray.borrow().as_ref() {
                t.update(s);
            }
        });
    }

    let mut rx = ctx.engine.subscribe();
    let weak = Rc::downgrade(&ctx);
    gtk::glib::spawn_future_local(async move {
        while rx.changed().await.is_ok() {
            let snap = rx.borrow_and_update().clone();
            let Some(c) = weak.upgrade() else { break };
            c.apply(&snap);
        }
    });
    ctx
}

/// Asks for the code shown on the tablet while a pairing request is open.
fn pairing_prompt(ctx: &Rc<Ctx>, s: &HostSnapshot) {
    let current = ctx.pairing.borrow().as_ref().map(|(id, _)| *id);
    match (&s.pairing, current) {
        (Some(p), Some(id)) if p.id == id => {}
        (None, None) => {}
        (prompt, _) => {
            if let Some((_, Some(dialog))) = ctx.pairing.borrow_mut().take() {
                dialog.force_close();
            }
            let Some(p) = prompt else { return };
            let dialog = adw::AlertDialog::new(
                Some(&format!("Pair {}?", p.device_name)),
                Some(&format!(
                    "Enter the 6-digit code shown on the tablet ({} · {}, ID {}). Only pair tablets you own.",
                    p.device_model, p.remote_address, p.fingerprint_short
                )),
            );
            let entry = gtk::Entry::builder()
                .placeholder_text("123456")
                .input_purpose(gtk::InputPurpose::Digits)
                .max_length(7)
                .activates_default(true)
                .css_classes(["numeric"])
                .build();
            dialog.set_extra_child(Some(&entry));
            dialog.add_responses(&[("reject", "Reject"), ("pair", "Pair")]);
            dialog.set_response_appearance("pair", adw::ResponseAppearance::Suggested);
            dialog.set_response_appearance("reject", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("pair"));
            dialog.set_close_response("close");
            let (id, weak) = (p.id, Rc::downgrade(ctx));
            dialog.connect_response(None, move |_, response| {
                let Some(c) = weak.upgrade() else { return };
                // Dismissed: not asked again for this request.
                *c.pairing.borrow_mut() = Some((id, None));
                let result = match response {
                    "pair" => c.engine.submit_pairing_pin(id, entry.text().trim()),
                    "reject" => c.engine.reject_pairing(id),
                    _ => Ok(()),
                };
                if let Err(e) = result {
                    c.toast(&e.to_string());
                    // A mistyped code: ask again.
                    if response == "pair" {
                        c.reopen_pairing();
                    }
                }
            });
            ctx.stack.set_visible_child_name("home");
            ctx.show_window();
            dialog.present(Some(&ctx.window));
            *ctx.pairing.borrow_mut() = Some((p.id, Some(dialog)));
        }
    }
}
