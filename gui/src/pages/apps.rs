//! Apps page: installed apps grid (via the remote channel), YouTube via DIAL, Netflix.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use tvremote_core::App;

use crate::ctx::Ctx;
use crate::format::non_empty;

pub struct AppsPage {
    pub widget: gtk::Widget,
    ctx: std::rc::Weak<Ctx>,
    flow: gtk::FlowBox,
    stack: gtk::Stack,
    status: adw::StatusPage,
    refresh: gtk::Button,
    loaded: Rc<Cell<bool>>,
    loading: Rc<Cell<bool>>,
    /// Bumped on every TV switch, so late answers for the old TV are dropped.
    gen: Rc<Cell<u64>>,
}

impl AppsPage {
    pub fn new(ctx: &Rc<Ctx>) -> Rc<AppsPage> {
        let page = adw::PreferencesPage::new();

        // --- quick launch ------------------------------------------------------------------
        let quick = adw::PreferencesGroup::builder().title("Quick launch").build();
        let yt = adw::EntryRow::builder()
            .title("YouTube video (URL or ID)")
            .show_apply_button(true)
            .build();
        yt.add_prefix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));
        quick.add(&yt);
        let nf = adw::ActionRow::builder()
            .title("Netflix")
            .subtitle("Opens the Netflix app (DIAL)")
            .activatable(true)
            .build();
        nf.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        quick.add(&nf);
        page.add(&quick);

        let weak = ctx.weak();
        yt.connect_apply(move |row| {
            let Some(c) = weak.upgrade() else { return };
            let Some(text) = non_empty(&row.text()) else {
                c.toast("Paste a YouTube link or video ID first");
                return;
            };
            let weak2 = c.weak();
            c.spawn(
                move |tv| tv.youtube(&text),
                move |res| {
                    if let Some(c) = weak2.upgrade() {
                        match res {
                            Ok(tvremote_core::YouTubeOpened::Video) => c.toast("Opening on YouTube…"),
                            Ok(tvremote_core::YouTubeOpened::AppOnly) => c.toast(
                                "This TV doesn't accept video links: opened the YouTube app instead",
                            ),
                            Err(e) => c.toast(&format!("YouTube: {e}")),
                        }
                    }
                },
            );
        });
        let weak = ctx.weak();
        nf.connect_activated(move |_| {
            let Some(c) = weak.upgrade() else { return };
            let weak2 = c.weak();
            c.spawn(
                |tv| tv.netflix(),
                move |res| {
                    if let (Some(c), Err(e)) = (weak2.upgrade(), res) {
                        c.toast(&format!("Netflix: {e}"));
                    }
                },
            );
        });

        // --- installed apps -----------------------------------------------------------------
        let group = adw::PreferencesGroup::builder()
            .title("Installed apps")
            .description("Click an app to launch it on the TV")
            .build();
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.set_tooltip_text(Some("Refresh the list"));
        refresh.add_css_class("flat");
        refresh.set_valign(gtk::Align::Center);
        group.set_header_suffix(Some(&refresh));

        let flow = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(true)
            .min_children_per_line(2)
            .max_children_per_line(6)
            .column_spacing(12)
            .row_spacing(12)
            .build();
        let status = adw::StatusPage::builder()
            .icon_name("view-app-grid-symbolic")
            .title("Apps not loaded")
            .description("Open this page with the TV on to load its apps, or press Refresh.")
            .build();
        status.add_css_class("compact");
        let spinner = adw::Spinner::new();
        spinner.set_size_request(32, 32);
        spinner.set_halign(gtk::Align::Center);
        spinner.set_margin_top(24);
        spinner.set_margin_bottom(24);
        let stack = gtk::Stack::new();
        stack.add_named(&status, Some("status"));
        stack.add_named(&spinner, Some("loading"));
        stack.add_named(&flow, Some("apps"));
        stack.set_visible_child_name("status");
        group.add(&stack);
        page.add(&group);

        let this = Rc::new(AppsPage {
            widget: page.upcast(),
            ctx: ctx.weak(),
            flow,
            stack,
            status,
            refresh: refresh.clone(),
            loaded: Rc::new(Cell::new(false)),
            loading: Rc::new(Cell::new(false)),
            gen: Rc::new(Cell::new(0)),
        });
        let weak = Rc::downgrade(&this);
        refresh.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.load();
            }
        });
        this
    }

    /// Another TV is controlled now: forget the list; reload at once if `visible`.
    pub fn on_tv_changed(&self, visible: bool) {
        self.gen.set(self.gen.get() + 1);
        self.loaded.set(false);
        self.loading.set(false);
        while let Some(child) = self.flow.first_child() {
            self.flow.remove(&child);
        }
        self.refresh.set_sensitive(true);
        if visible {
            self.load();
        } else {
            self.stack.set_visible_child_name("status");
        }
    }

    /// Called when the page becomes visible: load once.
    pub fn on_shown(&self) {
        if !self.loaded.get() {
            self.load();
        }
    }

    pub fn load(&self) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        if self.loading.replace(true) {
            return;
        }
        self.refresh.set_sensitive(false);
        self.stack.set_visible_child_name("loading");
        let flow = self.flow.clone();
        let stack = self.stack.clone();
        let status = self.status.clone();
        let refresh = self.refresh.clone();
        let loading = self.loading.clone();
        let loaded = self.loaded.clone();
        let gen = self.gen.clone();
        let my_gen = gen.get();
        let weak = ctx.weak();
        ctx.spawn(
            |tv| tv.installed_apps(),
            move |res| {
                if gen.get() != my_gen {
                    return; // answer for the previous TV
                }
                refresh.set_sensitive(true);
                loading.set(false);
                match res {
                    Ok(mut apps) => {
                        loaded.set(true);
                        apps.sort_by_key(|a| a.name.to_lowercase());
                        while let Some(child) = flow.first_child() {
                            flow.remove(&child);
                        }
                        for app in apps.iter() {
                            flow.append(&app_tile(&weak, app));
                        }
                        if apps.is_empty() {
                            status.set_title("No apps reported");
                            status.set_description(Some("The TV returned an empty list."));
                            stack.set_visible_child_name("status");
                        } else {
                            stack.set_visible_child_name("apps");
                        }
                    }
                    Err(e) => {
                        status.set_title("Could not load the apps");
                        status.set_description(Some(&e.to_string()));
                        stack.set_visible_child_name("status");
                        if let Some(c) = weak.upgrade() {
                            c.toast(&format!("Apps: {e}"));
                        }
                    }
                }
            },
        );
    }
}

fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

fn app_tile(ctx: &std::rc::Weak<Ctx>, app: &App) -> gtk::Widget {
    let b = gtk::Button::new();
    b.add_css_class("tv-app-tile");
    let v = gtk::Box::new(gtk::Orientation::Vertical, 6);
    v.set_margin_top(10);
    v.set_margin_bottom(10);
    let av = adw::Avatar::new(48, Some(&app.name), true);
    av.set_text(Some(&initials(&app.name)));
    let l = gtk::Label::new(Some(&app.name));
    l.set_ellipsize(gtk::pango::EllipsizeMode::End);
    l.set_max_width_chars(14);
    v.append(&av);
    v.append(&l);
    b.set_child(Some(&v));
    b.set_tooltip_text(Some(&format!("{} ({})", app.name, app.app_id)));
    let app = app.clone();
    let weak = ctx.clone();
    b.connect_clicked(move |_| {
        if let Some(c) = weak.upgrade() {
            let app = app.clone();
            let name = app.name.clone();
            let w2 = c.weak();
            c.spawn(
                move |tv| tv.launch_app(&app),
                move |res| {
                    if let Some(c) = w2.upgrade() {
                        match res {
                            Ok(()) => c.toast(&format!("Launching {name}…")),
                            Err(e) => c.toast(&format!("{name}: {e}")),
                        }
                    }
                },
            );
        }
    });
    b.upcast()
}

#[cfg(test)]
mod tests {
    #[test]
    fn initials() {
        assert_eq!(super::initials("Prime Video"), "PV");
        assert_eq!(super::initials("youtube"), "Y");
        assert_eq!(super::initials(""), "");
    }
}
