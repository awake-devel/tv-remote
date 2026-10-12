//! Remote page: now-playing bar (while something casts), D-pad, navigation keys,
//! volume / channel, number pad, colour and media keys, power; plus the window keyboard
//! shortcuts. While a cast plays, the media keys and ← → act on it (see
//! [`Ctx::media_key`]).

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};
use tvremote_core::Key;

use crate::ctx::Ctx;
use crate::pages::castbar::CastBar;

fn key_button(ctx: &Rc<Ctx>, key: Key, icon: Option<&str>, label: Option<&str>) -> gtk::Button {
    let b = gtk::Button::new();
    match (icon, label) {
        (Some(i), Some(l)) => {
            let c = adw::ButtonContent::builder().icon_name(i).label(l).build();
            b.set_child(Some(&c));
        }
        (Some(i), None) => b.set_icon_name(i),
        (None, Some(l)) => b.set_label(l),
        (None, None) => b.set_label(key.label()),
    }
    b.set_tooltip_text(Some(&format!("{} ({})", key.label(), key.code())));
    let weak = ctx.weak();
    b.connect_clicked(move |_| {
        if let Some(c) = weak.upgrade() {
            c.media_key(key.clone());
        }
    });
    b
}

fn circular(b: &gtk::Button, size: i32) {
    b.add_css_class("circular");
    b.set_size_request(size, size);
}

fn hbox(spacing: i32) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, spacing);
    b.set_halign(gtk::Align::Center);
    b
}

fn caption(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("caption");
    l.add_css_class("dim-label");
    l
}

pub fn build(ctx: &Rc<Ctx>) -> (gtk::Widget, Rc<CastBar>) {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 18);
    root.set_margin_top(24);
    root.set_margin_bottom(24);
    root.set_margin_start(12);
    root.set_margin_end(12);

    // --- top row: power + navigation helpers ---------------------------------------------
    // A WrapBox so the row wraps instead of overflowing narrow windows.
    let top = adw::WrapBox::builder()
        .child_spacing(8)
        .line_spacing(8)
        .justify(adw::JustifyMode::None)
        .halign(gtk::Align::Center)
        .build();
    let power = key_button(ctx, Key::Power, Some("system-shutdown-symbolic"), None);
    circular(&power, 48);
    power.add_css_class("destructive-action");
    top.append(&power);
    for (k, label) in [
        (Key::Source, "Source"),
        (Key::Menu, "Menu"),
        (Key::Info, "Info"),
        (Key::Guide, "Guide"),
        (Key::Tools, "Tools"),
        (Key::Exit, "Exit"),
    ] {
        let b = key_button(ctx, k, None, Some(label));
        b.add_css_class("pill");
        top.append(&b);
    }
    root.append(&top);

    // --- middle: volume | D-pad | channels -------------------------------------------------
    let middle = hbox(36);

    let vol = gtk::Box::new(gtk::Orientation::Vertical, 8);
    vol.set_valign(gtk::Align::Center);
    let vup = key_button(ctx, Key::VolumeUp, Some("list-add-symbolic"), None);
    let mute = key_button(ctx, Key::Mute, Some("audio-volume-muted-symbolic"), None);
    let vdown = key_button(ctx, Key::VolumeDown, Some("list-remove-symbolic"), None);
    for b in [&vup, &mute, &vdown] {
        circular(b, 52);
        vol.append(b);
    }
    vol.prepend(&caption("Volume"));
    vol.add_css_class("tv-rocker");
    middle.append(&vol);

    let dpad = gtk::Grid::new();
    dpad.set_row_spacing(6);
    dpad.set_column_spacing(6);
    dpad.set_valign(gtk::Align::Center);
    let up = key_button(ctx, Key::Up, Some("go-up-symbolic"), None);
    let down = key_button(ctx, Key::Down, Some("go-down-symbolic"), None);
    let left = key_button(ctx, Key::Left, Some("go-previous-symbolic"), None);
    let right = key_button(ctx, Key::Right, Some("go-next-symbolic"), None);
    let ok = key_button(ctx, Key::Enter, None, Some("OK"));
    for b in [&up, &down, &left, &right] {
        circular(b, 64);
    }
    circular(&ok, 80);
    ok.add_css_class("suggested-action");
    ok.add_css_class("tv-ok");
    let back = key_button(ctx, Key::Return, Some("edit-undo-symbolic"), None);
    back.set_tooltip_text(Some("Back (Backspace)"));
    let home = key_button(ctx, Key::Home, Some("go-home-symbolic"), None);
    home.set_tooltip_text(Some("Home (H)"));
    for b in [&back, &home] {
        circular(b, 48);
        b.set_halign(gtk::Align::Center);
        b.set_valign(gtk::Align::Center);
    }
    for b in [&up, &down, &left, &right, &ok] {
        b.set_halign(gtk::Align::Center);
        b.set_valign(gtk::Align::Center);
    }
    dpad.attach(&up, 1, 0, 1, 1);
    dpad.attach(&left, 0, 1, 1, 1);
    dpad.attach(&ok, 1, 1, 1, 1);
    dpad.attach(&right, 2, 1, 1, 1);
    dpad.attach(&down, 1, 2, 1, 1);
    dpad.attach(&back, 0, 2, 1, 1);
    dpad.attach(&home, 2, 2, 1, 1);
    middle.append(&dpad);

    let ch = gtk::Box::new(gtk::Orientation::Vertical, 8);
    ch.set_valign(gtk::Align::Center);
    ch.append(&caption("Channel"));
    let cup = key_button(ctx, Key::ChannelUp, Some("go-up-symbolic"), None);
    let clist = key_button(ctx, Key::ChannelList, Some("view-list-symbolic"), None);
    let cdown = key_button(ctx, Key::ChannelDown, Some("go-down-symbolic"), None);
    for b in [&cup, &clist, &cdown] {
        circular(b, 52);
        ch.append(b);
    }
    ch.add_css_class("tv-rocker");
    middle.append(&ch);
    root.append(&middle);

    // --- number pad ---------------------------------------------------------------------
    let pad = gtk::Grid::new();
    pad.set_row_spacing(6);
    pad.set_column_spacing(6);
    pad.set_halign(gtk::Align::Center);
    for d in 1..=9u8 {
        if let Some(k) = Key::digit(d) {
            let b = key_button(ctx, k, None, Some(&d.to_string()));
            circular(&b, 44);
            let i = i32::from(d - 1);
            pad.attach(&b, i % 3, i / 3, 1, 1);
        }
    }
    let prev = key_button(ctx, Key::PreviousChannel, Some("media-skip-backward-symbolic"), None);
    let zero = key_button(ctx, Key::Num0, None, Some("0"));
    let dash = key_button(ctx, Key::Raw("KEY_DASH".into()), None, Some("‒"));
    dash.set_tooltip_text(Some("Dash (sub-channel separator, KEY_DASH)"));
    for b in [&prev, &zero, &dash] {
        circular(b, 44);
    }
    pad.attach(&prev, 0, 3, 1, 1);
    pad.attach(&zero, 1, 3, 1, 1);
    pad.attach(&dash, 2, 3, 1, 1);

    // --- colour + media keys --------------------------------------------------------------
    let side = gtk::Box::new(gtk::Orientation::Vertical, 18);
    side.set_valign(gtk::Align::Center);
    let colours = hbox(8);
    for (k, css) in [
        (Key::Red, "tv-red"),
        (Key::Green, "tv-green"),
        (Key::Yellow, "tv-yellow"),
        (Key::Blue, "tv-blue"),
    ] {
        let b = key_button(ctx, k, None, Some(" "));
        b.add_css_class("tv-colour");
        b.add_css_class(css);
        b.set_size_request(52, 28);
        colours.append(&b);
    }
    side.append(&colours);
    let media = hbox(6);
    for (k, icon, tip) in [
        (Key::Rewind, "media-seek-backward-symbolic", "Rewind; while casting: back 10 s"),
        (Key::Play, "media-playback-start-symbolic", "Play; while casting: play / pause"),
        (Key::Pause, "media-playback-pause-symbolic", "Pause; while casting: play / pause"),
        (Key::Stop, "media-playback-stop-symbolic", "Stop; while casting: stop the cast"),
        (Key::FastForward, "media-seek-forward-symbolic", "Fast forward; while casting: forward 10 s"),
    ] {
        let b = key_button(ctx, k, Some(icon), None);
        b.set_tooltip_text(Some(tip));
        circular(&b, 44);
        media.append(&b);
    }
    side.append(&media);

    let bottom = gtk::FlowBox::new();
    bottom.set_selection_mode(gtk::SelectionMode::None);
    bottom.set_homogeneous(false);
    bottom.set_max_children_per_line(2);
    bottom.set_column_spacing(36);
    bottom.set_row_spacing(18);
    bottom.set_halign(gtk::Align::Center);
    bottom.append(&pad);
    bottom.append(&side);
    root.append(&bottom);

    let hint = gtk::Label::new(Some(
        "Keyboard: arrows, Enter = OK, Backspace = Back, +/− = volume, M = mute, H = Home, 0-9. \
         While a cast plays, ← → and ⏪ ⏩ seek 10 s and ▶ ⏸ pause / resume it.",
    ));
    hint.add_css_class("dim-label");
    hint.add_css_class("caption");
    hint.set_wrap(true);
    hint.set_justify(gtk::Justification::Center);
    root.append(&hint);

    // Now playing (only while something casts): outside `root` so that, hidden, it
    // adds no spacing.
    let bar = CastBar::new(ctx);
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&bar.widget);
    page.append(&root);

    let clamp = adw::Clamp::builder().maximum_size(720).child(&page).build();
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build();
    (scroll.upcast(), bar)
}

/// Map a key press to a TV key (no modifiers handled here).
pub fn shortcut_key(keyval: gdk::Key) -> Option<Key> {
    use gdk::Key as K;
    Some(match keyval {
        K::Up | K::KP_Up => Key::Up,
        K::Down | K::KP_Down => Key::Down,
        K::Left | K::KP_Left => Key::Left,
        K::Right | K::KP_Right => Key::Right,
        K::Return | K::KP_Enter | K::ISO_Enter => Key::Enter,
        K::BackSpace => Key::Return,
        K::plus | K::KP_Add | K::equal => Key::VolumeUp,
        K::minus | K::KP_Subtract | K::underscore => Key::VolumeDown,
        K::m | K::M => Key::Mute,
        K::h | K::H => Key::Home,
        K::Page_Up => Key::ChannelUp,
        K::Page_Down => Key::ChannelDown,
        K::AudioPlay => Key::Play,
        K::AudioPause => Key::Pause,
        K::AudioStop => Key::Stop,
        K::AudioRewind => Key::Rewind,
        K::AudioForward => Key::FastForward,
        _ => {
            let d = keyval.to_unicode().and_then(|c| c.to_digit(10))?;
            Key::digit(u8::try_from(d).ok()?)?
        }
    })
}

/// Window-level shortcuts: active while `active()` is true, no dialog is open, and
/// the focus is not in a text field.
pub fn install_shortcuts(ctx: &Rc<Ctx>, active: impl Fn() -> bool + 'static) {
    let ctrl = gtk::EventControllerKey::new();
    ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = ctx.weak();
    ctrl.connect_key_pressed(move |_, keyval, _, state| {
        let Some(c) = weak.upgrade() else { return glib::Propagation::Proceed };
        let mods = gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::SUPER_MASK
            | gdk::ModifierType::META_MASK;
        if state.intersects(mods) || !active() || c.window.visible_dialog().is_some() {
            return glib::Propagation::Proceed;
        }
        if let Some(w) = gtk::prelude::GtkWindowExt::focus(&c.window) {
            if w.type_().is_a(gtk::Editable::static_type())
                || w.type_().is_a(gtk::TextView::static_type())
            {
                return glib::Propagation::Proceed;
            }
        }
        match shortcut_key(keyval) {
            Some(k) => {
                c.media_key(k);
                glib::Propagation::Stop
            }
            None => glib::Propagation::Proceed,
        }
    });
    ctx.window.add_controller(ctrl);
}
