//! Picture & Input page (SmartThings cloud). Shows a status page until a token is set
//! in Device settings; then offers only what the TV's capabilities report.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use tvremote_core::smartthings::{StDevice, TvStatus};

use crate::ctx::Ctx;
use crate::format::non_empty;

const CAP_INPUT: [&str; 2] = ["samsungvd.mediaInputSource", "mediaInputSource"];
const CAP_VOLUME: &str = "audioVolume";
const CAP_MUTE: &str = "audioMute";
const CAP_CHANNEL: &str = "tvChannel";
const CAP_SWITCH: &str = "switch";
/// The volume is sent once it has not changed for this long (one cloud command per
/// adjustment, not per step).
const VOLUME_DEBOUNCE: Duration = Duration::from_millis(300);

struct Loaded {
    dev: StDevice,
    status: TvStatus,
}

pub struct PicturePage {
    pub widget: gtk::Widget,
    ctx: std::rc::Weak<Ctx>,
    stack: gtk::Stack,
    dev_row: adw::ActionRow,
    reload: gtk::Button,
    picture: adw::ComboRow,
    sound: adw::ComboRow,
    input: adw::ComboRow,
    volume: adw::SpinRow,
    mute: adw::SwitchRow,
    power: adw::SwitchRow,
    channel_row: adw::ActionRow,
    channel_entry: adw::EntryRow,
    controls: adw::PreferencesGroup,
    loaded: Rc<RefCell<Option<Loaded>>>,
    /// True while widgets are updated from code (ignore their change signals).
    updating: Rc<Cell<bool>>,
    /// Pending debounced volume command.
    volume_timer: RefCell<Option<glib::SourceId>>,
}

impl PicturePage {
    pub fn new(ctx: &Rc<Ctx>) -> Rc<PicturePage> {
        let stack = gtk::Stack::new();

        let empty = adw::StatusPage::builder()
            .icon_name("preferences-desktop-display-symbolic")
            .title("SmartThings not set up")
            .description("Picture mode and input source are only available through the SmartThings cloud. Connect SmartThings on the Device page (or paste a personal access token there) to enable this page.")
            .build();
        let go = gtk::Button::with_label("Open Device settings");
        go.add_css_class("pill");
        go.add_css_class("suggested-action");
        go.set_halign(gtk::Align::Center);
        empty.set_child(Some(&go));
        let wctx = ctx.weak();
        go.connect_clicked(move |_| {
            if let Some(c) = wctx.upgrade() {
                c.navigate("device");
            }
        });
        stack.add_named(&empty, Some("empty"));

        let page = adw::PreferencesPage::new();
        let head = adw::PreferencesGroup::builder().title("SmartThings").build();
        let dev_row = adw::ActionRow::builder()
            .title("TV device")
            .subtitle("Not loaded yet")
            .build();
        let reload = gtk::Button::from_icon_name("view-refresh-symbolic");
        reload.set_valign(gtk::Align::Center);
        reload.add_css_class("flat");
        reload.set_tooltip_text(Some("Reload the TV status"));
        dev_row.add_suffix(&reload);
        head.add(&dev_row);
        page.add(&head);

        let controls = adw::PreferencesGroup::builder().title("Picture and input").build();
        let picture = adw::ComboRow::builder().title("Picture mode").build();
        let sound = adw::ComboRow::builder().title("Sound mode").build();
        let input = adw::ComboRow::builder().title("Input source").build();
        let power = adw::SwitchRow::builder().title("Power").build();
        let volume = adw::SpinRow::with_range(0.0, 100.0, 1.0);
        volume.set_title("Volume");
        let mute = adw::SwitchRow::builder().title("Mute").build();
        let channel_row = adw::ActionRow::builder().title("Channel").build();
        let ch_down = gtk::Button::from_icon_name("go-down-symbolic");
        let ch_up = gtk::Button::from_icon_name("go-up-symbolic");
        for b in [&ch_down, &ch_up] {
            b.set_valign(gtk::Align::Center);
            b.add_css_class("flat");
            channel_row.add_suffix(b);
        }
        let channel_entry = adw::EntryRow::builder()
            .title("Go to channel number")
            .show_apply_button(true)
            .build();
        for w in [
            picture.upcast_ref::<gtk::Widget>(),
            sound.upcast_ref(),
            input.upcast_ref(),
            power.upcast_ref(),
            volume.upcast_ref(),
            mute.upcast_ref(),
            channel_row.upcast_ref(),
            channel_entry.upcast_ref(),
        ] {
            w.set_visible(false);
            controls.add(w);
        }
        controls.set_visible(false);
        page.add(&controls);
        stack.add_named(&page, Some("content"));

        let this = Rc::new(PicturePage {
            widget: stack.clone().upcast(),
            ctx: ctx.weak(),
            stack,
            dev_row,
            reload: reload.clone(),
            picture,
            sound,
            input,
            volume,
            mute,
            power,
            channel_row,
            channel_entry,
            controls,
            loaded: Rc::new(RefCell::new(None)),
            updating: Rc::new(Cell::new(false)),
            volume_timer: RefCell::new(None),
        });

        let w = Rc::downgrade(&this);
        reload.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.load();
            }
        });

        // Picture mode: pass the selected id; core resolves it against the loaded map
        // (no extra status request).
        let w = Rc::downgrade(&this);
        this.picture.connect_selected_notify(move |row| {
            let Some(p) = w.upgrade() else { return };
            if p.updating.get() {
                return;
            }
            let sel = row.selected() as usize;
            let choice = p.loaded.borrow().as_ref().and_then(|l| {
                let modes = l.status.picture_modes.clone();
                modes.get(sel).map(|(id, _)| id.clone()).map(|id| (modes, id))
            });
            if let Some((modes, id)) = choice {
                p.command("Picture mode", move |st, dev| st.set_picture_mode_from(dev, &modes, &id));
            }
        });
        // Sound mode: same, with the sound-mode map.
        let w = Rc::downgrade(&this);
        this.sound.connect_selected_notify(move |row| {
            let Some(p) = w.upgrade() else { return };
            if p.updating.get() {
                return;
            }
            let sel = row.selected() as usize;
            let choice = p.loaded.borrow().as_ref().and_then(|l| {
                let modes = l.status.sound_modes.clone();
                modes.get(sel).map(|(id, _)| id.clone()).map(|id| (modes, id))
            });
            if let Some((modes, id)) = choice {
                p.command("Sound mode", move |st, dev| st.set_sound_mode_from(dev, &modes, &id));
            }
        });
        // Input: same, with the input-source map.
        let w = Rc::downgrade(&this);
        this.input.connect_selected_notify(move |row| {
            let Some(p) = w.upgrade() else { return };
            if p.updating.get() {
                return;
            }
            let sel = row.selected() as usize;
            let choice = p.loaded.borrow().as_ref().and_then(|l| {
                let srcs = l.status.input_sources.clone();
                srcs.get(sel).map(|(id, _)| id.clone()).map(|id| (srcs, id))
            });
            if let Some((srcs, id)) = choice {
                p.command("Input source", move |st, dev| st.set_input_source_from(dev, &srcs, &id));
            }
        });
        let w = Rc::downgrade(&this);
        this.power.connect_active_notify(move |row| {
            let Some(p) = w.upgrade() else { return };
            if p.updating.get() {
                return;
            }
            let on = row.is_active();
            p.command("Power", move |st, dev| st.set_power(dev, on));
        });
        let w = Rc::downgrade(&this);
        this.mute.connect_active_notify(move |row| {
            let Some(p) = w.upgrade() else { return };
            if p.updating.get() {
                return;
            }
            let m = row.is_active();
            p.command("Mute", move |st, dev| st.set_mute(dev, m));
        });
        let w = Rc::downgrade(&this);
        this.volume.connect_value_notify(move |_| {
            let Some(p) = w.upgrade() else { return };
            if p.updating.get() {
                return;
            }
            // Debounce: restart the timer on every change; send the last value once the
            // spin row has been still for VOLUME_DEBOUNCE.
            if let Some(id) = p.volume_timer.borrow_mut().take() {
                id.remove();
            }
            let w2 = Rc::downgrade(&p);
            let id = glib::timeout_add_local_once(VOLUME_DEBOUNCE, move || {
                let Some(p) = w2.upgrade() else { return };
                // The source has fired: forget its id so it is never removed twice.
                p.volume_timer.borrow_mut().take();
                let v = p.volume.value().clamp(0.0, 100.0).round() as u8;
                p.command("Volume", move |st, dev| st.set_volume(dev, v));
            });
            *p.volume_timer.borrow_mut() = Some(id);
        });
        let w = Rc::downgrade(&this);
        ch_up.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.command("Channel", |st, dev| st.channel_up(dev));
            }
        });
        let w = Rc::downgrade(&this);
        ch_down.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.command("Channel", |st, dev| st.channel_down(dev));
            }
        });
        let w = Rc::downgrade(&this);
        this.channel_entry.connect_apply(move |row| {
            let Some(p) = w.upgrade() else { return };
            if let Some(ch) = non_empty(&row.text()) {
                p.command("Channel", move |st, dev| st.set_channel(dev, &ch));
            }
        });

        this.sync_visibility();
        this
    }

    /// Show the status page or the controls depending on whether a token is set.
    pub fn sync_visibility(&self) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        if ctx.tv().has_smartthings() {
            self.stack.set_visible_child_name("content");
        } else {
            self.stack.set_visible_child_name("empty");
            *self.loaded.borrow_mut() = None;
            self.controls.set_visible(false);
            self.dev_row.set_subtitle("Not loaded yet");
        }
    }

    pub fn on_shown(&self) {
        self.sync_visibility();
        if self.stack.visible_child_name().as_deref() == Some("content") && self.loaded.borrow().is_none() {
            self.load();
        }
    }

    pub fn on_config_changed(&self) {
        let was_loaded = self.loaded.borrow().is_some();
        self.sync_visibility();
        // Token or device may have changed: reload on next show.
        if was_loaded {
            *self.loaded.borrow_mut() = None;
            self.dev_row.set_subtitle("Settings changed: press reload");
        }
    }

    /// Another TV is controlled now: drop the loaded state; reload at once if `visible`.
    pub fn on_tv_changed(&self, visible: bool) {
        *self.loaded.borrow_mut() = None;
        self.controls.set_visible(false);
        self.dev_row.set_subtitle("Not loaded yet");
        self.sync_visibility();
        if visible && self.stack.visible_child_name().as_deref() == Some("content") {
            self.load();
        }
    }

    pub fn load(&self) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        let for_tv = ctx.tv().id();
        self.reload.set_sensitive(false);
        self.dev_row.set_subtitle("Loading…");
        let loaded = self.loaded.clone();
        let reload = self.reload.clone();
        let dev_row = self.dev_row.clone();
        let rows = self.rows();
        let updating = self.updating.clone();
        let w = ctx.weak();
        ctx.spawn(
            |tv| -> tvremote_core::Result<Loaded> {
                let dev = tv.smartthings_tv()?;
                let st = tv.smartthings()?;
                let status = st.status(&dev.device_id)?;
                Ok(Loaded { dev, status })
            },
            move |res| {
                reload.set_sensitive(true);
                if w.upgrade().is_some_and(|c| c.tv().id() != for_tv) {
                    return; // answer for the previous TV
                }
                match res {
                    Ok(l) => {
                        dev_row.set_subtitle(&format!("{} ({})", l.dev.label, l.dev.name));
                        updating.set(true);
                        rows.apply(&l);
                        updating.set(false);
                        *loaded.borrow_mut() = Some(l);
                    }
                    Err(e) => {
                        dev_row.set_subtitle(&format!("Error: {e}"));
                        if let Some(c) = w.upgrade() {
                            c.toast(&e.to_string());
                        }
                    }
                }
            },
        );
    }

    fn rows(&self) -> Rows {
        Rows {
            controls: self.controls.clone(),
            picture: self.picture.clone(),
            sound: self.sound.clone(),
            input: self.input.clone(),
            volume: self.volume.clone(),
            mute: self.mute.clone(),
            power: self.power.clone(),
            channel_row: self.channel_row.clone(),
            channel_entry: self.channel_entry.clone(),
        }
    }

    /// Run a SmartThings command on a worker; toast on error.
    fn command<F>(&self, what: &'static str, f: F)
    where
        F: FnOnce(&tvremote_core::smartthings::SmartThings, &StDevice) -> tvremote_core::Result<()>
            + Send
            + 'static,
    {
        let Some(ctx) = self.ctx.upgrade() else { return };
        let Some(dev) = self.loaded.borrow().as_ref().map(|l| l.dev.clone()) else {
            ctx.toast("Load the TV status first");
            return;
        };
        let w = ctx.weak();
        ctx.spawn(
            move |tv| tv.smartthings().and_then(|st| f(&st, &dev)),
            move |res| {
                if let (Some(c), Err(e)) = (w.upgrade(), res) {
                    c.toast(&format!("{what}: {e}"));
                }
            },
        );
    }
}

struct Rows {
    controls: adw::PreferencesGroup,
    picture: adw::ComboRow,
    sound: adw::ComboRow,
    input: adw::ComboRow,
    volume: adw::SpinRow,
    mute: adw::SwitchRow,
    power: adw::SwitchRow,
    channel_row: adw::ActionRow,
    channel_entry: adw::EntryRow,
}

/// Index of `current` in `list` (matching either the id or the display name).
fn find_pair(list: &[(String, String)], current: Option<&str>) -> Option<usize> {
    let cur = current?;
    list.iter()
        .position(|(id, name)| id.eq_ignore_ascii_case(cur) || name.eq_ignore_ascii_case(cur))
}

impl Rows {
    fn apply(&self, l: &Loaded) {
        let has = |c: &str| l.dev.has(c);
        let s = &l.status;

        let show_pic =
            tvremote_core::smartthings::picture_cap(&l.dev).is_some() && !s.picture_modes.is_empty();
        if show_pic {
            let names: Vec<&str> = s.picture_modes.iter().map(|(_, n)| n.as_str()).collect();
            self.picture.set_model(Some(&gtk::StringList::new(&names)));
            if let Some(i) = find_pair(&s.picture_modes, s.picture_mode.as_deref()) {
                self.picture.set_selected(i as u32);
            }
        }
        self.picture.set_visible(show_pic);

        let show_sound = has(tvremote_core::smartthings::SOUND_CAP) && !s.sound_modes.is_empty();
        if show_sound {
            let names: Vec<&str> = s.sound_modes.iter().map(|(_, n)| n.as_str()).collect();
            self.sound.set_model(Some(&gtk::StringList::new(&names)));
            if let Some(i) = find_pair(&s.sound_modes, s.sound_mode.as_deref()) {
                self.sound.set_selected(i as u32);
            }
        }
        self.sound.set_visible(show_sound);

        let show_in = CAP_INPUT.iter().any(|c| has(c)) && !s.input_sources.is_empty();
        if show_in {
            let names: Vec<&str> = s.input_sources.iter().map(|(_, n)| n.as_str()).collect();
            self.input.set_model(Some(&gtk::StringList::new(&names)));
            if let Some(i) = find_pair(&s.input_sources, s.input_source.as_deref()) {
                self.input.set_selected(i as u32);
            }
        }
        self.input.set_visible(show_in);

        self.power.set_visible(has(CAP_SWITCH));
        self.power.set_active(s.power_on.unwrap_or(false));
        self.volume.set_visible(has(CAP_VOLUME));
        self.volume.set_value(f64::from(s.volume.unwrap_or(0)));
        self.mute.set_visible(has(CAP_MUTE));
        self.mute.set_active(s.muted.unwrap_or(false));
        let ch = has(CAP_CHANNEL);
        self.channel_row.set_visible(ch);
        self.channel_row
            .set_subtitle(s.channel.as_deref().unwrap_or(""));
        self.channel_entry.set_visible(ch);

        let any = show_pic || show_sound || show_in || ch || has(CAP_SWITCH) || has(CAP_VOLUME) || has(CAP_MUTE);
        self.controls.set_visible(true);
        self.controls.set_description(if any {
            // Verified 2026-10-10 on the 85" Q7F: SmartThings reports these commands as
            // COMPLETED but the TV keeps its mode (volume/mute/power do apply).
            Some("Some TVs (seen on the 2025 Q7F) accept picture and sound mode changes from SmartThings without applying them. Volume, mute and power are reliable.")
        } else {
            Some("This TV reports none of the supported SmartThings capabilities.")
        });
    }
}

#[cfg(test)]
mod tests {
    use super::find_pair;

    #[test]
    fn pairs() {
        let l = vec![
            ("HDMI1".to_string(), "PC".to_string()),
            ("dtv".to_string(), "TV".to_string()),
        ];
        assert_eq!(find_pair(&l, Some("hdmi1")), Some(0));
        assert_eq!(find_pair(&l, Some("TV")), Some(1));
        assert_eq!(find_pair(&l, Some("x")), None);
        assert_eq!(find_pair(&l, None), None);
    }
}
