//! Diagnostics: connection details, desktop support, encoders, logs.

use std::rc::Rc;

use adw::prelude::*;
use secondscrn_session::snapshot::HostSnapshot;

use super::widgets;
use super::{Ctx, system};

pub fn page(ctx: &Rc<Ctx>) -> gtk::Widget {
    let page = adw::PreferencesPage::new();

    let connection = widgets::group("Connection", None);
    let details = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .selectable(true)
        .css_classes(["monospace", "numeric"])
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let frame = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    frame.append(&details);
    connection.add(&frame);
    let (net_row, _) = widgets::value_row("Network");
    net_row.set_subtitle_lines(3);
    connection.add(&net_row);

    let desktop = widgets::group("Desktop", None);
    let monitor = adw::ActionRow::builder().title("Virtual monitor").subtitle_lines(3).build();
    let input = adw::ActionRow::builder().title("Touch and pen").subtitle_lines(4).build();
    desktop.add(&monitor);
    desktop.add(&input);

    let encoders = widgets::group(
        "Video encoders",
        Some(
            "Only encoders that were verified to work are listed. Software encoding is used only when no hardware encoder works.",
        ),
    );
    let encoder_list = adw::ActionRow::builder().title("Encoders").subtitle_lines(6).build();
    let recheck = gtk::Button::builder().label("Check again").valign(gtk::Align::Center).build();
    encoder_list.add_suffix(&recheck);
    {
        let weak = Rc::downgrade(ctx);
        recheck.connect_clicked(move |_| {
            let Some(c) = weak.upgrade() else { return };
            let (platform, engine) = (c.platform.clone(), c.engine.clone());
            std::thread::spawn(move || {
                platform.reprobe();
                engine.refresh_platform();
            });
            c.toast("Checking encoders…");
        });
    }
    encoders.add(&encoder_list);

    let logs =
        widgets::group("Logs", Some("Exports contain statistics and logs with keys, codes and identities removed."));
    let row = adw::ActionRow::builder().title("Diagnostics").subtitle(ctx.log_dir.display().to_string()).build();
    let open = gtk::Button::builder().label("Open log folder").valign(gtk::Align::Center).build();
    let export = gtk::Button::builder().label("Export diagnostics…").valign(gtk::Align::Center).build();
    row.add_suffix(&open);
    row.add_suffix(&export);
    {
        let weak = Rc::downgrade(ctx);
        open.connect_clicked(move |_| {
            if let Some(c) = weak.upgrade() {
                let _ = std::fs::create_dir_all(&c.log_dir);
                let uri = gtk::gio::File::for_path(&c.log_dir).uri();
                widgets::open_uri(&c, &uri);
            }
        });
        let weak = Rc::downgrade(ctx);
        export.connect_clicked(move |_| {
            let Some(c) = weak.upgrade() else { return };
            let dialog = gtk::FileDialog::builder()
                .title("Export diagnostics")
                .initial_name("secondscrn-diagnostics.json")
                .build();
            let weak = Rc::downgrade(&c);
            dialog.save(Some(&c.window), gtk::gio::Cancellable::NONE, move |r| {
                let (Some(c), Ok(file)) = (weak.upgrade(), r) else { return };
                let Some(path) = file.path() else { return };
                match std::fs::write(&path, c.engine.export_diagnostics()) {
                    Ok(()) => c.toast("Diagnostics exported"),
                    Err(e) => c.toast(&format!("Couldn't save: {e}")),
                }
            });
        });
    }
    logs.add(&row);

    for g in [&connection, &desktop, &encoders, &logs] {
        page.add(g);
    }

    let weak = Rc::downgrade(ctx);
    ctx.on_snapshot(move |s: &HostSnapshot| {
        let Some(c) = weak.upgrade() else { return };
        details.set_text(&system::session_details(s));
        net_row.set_subtitle(&if s.listening {
            format!(
                "Listening on port {} · {} · addresses {}",
                s.port,
                if s.settings.discoverable { "discoverable on the local network" } else { "not discoverable" },
                s.addresses.join(", ")
            )
        } else {
            format!("Not listening: {}", s.last_error.as_deref().unwrap_or("unknown error"))
        });
        let portal = c.platform.portal_status();
        monitor.set_subtitle(&portal.detail);
        input.set_subtitle(&format!(
            "Touch: {}. Pen: {}.",
            if portal.touch {
                "real touch through the desktop portal"
            } else {
                "the first finger moves the pointer (the desktop's portal has no touch)"
            },
            if c.platform.has_pen_tablet() {
                "virtual tablet with pressure and tilt"
            } else if system::uinput_accessible() {
                "virtual tablet, created on the first connection"
            } else {
                "moves the pointer (no access to /dev/uinput: the package's udev rule grants it after logging in again)"
            }
        ));
        encoder_list.set_subtitle(&if s.encoders.is_empty() {
            "No working video encoder was found. Install GStreamer's VA-API (Intel, AMD) or NVIDIA plugins.".to_string()
        } else {
            c.platform.encoder_summary().join("\n")
        });
    });
    page.upcast()
}
