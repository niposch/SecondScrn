//! Home: connection, the connected tablet's picture settings and live
//! statistics.

use std::rc::Rc;

use adw::prelude::*;
use secondscrn_session::platform::DriverStatus;
use secondscrn_session::snapshot::{HostSnapshot, LinkHealth, SessionPhase};

use super::widgets::{self, PRESET_HINTS, PRESETS, fixed};
use super::{Ctx, system};

pub fn page(ctx: &Rc<Ctx>) -> gtk::Widget {
    let page = adw::PreferencesPage::new();

    // ---- Banners ----------------------------------------------------------
    let mismatch = adw::Banner::builder().use_markup(false).build();
    let limited = adw::Banner::builder().use_markup(false).build();
    let desktop = adw::Banner::builder().use_markup(false).build();
    let reconnect = adw::Banner::builder()
        .title("Changed settings apply when the tablet reconnects.")
        .button_label("Reconnect now")
        .build();
    {
        let weak = Rc::downgrade(ctx);
        mismatch.connect_button_clicked(move |_| {
            let Some(c) = weak.upgrade() else { return };
            if let Some(m) = c.snapshot().version_mismatch {
                widgets::open_uri(&c, &system::version_mismatch_url(&m));
            }
        });
        let weak = Rc::downgrade(ctx);
        reconnect.connect_button_clicked(move |_| {
            if let Some(c) = weak.upgrade() {
                c.engine.reconnect();
            }
        });
    }

    // ---- Connection -------------------------------------------------------
    let connection = widgets::group("Connection", None);
    let status = adw::ActionRow::builder().title("No tablet connected").subtitle_lines(4).build();
    let disconnect = gtk::Button::builder().label("Disconnect").valign(gtk::Align::Center).build();
    status.add_suffix(&disconnect);
    let enter_code = gtk::Button::builder()
        .label("Enter code…")
        .valign(gtk::Align::Center)
        .css_classes(["suggested-action"])
        .build();
    status.add_suffix(&enter_code);
    {
        let weak = Rc::downgrade(ctx);
        enter_code.connect_clicked(move |_| {
            if let Some(c) = weak.upgrade() {
                c.reopen_pairing();
            }
        });
    }
    {
        let weak = Rc::downgrade(ctx);
        disconnect.connect_clicked(move |_| {
            if let Some(c) = weak.upgrade() {
                c.engine.disconnect();
            }
        });
    }
    let (health_row, health) = widgets::value_row("Connection quality");
    connection.add(&status);
    connection.add(&health_row);

    // ---- Picture ----------------------------------------------------------
    let picture = widgets::group("Picture", None);
    let preset = widgets::combo(ctx, "Preset", &PRESETS, |c, i| {
        let Some(p) = widgets::preset_at(i) else { return };
        if c.snapshot().session.is_some() {
            if let Err(e) = c.engine.set_session_preset(p) {
                tracing::debug!(error = %e, "preset not changed");
            }
        } else {
            c.engine.set_preset(p);
        }
    });
    let resolution = widgets::combo(ctx, "Resolution", &[], |c, i| {
        if let Some(r) = c.snapshot().session.and_then(|s| s.resolutions.get(i as usize).cloned()) {
            let _ = c.engine.set_resolution(r.scale_percent);
        }
    });
    let frame_rate = widgets::combo(ctx, "Frame rate", &[], |c, i| {
        if let Some(f) = c.snapshot().session.and_then(|s| s.frame_rates.get(i as usize).copied()) {
            let _ = c.engine.set_frame_rate(f);
        }
    });
    let custom = adw::ActionRow::builder().title("Resolution or frame rate changed from the preset").build();
    let reset = gtk::Button::builder().label("Reset to preset").valign(gtk::Align::Center).build();
    custom.add_suffix(&reset);
    {
        let weak = Rc::downgrade(ctx);
        reset.connect_clicked(move |_| {
            let Some(c) = weak.upgrade() else { return };
            if let Some(s) = c.snapshot().session {
                let _ = c.engine.set_session_preset(s.preset);
            }
        });
    }
    picture.add(&preset);
    picture.add(&resolution);
    picture.add(&frame_rate);
    picture.add(&custom);

    // ---- Statistics -------------------------------------------------------
    let stats = widgets::group("Statistics", None);
    let (res_row, res) = widgets::value_row("Resolution");
    let (fps_row, fps) = widgets::value_row("Frame rate");
    let (bit_row, bitrate) = widgets::value_row("Bitrate");
    let (lat_row, latency) = widgets::value_row("Latency (round trip)");
    let (loss_row, loss) = widgets::value_row("Packet loss (before repair)");
    let (codec_row, codec) = widgets::value_row("Codec");
    for r in [&res_row, &fps_row, &bit_row, &lat_row, &loss_row, &codec_row] {
        stats.add(r);
    }

    // ---- Tablet monitor ---------------------------------------------------
    let monitor = widgets::group("Tablet monitor", None);
    let desktop_row = adw::ActionRow::builder().title("Virtual monitor").subtitle_lines(3).build();
    let arrange = gtk::Button::builder().label("Display settings…").valign(gtk::Align::Center).build();
    desktop_row.add_suffix(&arrange);
    arrange.connect_clicked(|_| system::open_display_settings());
    let permission = adw::ActionRow::builder()
        .title("Screen sharing permission")
        .subtitle("The desktop asks once whether SecondScrn may add a monitor and control the pointer. Allow it now so the first connection doesn't wait for you.")
        .subtitle_lines(4)
        .build();
    let allow =
        gtk::Button::builder().label("Allow…").valign(gtk::Align::Center).css_classes(["suggested-action"]).build();
    permission.add_suffix(&allow);
    {
        let (weak, row) = (Rc::downgrade(ctx), permission.clone());
        allow.connect_clicked(move |b| {
            let Some(c) = weak.upgrade() else { return };
            b.set_sensitive(false);
            let platform = c.platform.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let _ = tx.send(platform.authorize());
            });
            let (weak, row, b) = (weak.clone(), row.clone(), b.clone());
            gtk::glib::spawn_future_local(async move {
                let result = rx.await.unwrap_or_else(|_| Err("stopped".into()));
                b.set_sensitive(true);
                let Some(c) = weak.upgrade() else { return };
                match result {
                    Ok(()) => {
                        row.set_visible(!system::portal_permission_saved(&c.data_dir));
                        c.toast("Permission saved");
                    }
                    Err(e) => c.toast(&format!("Not allowed: {e}")),
                }
            });
        });
    }
    monitor.add(&desktop_row);
    monitor.add(&permission);

    page.add(&connection);
    page.add(&picture);
    page.add(&stats);
    page.add(&monitor);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    for b in [&mismatch, &limited, &desktop, &reconnect] {
        content.append(b);
    }
    page.set_vexpand(true);
    content.append(&page);

    let weak = Rc::downgrade(ctx);
    ctx.on_snapshot(move |s: &HostSnapshot| {
        let Some(c) = weak.upgrade() else { return };
        // Banners
        match &s.version_mismatch {
            Some(m) => {
                let (text, button) = system::version_mismatch_text(m);
                mismatch.set_title(&text);
                mismatch.set_button_label(Some(&button));
                mismatch.set_revealed(true);
            }
            None => mismatch.set_revealed(false),
        }
        match s.session.as_ref().filter(|x| x.network_limited) {
            Some(x) => {
                let (w, h, f) = x.stream.as_ref().map(|c| (c.width, c.height, c.max_fps)).unwrap_or_default();
                limited.set_title(&format!(
                    "The network can't carry {w} × {h} at {f} fps right now, so fewer frames are sent. Choose a lower resolution or frame rate."
                ));
                limited.set_revealed(true);
            }
            None => limited.set_revealed(false),
        }
        match &s.driver {
            DriverStatus::Unavailable { reason } => {
                desktop.set_title(reason);
                desktop.set_revealed(true);
            }
            _ if s.encoders.is_empty() => {
                desktop.set_title("No working video encoder was found. Install GStreamer's VA-API (Intel, AMD) or NVIDIA plugins.");
                desktop.set_revealed(true);
            }
            _ => desktop.set_revealed(false),
        }
        reconnect.set_revealed(c.needs_reconnect(s));

        // Connection
        let (title, detail) = match (&s.session, &s.pairing) {
            (None, Some(p)) => (format!("Pairing request from {}", p.device_name), "Enter the code shown on the tablet.".to_string()),
            (None, None) => (
                "No tablet connected".to_string(),
                if s.listening {
                    format!(
                        "Open SecondScrn on your tablet and choose “{}”. Manual address: {} (port {}).",
                        s.host_name,
                        s.addresses.join(" or "),
                        s.port
                    )
                } else {
                    s.last_error.clone().unwrap_or_else(|| "Not accepting connections.".into())
                },
            ),
            (Some(x), _) => {
                let title = match x.phase {
                    SessionPhase::Negotiating => format!("Connecting {}…", x.device_name),
                    SessionPhase::Interrupted => format!("{}: connection interrupted", x.device_name),
                    SessionPhase::Streaming => format!("Connected to {}", x.device_name),
                };
                let detail = match &x.stream {
                    Some(st) => format!(
                        "{} × {} · up to {} fps · {} ({}) · {}",
                        st.width,
                        st.height,
                        st.max_fps,
                        st.codec,
                        system::encoder_name(s, st.codec),
                        x.remote_address
                    ),
                    None => x.remote_address.clone(),
                };
                (title, detail)
            }
        };
        status.set_title(&title);
        status.set_subtitle(&detail);
        disconnect.set_visible(s.session.is_some());
        enter_code.set_visible(s.session.is_none() && s.pairing.is_some());
        health_row.set_visible(s.session.is_some());
        health.set_text(match s.session.as_ref().map(|x| x.metrics.health) {
            Some(LinkHealth::Good) => "Excellent",
            Some(LinkHealth::Fair) => "Fair",
            Some(LinkHealth::Poor) => "Poor",
            _ => "Measuring…",
        });

        // Picture
        let current = s.session.as_ref().map(|x| x.preset).unwrap_or(s.settings.preset);
        let pi = widgets::preset_index(current);
        widgets::select(&preset, Some(pi));
        preset.set_subtitle(if s.session.is_some() {
            PRESET_HINTS[pi as usize]
        } else {
            "The default for tablets without a preset of their own."
        });
        let session = s.session.as_ref();
        let streaming = session.is_some_and(|x| x.phase == SessionPhase::Streaming);
        let res_items: Vec<String> = session
            .map(|x| x.resolutions.iter().map(|r| format!("{} × {} ({} %)", r.width, r.height, r.scale_percent)).collect())
            .unwrap_or_default();
        widgets::set_items(&resolution, &res_items);
        let scale = session.and_then(|x| x.stream.as_ref()).map(|st| st.scale_percent);
        widgets::select(
            &resolution,
            session.and_then(|x| x.resolutions.iter().position(|r| Some(r.scale_percent) == scale)).map(|i| i as u32),
        );
        resolution.set_sensitive(streaming && !res_items.is_empty());
        resolution.set_subtitle(if session.is_some() {
            "Lower resolutions need less bandwidth; the tablet scales the picture up. Remembered for this tablet."
        } else {
            "Available while a tablet is connected."
        });
        let fps_items: Vec<String> =
            session.map(|x| x.frame_rates.iter().map(|f| format!("{f} fps")).collect()).unwrap_or_default();
        widgets::set_items(&frame_rate, &fps_items);
        let max_fps = session.and_then(|x| x.stream.as_ref()).map(|st| st.max_fps);
        widgets::select(
            &frame_rate,
            session.and_then(|x| x.frame_rates.iter().position(|f| Some(*f) == max_fps)).map(|i| i as u32),
        );
        frame_rate.set_sensitive(streaming && !fps_items.is_empty());
        custom.set_visible(session.is_some_and(|x| x.custom));
        custom.set_title(&format!("Resolution or frame rate changed from “{}”", PRESETS[pi as usize]));

        // Statistics
        stats.set_visible(session.is_some());
        if let Some(x) = session {
            let m = &x.metrics;
            res.set_text(&x.stream.as_ref().map(|st| format!("{} × {}", st.width, st.height)).unwrap_or("–".into()));
            let shown = m.client.map(|c| c.render_fps).filter(|f| *f > 0.0);
            fps.set_text(&format!("{} fps", fixed(shown.unwrap_or(m.pipeline.encode_fps) as f64, 0, 3)));
            fps_row.set_subtitle(&match shown {
                Some(_) => format!(
                    "shown · sending {} · target {}",
                    fixed(m.pipeline.encode_fps as f64, 0, 3),
                    fixed(m.pipeline.fps_target as f64, 0, 3)
                ),
                None => format!("sending · target {}", fixed(m.pipeline.fps_target as f64, 0, 3)),
            });
            bitrate.set_text(&format!("{} Mbit/s", fixed(m.send_rate_bps as f64 / 1e6, 1, 5)));
            bit_row.set_subtitle(&format!("encoder target {}", fixed(m.pipeline.bitrate_target_bps as f64 / 1e6, 1, 5)));
            latency.set_text(&m.rtt_ms.map(|r| format!("{} ms", fixed(r as f64, 0, 3))).unwrap_or("–".into()));
            loss.set_text(&m.loss.map(|l| format!("{} %", fixed(l as f64 * 100.0, 1, 4))).unwrap_or("–".into()));
            codec.set_text(&widgets::codec_name(x.stream.as_ref().map(|st| st.codec)));
        }

        // Monitor
        permission.set_visible(
            matches!(s.driver, DriverStatus::Ready { .. }) && !system::portal_permission_saved(&c.data_dir),
        );
        desktop_row.set_subtitle(&match &s.driver {
            DriverStatus::Ready { detail } => format!(
                "{detail}. Arrange it next to your screens in the display settings. Pen: {}.",
                if c.platform.has_pen_tablet() { "pressure and tilt (virtual tablet)" } else { "set up on the first connection" }
            ),
            DriverStatus::Unavailable { reason } => reason.clone(),
            DriverStatus::NotInstalled => "Not available on this desktop.".into(),
            DriverStatus::NotApplicable => "Test pattern (no desktop).".into(),
        });
    });

    content.upcast()
}
