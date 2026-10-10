//! Main window: `AdwNavigationSplitView` with the sidebar pages, the TV switcher (sidebar
//! header), the header status indicator, the event pump and the keyboard shortcuts.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use tvremote_core::{AppConfig, ConnectionState, Event, Fleet, PowerState, TvEvent};

use crate::ctx::Ctx;
use crate::format::{connection_text, dots_markup, power_text, status_icon};
use crate::pages::{apps::AppsPage, cast::CastPage, device::DevicePage, picture::PicturePage, remote, touchpad::TouchpadPage};

const PAGES: [(&str, &str, &str); 6] = [
    ("remote", "Remote", "input-dialpad-symbolic"),
    ("touchpad", "Touchpad & Keyboard", "input-touchpad-symbolic"),
    ("apps", "Apps", "view-app-grid-symbolic"),
    ("cast", "Cast", "video-x-generic-symbolic"),
    ("picture", "Picture & Input", "preferences-desktop-display-symbolic"),
    ("device", "Device", "video-display-symbolic"),
];

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Open the config; never fails (falls back to defaults and reports why).
fn open_fleet() -> (Fleet, Option<String>) {
    match Fleet::open_default() {
        // A missing file opens empty (created on the first save).
        Ok(f) if f.entries().is_empty() => (
            f,
            Some("No TV yet: scan for TVs on the Device page".into()),
        ),
        Ok(f) => (f, None),
        // The file exists but is broken: core refuses to overwrite it, so run with an
        // in-memory config.
        Err(e) => (
            Fleet::new(AppConfig::default()),
            Some(format!("{e}. Settings will not be saved until the file is fixed.")),
        ),
    }
}

/// The TV switcher: a menu button with the current TV's name and status dots; its
/// popover lists every TV (click = use it) and links to the Device page.
struct Switcher {
    button: gtk::MenuButton,
    name: gtk::Label,
    dots: gtk::Label,
    list: gtk::ListBox,
    popover: gtk::Popover,
    manage: gtk::Button,
}

impl Switcher {
    fn new() -> Switcher {
        let name = gtk::Label::new(None);
        name.add_css_class("heading");
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let dots = gtk::Label::new(None);
        dots.set_use_markup(true);
        let bx = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bx.append(&name);
        bx.append(&dots);
        bx.append(&gtk::Image::from_icon_name("pan-down-symbolic"));
        let button = gtk::MenuButton::new();
        button.set_child(Some(&bx));
        button.add_css_class("flat");
        button.set_tooltip_text(Some("Switch TV (● LAN, ● SmartThings)"));
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("navigation-sidebar");
        let manage = gtk::Button::with_label("Manage TVs…");
        manage.add_css_class("flat");
        let vb = gtk::Box::new(gtk::Orientation::Vertical, 6);
        vb.append(&list);
        vb.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        vb.append(&manage);
        let popover = gtk::Popover::new();
        popover.set_child(Some(&vb));
        button.set_popover(Some(&popover));
        Switcher { button, name, dots, list, popover, manage }
    }

    /// Refresh the label and the list.
    fn update(&self, ctx: &Rc<Ctx>) {
        let tv = ctx.tv();
        let cur = tv.id();
        let entries = ctx.fleet.entries();
        let has_st = ctx.fleet.has_smartthings();
        let dots_of = |id: &str, linked: bool| {
            let d = ctx.dots.borrow().get(id).cloned().unwrap_or_default();
            dots_markup(d.power.as_ref(), has_st && linked, d.cloud.as_ref(), false)
        };
        let name = tv.display_name();
        self.name.set_text(if name.is_empty() { "No TV" } else { &name });
        self.dots.set_markup(&if cur.is_empty() {
            String::new()
        } else {
            dots_of(&cur, tv.config().smartthings_device_id.is_some())
        });
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        for e in entries {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let check = gtk::Image::from_icon_name("object-select-symbolic");
            check.set_opacity(if e.id == cur { 1.0 } else { 0.0 });
            row.append(&check);
            let l = gtk::Label::new(Some(&e.display_name()));
            l.set_xalign(0.0);
            l.set_hexpand(true);
            row.append(&l);
            let dl = gtk::Label::new(None);
            dl.set_markup(&dots_of(&e.id, e.smartthings_device_id.is_some()));
            row.append(&dl);
            let b = gtk::Button::new();
            b.set_child(Some(&row));
            b.add_css_class("flat");
            let w = ctx.weak();
            let pop = self.popover.clone();
            let id = e.id.clone();
            b.connect_clicked(move |_| {
                pop.popdown();
                if let Some(c) = w.upgrade() {
                    c.switch_tv(&id);
                }
            });
            self.list.append(&b);
        }
    }
}

pub fn build(app: &adw::Application, start_page: Option<&str>) {
    load_css();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("TV Remote")
        .default_width(1180)
        .default_height(820)
        .width_request(360)
        .height_request(420)
        .build();
    let toasts = adw::ToastOverlay::new();
    let (fleet, open_err) = open_fleet();
    let ctx = Ctx::new(fleet, window.clone(), toasts.clone());
    if let Some(e) = open_err {
        ctx.toast_nav(&e, "Settings", "device");
    }

    // --- pages ------------------------------------------------------------------------------
    let remote_w = remote::build(&ctx);
    let touch = TouchpadPage::new(&ctx);
    let apps = AppsPage::new(&ctx);
    let cast = CastPage::new(&ctx);
    let picture = PicturePage::new(&ctx);
    let device = DevicePage::new(&ctx);

    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.add_named(&remote_w, Some("remote"));
    stack.add_named(&touch.widget, Some("touchpad"));
    stack.add_named(&apps.widget, Some("apps"));
    stack.add_named(&cast.widget, Some("cast"));
    stack.add_named(&picture.widget, Some("picture"));
    stack.add_named(&device.widget, Some("device"));

    // --- sidebar ----------------------------------------------------------------------------
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    for (id, title, icon) in PAGES {
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        b.append(&gtk::Image::from_icon_name(icon));
        let l = gtk::Label::new(Some(title));
        l.set_xalign(0.0);
        b.append(&l);
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&b));
        row.set_widget_name(id);
        list.append(&row);
    }
    let sidebar_tv = adw::ToolbarView::new();
    let sidebar_header = adw::HeaderBar::new();
    let switcher = Rc::new(Switcher::new());
    sidebar_header.set_title_widget(Some(&switcher.button));
    switcher.update(&ctx);
    {
        // "Manage TVs…" → Device page.
        let wctx = ctx.weak();
        let pop = switcher.popover.clone();
        switcher.manage.connect_clicked(move |_| {
            pop.popdown();
            if let Some(c) = wctx.upgrade() {
                c.navigate("device");
            }
        });
        let sw = switcher.clone();
        let wctx = ctx.weak();
        switcher.popover.connect_show(move |_| {
            if let Some(c) = wctx.upgrade() {
                sw.update(&c);
            }
        });
    }
    sidebar_tv.add_top_bar(&sidebar_header);
    sidebar_tv.set_content(Some(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .build(),
    ));
    let sidebar_page = adw::NavigationPage::new(&sidebar_tv, "TV Remote");

    // --- content + header status ------------------------------------------------------------
    let content_tv = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("Remote", "Disconnected");
    header.set_title_widget(Some(&title));
    let status_icon_w = gtk::Image::from_icon_name("network-wireless-offline-symbolic");
    let status_label = gtk::Label::new(Some("Disconnected"));
    let status_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    status_box.append(&status_icon_w);
    status_box.append(&status_label);
    let status_btn = gtk::Button::new();
    status_btn.set_child(Some(&status_box));
    status_btn.add_css_class("flat");
    status_btn.add_css_class("tv-status");
    status_btn.set_tooltip_text(Some("Connection and power: open the Device page"));
    header.pack_end(&status_btn);
    content_tv.add_top_bar(&header);
    content_tv.set_content(Some(&stack));
    let content_page = adw::NavigationPage::new(&content_tv, "Remote");

    let split = adw::NavigationSplitView::new();
    split.set_sidebar(Some(&sidebar_page));
    split.set_content(Some(&content_page));
    split.set_min_sidebar_width(200.0);
    split.set_max_sidebar_width(260.0);
    toasts.set_child(Some(&split));
    window.set_content(Some(&toasts));

    let bp = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        720.0,
        adw::LengthUnit::Sp,
    ));
    bp.add_setter(&split, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(bp);

    // --- navigation -------------------------------------------------------------------------
    {
        let stack = stack.clone();
        let title = title.clone();
        let content_page = content_page.clone();
        let split = split.clone();
        let apps = apps.clone();
        let picture = picture.clone();
        let device = device.clone();
        list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let id = row.widget_name();
            let Some((_, t, _)) = PAGES.iter().find(|(i, _, _)| *i == id.as_str()) else { return };
            stack.set_visible_child_name(id.as_str());
            title.set_title(t);
            content_page.set_title(t);
            split.set_show_content(true);
            match id.as_str() {
                "apps" => apps.on_shown(),
                "picture" => picture.on_shown(),
                "device" => device.on_shown(),
                _ => {}
            }
        });
    }
    {
        let list = list.clone();
        let split = split.clone();
        ctx.set_navigator(Rc::new(move |page: &str| {
            let mut child = list.first_child();
            while let Some(w) = child {
                if let Ok(row) = w.clone().downcast::<gtk::ListBoxRow>() {
                    if row.widget_name() == page {
                        list.select_row(Some(&row));
                        split.set_show_content(true);
                        return;
                    }
                }
                child = w.next_sibling();
            }
        }));
    }
    if let Some(first) = list.row_at_index(0) {
        list.select_row(Some(&first));
    }
    let wctx = ctx.weak();
    status_btn.connect_clicked(move |_| {
        if let Some(c) = wctx.upgrade() {
            c.navigate("device");
        }
    });

    // --- shortcuts --------------------------------------------------------------------------
    {
        let stack = stack.clone();
        remote::install_shortcuts(&ctx, move || {
            matches!(stack.visible_child_name().as_deref(), Some("remote") | Some("touchpad"))
        });
    }

    // --- header status driven by core events ------------------------------------------------
    let conn_state: Rc<RefCell<ConnectionState>> = Rc::new(RefCell::new(ConnectionState::Disconnected));
    let power_state: Rc<RefCell<Option<PowerState>>> = Rc::new(RefCell::new(None));
    let update_status: Rc<dyn Fn()> = {
        let conn_state = conn_state.clone();
        let power_state = power_state.clone();
        let title = title.clone();
        Rc::new(move || {
            let c = conn_state.borrow();
            let p = power_state.borrow();
            let mut text = connection_text(&c);
            if let Some(p) = p.as_ref() {
                text = format!("{text} · {}", power_text(p));
            }
            let (icon, css) = status_icon(&c, p.as_ref());
            status_icon_w.set_icon_name(Some(icon));
            for cl in ["success", "warning", "dim-label"] {
                status_btn.remove_css_class(cl);
            }
            status_btn.add_css_class(css);
            status_label.set_text(&match p.as_ref() {
                Some(p) => power_text(p),
                None => connection_text(&c),
            });
            title.set_subtitle(&text);
        })
    };
    update_status();

    // --- switching TV rebinds every page ----------------------------------------------------
    {
        let wctx = ctx.weak();
        let stack = stack.clone();
        let (apps, picture, device, cast) = (apps.clone(), picture.clone(), device.clone(), cast.clone());
        let (conn_state, power_state, update_status) = (conn_state.clone(), power_state.clone(), update_status.clone());
        let switcher = switcher.clone();
        ctx.on_tv_changed(move || {
            let Some(c) = wctx.upgrade() else { return };
            let tv = c.tv();
            *conn_state.borrow_mut() = tv.connection_state();
            *power_state.borrow_mut() = c.dots.borrow().get(&tv.id()).and_then(|d| d.power.clone());
            update_status();
            switcher.update(&c);
            let visible = stack.visible_child_name();
            let shown = |p: &str| visible.as_deref() == Some(p);
            apps.on_tv_changed(shown("apps"));
            picture.on_tv_changed(shown("picture"));
            device.on_tv_changed(shown("device"));
            cast.on_tv_changed();
            c.toast(&format!("Now controlling {}", tv.display_name()));
        });
    }

    // --- event pump: core bus → async channel → UI -------------------------------------------
    let rx = ctx.fleet.subscribe();
    let (tx, arx) = async_channel::unbounded::<TvEvent>();
    let pump = std::thread::Builder::new().name("tv-event-pump".into()).spawn(move || {
        for ev in rx {
            if tx.send_blocking(ev).is_err() {
                break;
            }
        }
    });
    if let Err(e) = pump {
        ctx.toast(&format!("Cannot start the event thread: {e}"));
    }
    {
        let wctx = ctx.weak();
        let stack = stack.clone();
        let touch = touch.clone();
        let cast = cast.clone();
        let device = device.clone();
        let picture = picture.clone();
        let ime_hint_shown = Rc::new(Cell::new(false));
        let switcher = switcher.clone();
        glib::spawn_future_local(async move {
            while let Ok(te) = arx.recv().await {
                let Some(c) = wctx.upgrade() else { break };
                let ev = te.event;
                // Status dots of every TV.
                let fleet_view_changed = match &ev {
                    Event::Power(p) => {
                        c.dots.borrow_mut().entry(te.tv.clone()).or_default().power = Some(p.clone());
                        true
                    }
                    Event::Cloud(s) => {
                        c.dots.borrow_mut().entry(te.tv.clone()).or_default().cloud = Some(s.clone());
                        true
                    }
                    Event::FleetChanged | Event::ConfigChanged => true,
                    _ => false,
                };
                if fleet_view_changed {
                    // First run: the first TV now exists; bind to its own handle.
                    if c.fleet.is_placeholder(&c.tv()) {
                        if let Some(a) = c.fleet.active_id() {
                            c.switch_tv(&a);
                        }
                    }
                    switcher.update(&c);
                    device.on_fleet_changed();
                }
                // Casts are followed whatever TV they play on.
                cast.handle_event(&te.tv, &ev);
                if te.tv != c.tv().id() {
                    if let Event::Connection(ConnectionState::Unauthorized) = ev {
                        let name = c.fleet.tv(&te.tv).map(|t| t.display_name()).unwrap_or(te.tv);
                        c.toast_for(&format!("{name} refused the connection: accept the prompt on that TV"), 8);
                    }
                    continue;
                }
                match &ev {
                    Event::Connection(s) => {
                        *conn_state.borrow_mut() = s.clone();
                        update_status();
                        if *s == ConnectionState::Unauthorized {
                            c.toast_for("The TV refused the connection: accept the prompt on the TV", 8);
                        }
                    }
                    Event::Power(p) => {
                        *power_state.borrow_mut() = Some(p.clone());
                        update_status();
                    }
                    Event::ConfigChanged => picture.on_config_changed(),
                    Event::RemoteEvent { name, .. } if name == "ms.remote.imeStart" => {
                        if stack.visible_child_name().as_deref() == Some("touchpad") {
                            touch.focus_entry();
                        } else if !ime_hint_shown.replace(true) {
                            c.toast_nav("The TV is waiting for text", "Type", "touchpad");
                        }
                    }
                    Event::RemoteEvent { name, .. } if name == "ms.remote.imeEnd" => {
                        ime_hint_shown.set(false);
                    }
                    _ => {}
                }
                device.handle_event(&ev);
            }
        });
    }

    // --- start background activity ------------------------------------------------------------
    // REST power of every TV every 5 s and SmartThings health every minute (status dots);
    // only the TV in use opens its remote channel.
    let fleet = ctx.fleet.clone();
    ctx.spawn(
        move |tv| {
            fleet.start_status_polling(Duration::from_secs(5), Duration::from_secs(60));
            if !tv.host().trim().is_empty() {
                tv.connect();
            }
        },
        |_| {},
    );

    // --- closing while casting stops the stream: ask first --------------------------------------
    let allow_close = Rc::new(Cell::new(false));
    {
        let wctx = ctx.weak();
        let cast = cast.clone();
        window.connect_close_request(move |win| {
            if allow_close.get() || !cast.is_casting() {
                return glib::Propagation::Proceed;
            }
            let Some(c) = wctx.upgrade() else { return glib::Propagation::Proceed };
            let allow = allow_close.clone();
            let win = win.clone();
            let w2 = c.weak();
            c.confirm(
                "Stop casting and quit?",
                "The stream comes from this app: closing it stops playback on the TV.",
                "Stop and quit",
                true,
                move || {
                    let Some(c) = w2.upgrade() else { return };
                    let allow = allow.clone();
                    let win = win.clone();
                    let fleet = c.fleet.clone();
                    c.spawn(
                        move |tv| {
                            // A cast may run on any TV (it stays on the TV it started on).
                            let mut tvs: Vec<_> = fleet.ids().iter().filter_map(|id| fleet.tv(id).ok()).collect();
                            tvs.push(tv);
                            for t in tvs {
                                if let Some(s) = t.current_cast() {
                                    let _ = s.stop();
                                }
                            }
                        },
                        move |_| {
                            allow.set(true);
                            win.close();
                        },
                    );
                },
            );
            glib::Propagation::Stop
        });
    }

    // Every page holds only weak references to the context, so the window owns the one
    // strong reference: without it the Ctx (and the Tv with its connection) would be
    // dropped as soon as this function returns.
    let keep_alive = RefCell::new(Some(ctx.clone()));
    window.connect_destroy(move |_| {
        keep_alive.borrow_mut().take();
    });

    if let Some(page) = start_page {
        ctx.navigate(page);
    }
    window.present();
}
