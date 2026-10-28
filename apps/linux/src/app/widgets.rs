//! Small helpers shared by the pages: rows, labels and text formatting.

use std::rc::Rc;

use adw::prelude::*;
use secondscrn_protocol::{Codec, QualityPreset};

use super::Ctx;

pub const PRESETS: [&str; 4] = ["Automatic", "High quality", "Smooth", "Weak Wi-Fi"];

pub const PRESET_HINTS: [&str; 4] = [
    "Full resolution at 60 fps; the bitrate follows the network.",
    "About twice the bitrate for sharper text and video, slightly more latency.",
    "75 % resolution at up to 120 fps.",
    "67 % resolution at 30 fps with a low bitrate.",
];

pub fn preset_index(p: QualityPreset) -> u32 {
    QualityPreset::ALL.iter().position(|x| *x == p).unwrap_or(0) as u32
}

pub fn preset_at(i: u32) -> Option<QualityPreset> {
    QualityPreset::ALL.get(i as usize).copied()
}

pub fn codec_name(c: Option<Codec>) -> String {
    c.map(|c| c.to_string()).unwrap_or_else(|| "–".into())
}

/// A combo row whose handler only runs for changes the user makes.
pub fn combo(ctx: &Rc<Ctx>, title: &str, items: &[&str], on_change: impl Fn(&Ctx, u32) + 'static) -> adw::ComboRow {
    let row = adw::ComboRow::builder().title(title).model(&gtk::StringList::new(items)).build();
    let weak = Rc::downgrade(ctx);
    row.connect_selected_notify(move |r| {
        if let Some(c) = weak.upgrade()
            && !c.updating.get()
            && r.selected() != gtk::INVALID_LIST_POSITION
        {
            on_change(&c, r.selected());
        }
    });
    row
}

/// Replaces a combo row's items if they changed (keeps the popup stable).
pub fn set_items(row: &adw::ComboRow, items: &[String]) {
    let current: Vec<String> = row
        .model()
        .and_downcast::<gtk::StringList>()
        .map(|m| (0..m.n_items()).filter_map(|i| m.string(i).map(|s| s.to_string())).collect())
        .unwrap_or_default();
    if current != items {
        let refs: Vec<&str> = items.iter().map(String::as_str).collect();
        row.set_model(Some(&gtk::StringList::new(&refs)));
    }
}

pub fn select(row: &adw::ComboRow, index: Option<u32>) {
    let i = index.unwrap_or(gtk::INVALID_LIST_POSITION);
    if row.selected() != i {
        row.set_selected(i);
    }
}

pub fn switch(
    ctx: &Rc<Ctx>,
    title: &str,
    subtitle: Option<&str>,
    on_change: impl Fn(&Ctx, bool) + 'static,
) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder().title(title).build();
    if let Some(s) = subtitle {
        row.set_subtitle(s);
    }
    let weak = Rc::downgrade(ctx);
    row.connect_active_notify(move |r| {
        if let Some(c) = weak.upgrade()
            && !c.updating.get()
        {
            on_change(&c, r.is_active());
        }
    });
    row
}

/// A row showing a value on the right in tabular figures, so changing
/// numbers don't move the text around them.
pub fn value_row(title: &str) -> (adw::ActionRow, gtk::Label) {
    let row = adw::ActionRow::builder().title(title).build();
    let label = gtk::Label::builder().xalign(1.0).css_classes(["numeric", "dim-label"]).build();
    row.add_suffix(&label);
    (row, label)
}

pub fn group(title: &str, description: Option<&str>) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder().title(title).build();
    if let Some(d) = description {
        g.set_description(Some(d));
    }
    g
}

/// `v` right-aligned in `width` characters, padded with figure spaces.
pub fn fixed(v: f64, decimals: usize, width: usize) -> String {
    let s = format!("{v:.decimals$}");
    format!("{}{s}", "\u{2007}".repeat(width.saturating_sub(s.chars().count())))
}

pub fn ago(unix: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let age = now.saturating_sub(unix);
    match age {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", age / 60),
        3600..86400 => format!("{} h ago", age / 3600),
        _ => format!("{} days ago", age / 86400),
    }
}

/// Opens a URL or file in the desktop's default application.
pub fn open_uri(ctx: &Ctx, uri: &str) {
    gtk::UriLauncher::new(uri).launch(Some(&ctx.window), gtk::gio::Cancellable::NONE, |r| {
        if let Err(e) = r {
            tracing::warn!(error = %e, "couldn't open link");
        }
    });
}
