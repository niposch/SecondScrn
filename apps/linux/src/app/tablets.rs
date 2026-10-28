//! Tablets: paired tablets with their preset and reconnect setting.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use secondscrn_session::snapshot::{HostSnapshot, PairedDevice};

use super::Ctx;
use super::widgets::{self, PRESETS};

/// The listed tablets, the connected one and their rows.
type Shown = (Vec<PairedDevice>, Option<String>, Vec<adw::ExpanderRow>);

pub fn page(ctx: &Rc<Ctx>) -> gtk::Widget {
    let page = adw::PreferencesPage::new();
    let list = widgets::group(
        "Paired tablets",
        Some(
            "A tablet's preset is remembered for it; choosing one on Home or on the tablet changes it here too. \
             To pair another tablet, open SecondScrn on it and choose this PC.",
        ),
    );
    let empty = adw::ActionRow::builder()
        .title("No tablets paired yet")
        .subtitle(
            "Open SecondScrn on the tablet and choose this PC. The tablet shows a 6-digit code; SecondScrn asks you \
             for it here. Only paired tablets can connect or send input.",
        )
        .subtitle_lines(4)
        .build();
    list.add(&empty);
    page.add(&list);

    // Rows are rebuilt when the list or a tablet's settings change.
    let shown: Rc<RefCell<Shown>> = Rc::default();
    let weak = Rc::downgrade(ctx);
    ctx.on_snapshot(move |s: &HostSnapshot| {
        let Some(c) = weak.upgrade() else { return };
        let connected = s.session.as_ref().map(|x| x.fingerprint.clone());
        let mut st = shown.borrow_mut();
        if st.0 == s.paired_devices && st.1 == connected {
            return;
        }
        for row in st.2.drain(..) {
            list.remove(&row);
        }
        empty.set_visible(s.paired_devices.is_empty());
        for d in &s.paired_devices {
            let row = device_row(&c, d, connected.as_deref() == Some(d.fingerprint.as_str()), s);
            list.add(&row);
            st.2.push(row);
        }
        st.0 = s.paired_devices.clone();
        st.1 = connected;
    });
    page.upcast()
}

fn device_row(ctx: &Rc<Ctx>, d: &PairedDevice, connected: bool, s: &HostSnapshot) -> adw::ExpanderRow {
    let seen = d.last_seen.map(|t| format!("last connected {}", widgets::ago(t))).unwrap_or("never connected".into());
    let row = adw::ExpanderRow::builder()
        .title(&d.name)
        .subtitle(if connected { format!("● connected · {}", d.model) } else { format!("{} · {seen}", d.model) })
        .build();

    let mut items = vec![format!("Default ({})", PRESETS[widgets::preset_index(s.settings.preset) as usize])];
    items.extend(PRESETS.iter().map(|p| p.to_string()));
    let refs: Vec<&str> = items.iter().map(String::as_str).collect();
    let fp = d.fingerprint.clone();
    let preset = widgets::combo(ctx, "Preset", &refs, move |c, i| {
        let Some(dev) = c.snapshot().paired_devices.into_iter().find(|x| x.fingerprint == fp) else { return };
        let mut prefs = dev.preferences;
        prefs.preset = if i == 0 { None } else { widgets::preset_at(i - 1) };
        if let Err(e) = c.engine.set_device_preferences(&fp, prefs) {
            c.toast(&e.to_string());
        }
    });
    ctx.updating.set(true);
    preset.set_selected(d.preferences.preset.map(|p| widgets::preset_index(p) + 1).unwrap_or(0));
    ctx.updating.set(false);

    let fp = d.fingerprint.clone();
    let auto = widgets::switch(
        ctx,
        "Reconnect automatically",
        Some("Connects again after Wi-Fi drops or restarts without asking."),
        move |c, on| {
            let Some(dev) = c.snapshot().paired_devices.into_iter().find(|x| x.fingerprint == fp) else { return };
            let mut prefs = dev.preferences;
            prefs.allow_auto_reconnect = on;
            if let Err(e) = c.engine.set_device_preferences(&fp, prefs) {
                c.toast(&e.to_string());
            }
        },
    );
    ctx.updating.set(true);
    auto.set_active(d.preferences.allow_auto_reconnect);
    ctx.updating.set(false);

    let remove_row = adw::ActionRow::builder().title(format!("Tablet ID {}", d.fingerprint_short)).build();
    let remove =
        gtk::Button::builder().label("Remove").valign(gtk::Align::Center).css_classes(["destructive-action"]).build();
    remove_row.add_suffix(&remove);
    let (weak, fp, name) = (Rc::downgrade(ctx), d.fingerprint.clone(), d.name.clone());
    remove.connect_clicked(move |_| {
        let Some(c) = weak.upgrade() else { return };
        let dialog = adw::AlertDialog::new(
            Some(&format!("Remove {name}?")),
            Some("It can no longer connect until it is paired again."),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.set_close_response("cancel");
        let (weak, fp) = (Rc::downgrade(&c), fp.clone());
        dialog.connect_response(Some("remove"), move |_, _| {
            if let Some(c) = weak.upgrade()
                && let Err(e) = c.engine.revoke_device(&fp)
            {
                c.toast(&e.to_string());
            }
        });
        dialog.present(Some(&c.window));
    });

    row.add_row(&preset);
    row.add_row(&auto);
    row.add_row(&remove_row);
    row
}
