//! Device page: the TVs section (several TVs, scan, add, rename, link, remove), then
//! for the TV in use: device info, power on (WoL / SmartThings) / off, connection, SSDP
//! rediscovery, and settings (host, MAC, SmartThings OAuth login / legacy token +
//! device link). Also keeps the SmartThings OAuth login fresh while the app runs.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use tvremote_core::smartthings::{SmartThings, StDevice};
use tvremote_core::smartthings_auth::{
    default_app_file, open_in_browser, random_state, CallbackListener, LOGIN_TIMEOUT, PASTE_REDIRECT,
};
use tvremote_core::{AuthStatus, ConnectionState, DeviceInfo, Error, Event, PowerState};

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
    st_status: adw::ActionRow,
    st_app: adw::ActionRow,
    st_connect: gtk::Button,
    st_disconnect: gtk::Button,
    /// The "log in again" toast was shown this session.
    st_warned: Cell<bool>,
    me: RefCell<std::rc::Weak<DevicePage>>,
}

/// Background refresh: when the refresh token is older than this (the daily timer may
/// not run, e.g. without systemd), the GUI refreshes it.
const GUI_REFRESH_AGE_SECS: u64 = 20 * 3600;

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
            ("API version", |d| d.version.clone()),
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
            .description("Picture mode and input source go through the SmartThings cloud; power, volume and mute fall back to it. One login for all TVs: import the OAuth app you created with the SmartThings CLI (see docs/smartthings-app.json), then connect.")
            .build();
        let st_status = adw::ActionRow::builder().title("Status").subtitle("…").build();
        st_status.add_css_class("property");
        let st_app = adw::ActionRow::builder()
            .title("OAuth app")
            .subtitle("Not imported")
            .build();
        let st_import = button("Import app file…");
        st_import.set_tooltip_text(Some("The output of `smartthings apps:create -o …` (st-app-created.json)"));
        st_app.add_suffix(&st_import);
        let st_account = adw::ActionRow::builder()
            .title("Account")
            .subtitle("Log in once; the access is refreshed automatically")
            .build();
        let st_connect = button("Connect SmartThings");
        st_connect.add_css_class("suggested-action");
        let st_disconnect = button("Disconnect");
        st_disconnect.add_css_class("destructive-action");
        st_account.add_suffix(&st_connect);
        st_account.add_suffix(&st_disconnect);
        let st_token = adw::PasswordEntryRow::builder().title("Personal access token").build();
        let st_pat = adw::ExpanderRow::builder()
            .title("Personal access token (legacy)")
            .subtitle("Used only when OAuth is not connected; tokens from account.smartthings.com/tokens expire after 24 h")
            .build();
        st_pat.add_row(&st_token);
        let st_device = adw::ComboRow::builder()
            .title("TV device")
            .subtitle("Automatic: matched by model and name")
            .build();
        st_device.set_model(Some(&gtk::StringList::new(&["Automatic"])));
        let find_dev = button("Find devices");
        find_dev.add_css_class("flat");
        st.set_header_suffix(Some(&find_dev));
        st.add(&st_status);
        st.add(&st_app);
        st.add(&st_account);
        st.add(&st_device);
        st.add(&st_pat);
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
            st_status,
            st_app,
            st_connect: st_connect.clone(),
            st_disconnect: st_disconnect.clone(),
            st_warned: Cell::new(false),
            me: RefCell::new(std::rc::Weak::new()),
        });
        *this.me.borrow_mut() = Rc::downgrade(&this);
        this.load_settings();

        let w = Rc::downgrade(&this);
        st_import.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.import_app();
            }
        });
        let w = Rc::downgrade(&this);
        st_connect.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.connect_smartthings();
            }
        });
        let w = Rc::downgrade(&this);
        st_disconnect.connect_clicked(move |_| {
            let Some(p) = w.upgrade() else { return };
            let Some(c) = p.ctx.upgrade() else { return };
            let w2 = Rc::downgrade(&p);
            c.confirm(
                "Disconnect SmartThings?",
                "The OAuth tokens are forgotten (the imported app stays). The personal access token, if any, is used instead.",
                "Disconnect",
                true,
                move || {
                    let Some(p) = w2.upgrade() else { return };
                    let Some(c) = p.ctx.upgrade() else { return };
                    let fleet = c.fleet.clone();
                    let w3 = Rc::downgrade(&p);
                    c.spawn(
                        move |_| {
                            let r = fleet.smartthings_auth().logout();
                            fleet.smartthings_auth_changed();
                            r
                        },
                        move |r| {
                            let Some(p) = w3.upgrade() else { return };
                            if let Some(c) = p.ctx.upgrade() {
                                c.toast(&match r {
                                    Ok(()) => "SmartThings disconnected".to_string(),
                                    Err(e) => format!("Disconnect: {e}"),
                                });
                            }
                            p.refresh_auth_status();
                        },
                    );
                },
            );
        });

        // Keep the OAuth login fresh while the app runs (the systemd timer does it daily
        // otherwise): soon after start, then every 30 minutes.
        let w = Rc::downgrade(&this);
        glib::timeout_add_seconds_local_once(15, move || {
            if let Some(p) = w.upgrade() {
                p.background_refresh();
            }
        });
        let w = Rc::downgrade(&this);
        glib::timeout_add_seconds_local(30 * 60, move || match w.upgrade() {
            Some(p) => {
                p.background_refresh();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });

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
        self.refresh_auth_status();
    }

    /// Read the OAuth state (worker thread) and update the SmartThings rows.
    fn refresh_auth_status(&self) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        let fleet = ctx.fleet.clone();
        let (status, app, connect, disconnect) =
            (self.st_status.clone(), self.st_app.clone(), self.st_connect.clone(), self.st_disconnect.clone());
        let page = self.self_weak();
        ctx.spawn(
            move |_| fleet.smartthings_auth().status(),
            move |st: AuthStatus| {
                if st.needs_login {
                    if let Some(p) = page.upgrade() {
                        p.warn_relogin();
                    }
                }
                status.set_subtitle(&st.summary());
                app.set_subtitle(&if st.app_imported {
                    format!("Imported{}", st.app_id.as_deref().map(|a| format!(" ({a})")).unwrap_or_default())
                } else {
                    "Not imported".into()
                });
                connect.set_visible(st.app_imported && (!st.connected || st.needs_login));
                connect.set_label(if st.needs_login { "Log in again" } else { "Connect SmartThings" });
                disconnect.set_visible(st.connected);
            },
        );
    }

    /// Toast (once per session) that the SmartThings login must be renewed.
    fn warn_relogin(&self) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        if !self.st_warned.replace(true) {
            ctx.toast_nav("SmartThings login expired: connect again", "Device page", "device");
        }
    }

    /// Refresh the OAuth tokens if the refresh token is older than 20 h; warn if the
    /// login expired.
    fn background_refresh(&self) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        let fleet = ctx.fleet.clone();
        let page = self.self_weak();
        ctx.spawn(
            move |_| {
                let auth = fleet.smartthings_auth();
                let st = auth.status();
                if !(st.connected && st.refresh_age.is_some_and(|a| a >= GUI_REFRESH_AGE_SECS)) {
                    return Ok(false);
                }
                let r = auth.refresh();
                if r.is_ok() || matches!(r, Err(Error::SmartThingsLoginExpired)) {
                    fleet.smartthings_auth_changed();
                }
                r.map(|()| true)
            },
            move |r| {
                let Some(p) = page.upgrade() else { return };
                match r {
                    Ok(_) => {}
                    Err(Error::SmartThingsLoginExpired) => p.warn_relogin(),
                    // Network trouble: the next attempt (30 min) or the next use retries.
                    Err(e) => eprintln!("SmartThings background refresh: {e}"),
                }
                p.refresh_auth_status();
            },
        );
    }

    /// "Import app file…": pick the `apps:create` output and store its credentials.
    fn import_app(&self) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        let dialog = gtk::FileDialog::builder()
            .title("SmartThings app file (st-app-created.json)")
            .modal(true)
            .build();
        let def = default_app_file();
        if def.exists() {
            dialog.set_initial_file(Some(&gio::File::for_path(&def)));
        } else if let Some(dir) = def.parent().filter(|d| d.exists()) {
            dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
        }
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("JSON"));
        filter.add_pattern("*.json");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        dialog.set_filters(Some(&filters));
        let w = ctx.weak();
        let page = self.self_weak();
        dialog.open(Some(&ctx.window), gio::Cancellable::NONE, move |res| {
            let Ok(file) = res else { return }; // cancelled
            let Some(path) = file.path() else { return };
            let Some(c) = w.upgrade() else { return };
            let fleet = c.fleet.clone();
            let w2 = c.weak();
            let page = page.clone();
            c.spawn(
                move |_| {
                    let r = fleet.smartthings_auth().import_app(&path);
                    fleet.smartthings_auth_changed();
                    r
                },
                move |r| {
                    let Some(c) = w2.upgrade() else { return };
                    match r {
                        Ok(app) => c.toast(&format!(
                            "Imported {}: now Connect SmartThings",
                            app.app_name.as_deref().unwrap_or("the SmartThings app")
                        )),
                        Err(e) => c.toast(&format!("Import: {e}")),
                    }
                    if let Some(p) = page.upgrade() {
                        p.refresh_auth_status();
                    }
                },
            );
        });
    }

    fn self_weak(&self) -> std::rc::Weak<DevicePage> {
        self.me.borrow().clone()
    }

    /// "Connect SmartThings": open the authorize URL, listen on 127.0.0.1:47811 on a
    /// worker thread, and show a dialog with the URL (copy button) and a paste field.
    fn connect_smartthings(&self) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        let auth = ctx.fleet.smartthings_auth();
        let state = random_state();
        // The localhost redirect gets "403 Access forbidden to this app" from
        // SmartThings on some accounts, so the GUI always uses the httpbin paste flow.
        let listener: Option<CallbackListener> = None;
        let redirect = listener.as_ref().map(|l| l.redirect_uri()).unwrap_or_else(|| PASTE_REDIRECT.to_string());
        let url = match auth.authorize_url(&state, &redirect) {
            Ok(u) => u,
            Err(e) => return ctx.toast(&e.to_string()),
        };
        let opened = open_in_browser(&url);

        // --- dialog -------------------------------------------------------------------
        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
        vbox.set_margin_top(12);
        vbox.set_margin_bottom(18);
        vbox.set_margin_start(18);
        vbox.set_margin_end(18);
        let intro = gtk::Label::new(Some(&format!(
            "{}Log in to your Samsung account, pick your location and approve TV Remote.",
            if opened { "Your browser opened the SmartThings login. " } else { "Open this address in a browser. " }
        )));
        intro.set_wrap(true);
        intro.set_xalign(0.0);
        vbox.append(&intro);
        let url_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let url_entry = gtk::Entry::new();
        url_entry.set_text(&url);
        url_entry.set_editable(false);
        url_entry.set_hexpand(true);
        let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
        copy.set_tooltip_text(Some("Copy the address"));
        url_box.append(&url_entry);
        url_box.append(&copy);
        vbox.append(&url_box);
        let hint = gtk::Label::new(Some(if listener.is_some() {
            "This window closes by itself once you approve. If the browser cannot reach localhost, paste the address it ended on (or just the code) here:"
        } else {
            "Port 47811 is busy, so after approving the browser lands on an httpbin.org page. Paste that page's address (or just the code) here:"
        }));
        hint.set_wrap(true);
        hint.set_xalign(0.0);
        hint.add_css_class("dim-label");
        vbox.append(&hint);
        let paste_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let paste = gtk::Entry::new();
        paste.set_placeholder_text(Some(if listener.is_some() {
            "http://localhost:47811/callback?code=…"
        } else {
            "https://httpbin.org/get?code=…"
        }));
        paste.set_hexpand(true);
        let use_btn = gtk::Button::with_label("Connect");
        use_btn.add_css_class("suggested-action");
        paste_box.append(&paste);
        paste_box.append(&use_btn);
        vbox.append(&paste_box);
        let spinner = adw::Spinner::new();
        spinner.set_visible(listener.is_some());
        vbox.append(&spinner);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&vbox));
        let dialog = adw::Dialog::builder()
            .title("Connect SmartThings")
            .content_width(560)
            .child(&toolbar)
            .build();

        let cancel = Arc::new(AtomicBool::new(false));
        let done = Rc::new(Cell::new(false));
        let page = self.self_weak();

        // Shared ending: first success wins; later results are ignored.
        let finish = {
            let dialog = dialog.clone();
            let done = done.clone();
            let cancel = cancel.clone();
            let w = ctx.weak();
            let page = page.clone();
            Rc::new(move |r: Result<(), Error>, from_paste: bool| {
                if done.get() {
                    return;
                }
                let Some(c) = w.upgrade() else { return };
                match r {
                    Ok(()) => {
                        done.set(true);
                        cancel.store(true, Ordering::SeqCst);
                        dialog.close();
                        c.toast("SmartThings connected");
                        if let Some(p) = page.upgrade() {
                            p.st_warned.set(false);
                            p.refresh_auth_status();
                        }
                    }
                    Err(Error::Cancelled) => {}
                    Err(e) => {
                        c.toast(&format!("SmartThings login: {e}"));
                        if !from_paste {
                            // Listener gave up (timeout, refusal): only pasting is left.
                            done.set(true);
                            dialog.close();
                        }
                    }
                }
            })
        };

        if let Some(l) = listener {
            let auth = auth.clone();
            let state = state.clone();
            let cancel = cancel.clone();
            let fleet = ctx.fleet.clone();
            let finish = finish.clone();
            ctx.spawn(
                move |_| {
                    let r = auth.login_with_listener(&l, &state, LOGIN_TIMEOUT, &cancel);
                    if r.is_ok() {
                        fleet.smartthings_auth_changed();
                    }
                    r
                },
                move |r| finish(r, false),
            );
        }

        let on_use = {
            let w = ctx.weak();
            let paste = paste.clone();
            let use_btn = use_btn.clone();
            let finish = finish.clone();
            move || {
                let Some(c) = w.upgrade() else { return };
                let text = paste.text().to_string();
                if text.trim().is_empty() {
                    return;
                }
                use_btn.set_sensitive(false);
                let auth = auth.clone();
                let state = state.clone();
                let redirect = redirect.clone();
                let fleet = c.fleet.clone();
                let finish = finish.clone();
                let use_btn = use_btn.clone();
                c.spawn(
                    move |_| {
                        let r = auth.login_with_input(&text, Some(&state), &redirect);
                        if r.is_ok() {
                            fleet.smartthings_auth_changed();
                        }
                        r
                    },
                    move |r| {
                        use_btn.set_sensitive(true);
                        finish(r, true);
                    },
                );
            }
        };
        let on_use = Rc::new(on_use);
        let f = on_use.clone();
        use_btn.connect_clicked(move |_| f());
        let f = on_use.clone();
        paste.connect_activate(move |_| f());
        let u = url.clone();
        copy.connect_clicked(move |b| {
            b.clipboard().set_text(&u);
        });
        let cancel2 = cancel.clone();
        dialog.connect_closed(move |_| {
            cancel2.store(true, Ordering::SeqCst);
        });
        dialog.present(Some(&ctx.window));
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
        // A token typed but not saved yet is tried as is; otherwise the account's
        // client (OAuth if connected, else the saved token).
        let typed = non_empty(&self.st_token.text()).filter(|t| Some(t) != ctx.fleet.config().smartthings_token.as_ref());
        if typed.is_none() && !ctx.fleet.has_smartthings() {
            ctx.toast("Connect SmartThings (or paste a personal access token) first");
            return;
        }
        let fleet = ctx.fleet.clone();
        let combo = self.st_device.clone();
        let w = ctx.weak();
        let store = self.st_devices.clone();
        ctx.spawn(
            move |_tv| match typed {
                Some(token) => SmartThings::new(&token).and_then(|st| st.devices()),
                None => fleet.smartthings().and_then(|st| st.devices()),
            },
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
                    // Another account (unless OAuth, which wins, is connected): every link is stale.
                    let oauth = fleet.config().oauth_connected();
                    fleet.shared_config().update_app(|a| {
                        if oauth {
                            return;
                        }
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
