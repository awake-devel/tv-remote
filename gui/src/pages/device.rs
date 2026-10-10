//! Device page: the TVs section (several TVs, scan, add, rename, link, remove), then
//! for the TV in use: device info, power on (WoL / SmartThings) / off, connection, SSDP
//! rediscovery, and settings (host, MAC, SmartThings token + device link).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use tvremote_core::smartthings::{SmartThings, StDevice};
use tvremote_core::{ConnectionState, DeviceInfo, Event, PowerState};

use crate::ctx::Ctx;
use crate::format::{connection_text, non_empty, power_text};
use crate::pages::tvs::TvsSection;

type InfoField = fn(&DeviceInfo) -> Option<String>;

pub struct DevicePage {
    pub widget: gtk::Widget,
    ctx: std::rc::Weak<Ctx>,
    tvs: Rc<TvsSection>,
    info_group: adw::PreferencesGroup,
    conn_row: adw::ActionRow,
    power_row: adw::ActionRow,
    info_rows: Vec<(adw::ActionRow, InfoField)>,
    info_refresh: gtk::Button,
    host: adw::EntryRow,
    mac: adw::EntryRow,
    st_token: adw::PasswordEntryRow,
    st_device: adw::ComboRow,
    st_devices: Rc<RefCell<Vec<StDevice>>>,
}

/// The user is typing in this row (don't overwrite it).
fn editing(w: &impl IsA<gtk::Widget>) -> bool {
    w.state_flags().contains(gtk::StateFlags::FOCUS_WITHIN)
}

fn info_row(title: &str) -> adw::ActionRow {
    let r = adw::ActionRow::builder().title(title).subtitle("—").build();
    r.add_css_class("property");
    r.set_subtitle_selectable(true);
    r
}

fn button(label: &str) -> gtk::Button {
    let b = gtk::Button::with_label(label);
    b.set_valign(gtk::Align::Center);
    b
}

impl DevicePage {
    pub fn new(ctx: &Rc<Ctx>) -> Rc<DevicePage> {
        let page = adw::PreferencesPage::new();
        let tvs = TvsSection::new(ctx, &page);

        // --- status / info ------------------------------------------------------------------
        let info = adw::PreferencesGroup::builder().title("This TV").build();
        let info_refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        info_refresh.add_css_class("flat");
        info_refresh.set_tooltip_text(Some("Read the device info from the TV"));
        info.set_header_suffix(Some(&info_refresh));
        let conn_row = info_row("Remote connection");
        conn_row.set_subtitle(&connection_text(&ConnectionState::Disconnected));
        let power_row = info_row("Power");
        info.add(&conn_row);
        info.add(&power_row);
        let fields: [(&str, InfoField); 8] = [
            ("Name", |d| d.name.clone()),
            ("Model", |d| match (&d.model_name, &d.model) {
                (Some(a), Some(b)) => Some(format!("{a} ({b})")),
                (a, b) => a.clone().or_else(|| b.clone()),
            }),
            ("Resolution", |d| d.resolution.clone()),
            ("Network", |d| d.network_type.clone()),
            ("IP address", |d| d.ip.clone()),
            ("Wi-Fi MAC", |d| d.wifi_mac.clone()),
            ("OS", |d| d.os.clone()),
            ("Firmware", |d| d.version.clone()),
        ];
        let mut info_rows = Vec::new();
        for (t, f) in fields {
            let r = info_row(t);
            info.add(&r);
            info_rows.push((r, f));
        }
        page.add(&info);

        // --- power ------------------------------------------------------------------------
        let power = adw::PreferencesGroup::builder().title("Power").build();
        let on_row = adw::ActionRow::builder()
            .title("Turn on")
            .subtitle("Wake-on-LAN (may need “Power On with Mobile” enabled on the TV), plus SmartThings if linked")
            .build();
        let on_btn = button("Turn on");
        on_btn.add_css_class("suggested-action");
        on_row.add_suffix(&on_btn);
        let off_row = adw::ActionRow::builder().title("Turn off").build();
        let off_btn = button("Turn off");
        off_btn.add_css_class("destructive-action");
        off_row.add_suffix(&off_btn);
        power.add(&on_row);
        power.add(&off_row);
        page.add(&power);

        // --- connection ---------------------------------------------------------------------
        let conn = adw::PreferencesGroup::builder().title("Connection").build();
        let reconnect_row = adw::ActionRow::builder()
            .title("Reconnect the remote channel")
            .build();
        let reconnect_btn = button("Reconnect");
        reconnect_row.add_suffix(&reconnect_btn);
        let find_row = adw::ActionRow::builder()
            .title("Find the TV on the network")
            .subtitle("SSDP search; updates the address if the TV's IP changed")
            .build();
        let find_btn = button("Rediscover");
        find_row.add_suffix(&find_btn);
        conn.add(&reconnect_row);
        conn.add(&find_row);
        page.add(&conn);

        // --- settings -----------------------------------------------------------------------
        let settings = adw::PreferencesGroup::builder()
            .title("Settings of this TV")
            .description("Saved to ~/.config/samsung-tv/config.json")
            .build();
        let host = adw::EntryRow::builder().title("TV address (IP or host name)").build();
        let mac = adw::EntryRow::builder().title("TV MAC address (Wake-on-LAN)").build();
        settings.add(&host);
        settings.add(&mac);
        page.add(&settings);

        let st = adw::PreferencesGroup::builder()
            .title("SmartThings (optional)")
            .description("Picture mode and input source go through the SmartThings cloud; power, volume and mute fall back to it. Create a personal access token at account.smartthings.com/tokens and paste it here (one token for all TVs).")
            .build();
        let st_token = adw::PasswordEntryRow::builder().title("Personal access token").build();
        let st_device = adw::ComboRow::builder()
            .title("TV device")
            .subtitle("Automatic: matched by model and name")
            .build();
        st_device.set_model(Some(&gtk::StringList::new(&["Automatic"])));
        let find_dev = button("Find devices");
        find_dev.add_css_class("flat");
        st.set_header_suffix(Some(&find_dev));
        st.add(&st_token);
        st.add(&st_device);
        page.add(&st);

        let save_group = adw::PreferencesGroup::new();
        let save = gtk::Button::with_label("Save settings");
        save.add_css_class("pill");
        save.add_css_class("suggested-action");
        save.set_halign(gtk::Align::Center);
        save_group.add(&save);
        page.add(&save_group);

        let this = Rc::new(DevicePage {
            widget: page.upcast(),
            ctx: ctx.weak(),
            tvs,
            info_group: info.clone(),
            conn_row,
            power_row,
            info_rows,
            info_refresh: info_refresh.clone(),
            host,
            mac,
            st_token,
            st_device,
            st_devices: Rc::new(RefCell::new(Vec::new())),
        });
        this.load_settings();

        let w = Rc::downgrade(&this);
        info_refresh.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.refresh_info();
            }
        });

        let wctx = ctx.weak();
        on_btn.connect_clicked(move |b| {
            let Some(c) = wctx.upgrade() else { return };
            b.set_sensitive(false);
            c.toast("Turning the TV on…");
            let b = b.clone();
            let w2 = c.weak();
            c.spawn(
                |tv| tv.power_on(Duration::from_secs(20)),
                move |res| {
                    b.set_sensitive(true);
                    if let Some(c) = w2.upgrade() {
                        match res {
                            Ok(()) => c.toast("The TV is on"),
                            Err(e) => c.toast(&format!("Power on: {e}")),
                        }
                    }
                },
            );
        });
        let wctx = ctx.weak();
        off_btn.connect_clicked(move |_| {
            let Some(c) = wctx.upgrade() else { return };
            let w2 = c.weak();
            c.spawn(
                |tv| tv.power_off(),
                move |res| {
                    if let (Some(c), Err(e)) = (w2.upgrade(), res) {
                        c.toast(&format!("Power off: {e}"));
                    }
                },
            );
        });
        let wctx = ctx.weak();
        reconnect_btn.connect_clicked(move |_| {
            let Some(c) = wctx.upgrade() else { return };
            c.spawn(
                |tv| {
                    tv.disconnect();
                    tv.connect();
                },
                |_| {},
            );
        });
        let wctx = ctx.weak();
        find_btn.connect_clicked(move |b| {
            let Some(c) = wctx.upgrade() else { return };
            b.set_sensitive(false);
            let b = b.clone();
            let w2 = c.weak();
            c.spawn(
                |tv| tv.rediscover(Duration::from_secs(5)),
                move |res| {
                    b.set_sensitive(true);
                    let Some(c) = w2.upgrade() else { return };
                    match res {
                        Ok(Some(ip)) => c.toast(&format!("TV found at {ip}")),
                        Ok(None) => c.toast("No Samsung TV answered on this network"),
                        Err(e) => c.toast(&format!("Discovery: {e}")),
                    }
                },
            );
        });
        let w = Rc::downgrade(&this);
        find_dev.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.find_devices();
            }
        });
        let w = Rc::downgrade(&this);
        save.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.save();
            }
        });
        this
    }

    /// Fill the settings fields from the current config.
    fn load_settings(&self) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        let cfg = ctx.tv().config();
        self.info_group.set_description(Some(&if cfg.display_name.is_empty() {
            "No TV configured yet".to_string()
        } else {
            format!("{} ({})", cfg.display_name, cfg.id)
        }));
        if !editing(&self.host) {
            self.host.set_text(&cfg.host);
        }
        if !editing(&self.mac) {
            self.mac.set_text(cfg.mac.as_deref().unwrap_or(""));
        }
        if !editing(&self.st_token) {
            self.st_token
                .set_text(cfg.smartthings_token.as_deref().unwrap_or(""));
        }
        let sub = match &cfg.smartthings_device_id {
            Some(id) => format!("Saved device: {id}"),
            None => "Automatic: matched by model and name".into(),
        };
        self.st_device.set_subtitle(&sub);
    }

    pub fn on_shown(&self) {
        if self.info_rows.first().is_some_and(|(r, _)| r.subtitle().as_deref() == Some("—")) {
            self.refresh_info();
        }
    }

    pub fn refresh_info(&self) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        self.info_refresh.set_sensitive(false);
        let rows: Vec<_> = self.info_rows.clone();
        let btn = self.info_refresh.clone();
        let power_row = self.power_row.clone();
        let w = ctx.weak();
        let for_tv = ctx.tv().id();
        ctx.spawn(
            |tv| tv.device_info(),
            move |res| {
                btn.set_sensitive(true);
                let Some(c) = w.upgrade() else { return };
                if c.tv().id() != for_tv {
                    return; // answer for the previous TV
                }
                match res {
                    Ok(info) => {
                        for (r, f) in rows.iter() {
                            r.set_subtitle(&f(&info).unwrap_or_else(|| "—".into()));
                        }
                        if let Some(p) = &info.power_state {
                            power_row.set_subtitle(&power_text(p));
                        }
                        *c.device_info.borrow_mut() = Some(info);
                    }
                    Err(e) => c.toast(&format!("Device info: {e}")),
                }
            },
        );
    }

    fn find_devices(&self) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        let Some(token) = non_empty(&self.st_token.text()) else {
            ctx.toast("Paste a SmartThings token first");
            return;
        };
        let combo = self.st_device.clone();
        let w = ctx.weak();
        let store = self.st_devices.clone();
        ctx.spawn(
            move |_tv| SmartThings::new(&token).and_then(|st| st.devices()),
            move |res| {
                let Some(c) = w.upgrade() else { return };
                match res {
                    Ok(devs) => {
                        let mut names: Vec<String> = vec!["Automatic".into()];
                        names.extend(devs.iter().map(|d| {
                            let kind = d.ocf_device_type.as_deref().unwrap_or("");
                            if kind.contains("tv") || kind.contains("TV") {
                                format!("📺 {}", d.label)
                            } else {
                                d.label.clone()
                            }
                        }));
                        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
                        combo.set_model(Some(&gtk::StringList::new(&refs)));
                        let saved = c.tv().config().smartthings_device_id;
                        let pos = saved
                            .and_then(|id| devs.iter().position(|d| d.device_id == id))
                            .map(|p| p + 1)
                            .unwrap_or(0);
                        combo.set_selected(pos as u32);
                        c.toast(&format!("{} SmartThings device(s) found", devs.len()));
                        *store.borrow_mut() = devs;
                    }
                    Err(e) => c.toast(&e.to_string()),
                }
            },
        );
    }

    fn save(&self) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        let host = self.host.text().trim().to_string();
        if host.is_empty() && ctx.tv().config().smartthings_device_id.is_none() {
            ctx.toast("The TV address cannot be empty (unless the TV is linked to SmartThings)");
            return;
        }
        let mac = non_empty(&self.mac.text());
        let token = non_empty(&self.st_token.text());
        let sel = self.st_device.selected() as usize;
        let shared = self.st_devices.clone();
        let picked = if sel >= 1 {
            shared.borrow().get(sel - 1).map(|d| d.device_id.clone())
        } else {
            None
        };
        let devices_known = !shared.borrow().is_empty();
        let w = ctx.weak();
        let fleet = ctx.fleet.clone();
        ctx.spawn(
            move |tv| {
                let old_token = tv.config().smartthings_token;
                if old_token != token {
                    // Another account: every link is stale.
                    fleet.shared_config().update_app(|a| {
                        for e in a.tvs.iter_mut() {
                            e.smartthings_device_id = None;
                        }
                    })?;
                    fleet.set_smartthings_token(token)?;
                }
                tv.update_config(|c| {
                    c.host = host;
                    c.mac = mac;
                    if picked.is_none() && devices_known {
                        c.smartthings_device_id = None;
                    }
                })?;
                if let Some(id) = picked {
                    fleet.link_smartthings(&tv.id(), &id)?;
                }
                Ok::<_, tvremote_core::Error>(old_token)
            },
            move |res| {
                if let Some(c) = w.upgrade() {
                    match res {
                        Ok(_) => c.toast("Settings saved"),
                        Err(e) => c.toast(&format!("Could not save: {e}")),
                    }
                }
            },
        );
    }

    /// Another TV is in use: reset what belongs to the previous one.
    pub fn on_tv_changed(&self, visible: bool) {
        for (r, _) in &self.info_rows {
            r.set_subtitle("—");
        }
        let Some(ctx) = self.ctx.upgrade() else { return };
        let tv = ctx.tv();
        self.conn_row.set_subtitle(&connection_text(&tv.connection_state()));
        let p = ctx.dots.borrow().get(&tv.id()).and_then(|d| d.power.clone());
        self.power_row.set_subtitle(&p.map(|p| power_text(&p)).unwrap_or_else(|| "—".into()));
        *self.st_devices.borrow_mut() = Vec::new();
        self.st_device.set_model(Some(&gtk::StringList::new(&["Automatic"])));
        self.load_settings();
        self.tvs.rebuild();
        if visible {
            self.refresh_info();
        }
    }

    /// The TV list or a TV's status dots changed.
    pub fn on_fleet_changed(&self) {
        self.tvs.rebuild();
        self.load_settings();
    }

    pub fn handle_event(&self, ev: &Event) {
        match ev {
            Event::Connection(c) => {
                let mut t = connection_text(c);
                if let ConnectionState::Retrying(reason) = c {
                    t = format!("{t} ({reason})");
                }
                self.conn_row.set_subtitle(&t);
            }
            Event::Power(p) => {
                self.power_row.set_subtitle(&power_text(p));
                if *p == PowerState::On && self.info_rows.first().is_some_and(|(r, _)| r.subtitle().as_deref() == Some("—")) {
                    self.refresh_info();
                }
            }
            Event::ConfigChanged => self.load_settings(),
            _ => {}
        }
    }
}
