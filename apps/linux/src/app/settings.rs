//! Settings: quality limits, the tablet monitor, app behaviour, network and
//! advanced options. Mirrors the Windows app's Settings page.

use std::rc::Rc;

use adw::prelude::*;
use secondscrn_protocol::Codec;
use secondscrn_session::settings::{EncoderGpu, Theme};
use secondscrn_session::snapshot::HostSnapshot;

use super::widgets::{self, PRESETS};
use super::{Ctx, VERSION, system};

const FPS_LIMITS: [u32; 5] = [30, 60, 90, 120, 144];
const LINGER: [u32; 5] = [0, 10, 30, 60, 300];
const CODECS: [Option<Codec>; 4] = [None, Some(Codec::H264), Some(Codec::H265), Some(Codec::Av1)];
const GPUS: [Option<EncoderGpu>; 3] = [None, Some(EncoderGpu::Integrated), Some(EncoderGpu::Dedicated)];
const THEMES: [Theme; 3] = [Theme::System, Theme::Light, Theme::Dark];

fn nearest(list: &[u32], v: u32) -> u32 {
    list.iter().enumerate().min_by_key(|(_, x)| x.abs_diff(v)).map(|(i, _)| i as u32).unwrap_or(0)
}

pub fn page(ctx: &Rc<Ctx>) -> gtk::Widget {
    let page = adw::PreferencesPage::new();

    // ---- Quality ----------------------------------------------------------
    let quality = widgets::group(
        "Quality",
        Some("The limits apply to every preset and tablet; lower them if your PC or network can't keep up."),
    );
    let default_preset = widgets::combo(ctx, "Default preset", &PRESETS, |c, i| {
        if let Some(p) = widgets::preset_at(i) {
            c.engine.set_preset(p);
        }
    });
    default_preset.set_subtitle(
        "For tablets without a preset of their own. Automatic: full resolution at 60 fps. High quality: about twice \
         the bitrate. Smooth: 75 % resolution at up to 120 fps. Weak Wi-Fi: 67 % resolution at 30 fps.",
    );
    let mut fps_items = vec!["No limit".to_string()];
    fps_items.extend(FPS_LIMITS.iter().map(|f| format!("{f} fps")));
    let fps_refs: Vec<&str> = fps_items.iter().map(String::as_str).collect();
    let fps_limit = widgets::combo(ctx, "Highest frame rate", &fps_refs, |c, i| {
        c.change(|s| s.fps_limit = if i == 0 { None } else { FPS_LIMITS.get(i as usize - 1).copied() })
    });
    let bitrate = adw::SpinRow::with_range(1.0, 150.0, 1.0);
    bitrate.set_title("Mbit/s");
    let limit_bitrate = widgets::switch(ctx, "Limit bitrate", None, {
        let bitrate = bitrate.clone();
        move |c, on| c.change(|s| s.max_bitrate_mbps = if on { bitrate.value() as u32 } else { 0 })
    });
    {
        let weak = Rc::downgrade(ctx);
        bitrate.connect_value_notify(move |r| {
            if let Some(c) = weak.upgrade()
                && !c.updating.get()
            {
                c.change(|s| {
                    if s.max_bitrate_mbps > 0 {
                        s.max_bitrate_mbps = r.value() as u32
                    }
                });
            }
        });
    }
    for r in [
        default_preset.upcast_ref::<gtk::Widget>(),
        fps_limit.upcast_ref(),
        limit_bitrate.upcast_ref(),
        bitrate.upcast_ref(),
    ] {
        quality.add(r);
    }

    // ---- Tablet monitor ---------------------------------------------------
    let monitor = widgets::group(
        "Tablet monitor",
        Some(
            "The tablet appears as a monitor through the desktop's screen sharing (GNOME, KDE Plasma on Wayland). \
             Nothing is installed in the system.",
        ),
    );
    let linger = widgets::combo(
        ctx,
        "After the tablet disconnects, keep its monitor for",
        &["Remove it right away", "10 seconds", "30 seconds", "1 minute", "5 minutes"],
        |c, i| c.change(|s| s.display_linger_secs = LINGER.get(i as usize).copied().unwrap_or(30)),
    );
    linger.set_subtitle("Short Wi-Fi interruptions then don't move your windows back to the main screen.");
    monitor.add(&linger);

    // ---- App --------------------------------------------------------------
    let app = widgets::group("App", None);
    let autostart = widgets::switch(ctx, "Start at login", None, |c, on| match system::set_autostart(on) {
        Ok(()) => c.change(|s| s.start_with_windows = on),
        Err(e) => c.toast(&format!("Couldn't change autostart: {e}")),
    });
    let minimized =
        widgets::switch(ctx, "Start minimized to the system tray", None, |c, on| c.change(|s| s.start_minimized = on));
    let tray_close =
        widgets::switch(ctx, "Closing the window keeps SecondScrn running in the system tray", None, |c, on| {
            c.change(|s| s.close_to_tray = on)
        });
    if ctx.tray.borrow().is_none() {
        let note = "No system tray on this desktop (on GNOME, the AppIndicator extension adds one).";
        minimized.set_subtitle(note);
        tray_close.set_subtitle(note);
        minimized.set_sensitive(false);
        tray_close.set_sensitive(false);
    }
    let theme = widgets::combo(ctx, "Appearance", &["System", "Light", "Dark"], |c, i| {
        c.change(|s| s.theme = THEMES.get(i as usize).copied().unwrap_or_default())
    });
    let updates =
        widgets::switch(ctx, "Check for updates at startup", None, |c, on| c.change(|s| s.check_for_updates = on));
    let check = gtk::Button::builder().label("Check now").valign(gtk::Align::Center).build();
    updates.add_suffix(&check);
    {
        let weak = Rc::downgrade(ctx);
        check.connect_clicked(move |_| {
            if let Some(c) = weak.upgrade() {
                system::check_for_updates(c, true);
            }
        });
    }
    let update_row = adw::ActionRow::builder().title("Update available").subtitle_lines(3).build();
    let download =
        gtk::Button::builder().label("Download").valign(gtk::Align::Center).css_classes(["suggested-action"]).build();
    update_row.add_suffix(&download);
    {
        let weak = Rc::downgrade(ctx);
        download.connect_clicked(move |_| {
            let Some(c) = weak.upgrade() else { return };
            let version = c.update.borrow().clone();
            if let Some(v) = version {
                widgets::open_uri(&c, &system::release_page(&v));
            }
        });
    }
    for r in [
        autostart.upcast_ref::<gtk::Widget>(),
        minimized.upcast_ref(),
        tray_close.upcast_ref(),
        theme.upcast_ref(),
        updates.upcast_ref(),
        update_row.upcast_ref(),
    ] {
        app.add(r);
    }

    // ---- Network ----------------------------------------------------------
    let network = widgets::group(
        "Network",
        Some(
            "All connections are encrypted and only paired tablets are accepted. If a firewall is active, allow \
             TCP and UDP on the port below (and UDP 5353 for discovery) on private networks.",
        ),
    );
    let name =
        adw::EntryRow::builder().title("Name shown to tablets (empty = computer name)").show_apply_button(true).build();
    {
        let weak = Rc::downgrade(ctx);
        name.connect_apply(move |e| {
            if let Some(c) = weak.upgrade() {
                let text = e.text().trim().to_string();
                c.change(|s| s.host_name = (!text.is_empty()).then_some(text));
            }
        });
    }
    let discoverable = widgets::switch(ctx, "Let tablets on this network find this PC automatically", None, |c, on| {
        c.change(|s| s.discoverable = on)
    });
    let port = adw::SpinRow::with_range(1024.0, 65535.0, 1.0);
    port.set_title("Port");
    {
        let weak = Rc::downgrade(ctx);
        port.connect_value_notify(move |r| {
            if let Some(c) = weak.upgrade()
                && !c.updating.get()
            {
                c.change(|s| s.control_port = r.value() as u16);
            }
        });
    }
    network.add(&name);
    network.add(&discoverable);
    network.add(&port);

    // ---- Advanced ---------------------------------------------------------
    let advanced = widgets::group("Advanced", None);
    let codec = widgets::combo(ctx, "Video codec", &["Automatic (best available)", "H.264", "HEVC", "AV1"], |c, i| {
        c.change(|s| s.preferred_codec = CODECS.get(i as usize).copied().flatten())
    });
    let gpu = widgets::combo(
        ctx,
        "Encode on",
        &["Automatic (recommended)", "Intel / AMD graphics (VA-API)", "NVIDIA graphics (NVENC)"],
        |c, i| {
            let choice = GPUS.get(i as usize).copied().flatten();
            c.change(|s| s.encoder_gpu_choice = choice);
            // Re-checks the encoders (a few seconds) off the UI thread.
            let (platform, engine) = (c.platform.clone(), c.engine.clone());
            std::thread::spawn(move || {
                platform.set_encoder_gpu(choice);
                engine.refresh_platform();
            });
        },
    );
    gpu.set_subtitle("Automatic uses the graphics the desktop runs on, NVIDIA's encoder if that has none.");
    let verbose = widgets::switch(ctx, "Detailed logging (takes effect after restart)", None, |c, on| {
        c.change(|s| s.verbose_logging = on)
    });
    advanced.add(&codec);
    advanced.add(&gpu);
    advanced.add(&verbose);

    // ---- About ------------------------------------------------------------
    let about = widgets::group("About", None);
    let version = adw::ActionRow::builder()
        .title(format!("SecondScrn {VERSION}"))
        .subtitle(
            "Open-source software under the MIT licence. It includes third-party components under their own licences. \
             Tablets must run the same version.",
        )
        .subtitle_lines(3)
        .build();
    let site = gtk::Button::builder().label("Website").valign(gtk::Align::Center).build();
    version.add_suffix(&site);
    {
        let weak = Rc::downgrade(ctx);
        site.connect_clicked(move |_| {
            if let Some(c) = weak.upgrade() {
                widgets::open_uri(&c, secondscrn_protocol::RELEASES_URL.trim_end_matches("/releases"));
            }
        });
    }
    about.add(&version);

    for g in [&quality, &monitor, &app, &network, &advanced, &about] {
        page.add(g);
    }

    let weak = Rc::downgrade(ctx);
    ctx.on_snapshot(move |s: &HostSnapshot| {
        let Some(c) = weak.upgrade() else { return };
        let st = &s.settings;
        widgets::select(&default_preset, Some(widgets::preset_index(st.preset)));
        widgets::select(&fps_limit, Some(st.fps_limit.map(|f| nearest(&FPS_LIMITS, f) + 1).unwrap_or(0)));
        limit_bitrate.set_active(st.max_bitrate_mbps > 0);
        bitrate.set_sensitive(st.max_bitrate_mbps > 0);
        if st.max_bitrate_mbps > 0 {
            bitrate.set_value(st.max_bitrate_mbps as f64);
        } else if bitrate.value() < 1.0 {
            bitrate.set_value(50.0);
        }
        widgets::select(&linger, Some(nearest(&LINGER, st.display_linger_secs)));
        autostart.set_active(st.start_with_windows);
        minimized.set_active(st.start_minimized);
        tray_close.set_active(st.close_to_tray);
        widgets::select(&theme, THEMES.iter().position(|t| *t == st.theme).map(|i| i as u32));
        updates.set_active(st.check_for_updates);
        let available = c.update.borrow().clone();
        update_row.set_visible(available.is_some());
        if let Some(v) = available {
            update_row.set_title(&format!("SecondScrn {v} is available"));
            update_row.set_subtitle(&format!(
                "Download it and update the tablet app to {v} at the same time (both must run the same version)."
            ));
        }
        if !name.has_focus() {
            name.set_text(st.host_name.as_deref().unwrap_or(""));
        }
        discoverable.set_active(st.discoverable);
        if port.value() as u16 != st.control_port {
            port.set_value(st.control_port as f64);
        }
        widgets::select(&codec, CODECS.iter().position(|x| *x == st.preferred_codec).map(|i| i as u32));
        widgets::select(&gpu, GPUS.iter().position(|x| *x == st.encoder_gpu_choice).map(|i| i as u32));
        verbose.set_active(st.verbose_logging);
    });
    page.upcast()
}
