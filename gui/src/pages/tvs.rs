//! "TVs" section of the Device page: known TVs with LAN / SmartThings status dots
//! (use, rename, link / unlink SmartThings, power, remove), and a background
//! LAN + SmartThings scan whose new TVs can be added.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use tvremote_core::fleet::{Candidate, CloudTv};
use tvremote_core::{Presence, TvEntry};

use crate::ctx::Ctx;
use crate::format::dots_markup;

pub struct TvsSection {
    ctx: Weak<Ctx>,
    known: adw::PreferencesGroup,
    known_rows: RefCell<Vec<adw::ActionRow>>,
    found: adw::PreferencesGroup,
    found_rows: RefCell<Vec<adw::ActionRow>>,
    scan_btn: gtk::Button,
    candidates: RefCell<Vec<Candidate>>,
}

fn esc(s: &str) -> String {
    glib::markup_escape_text(s).to_string()
}

fn flat_button(label: &str) -> gtk::Button {
    let b = gtk::Button::with_label(label);
    b.add_css_class("flat");
    b.set_halign(gtk::Align::Fill);
    if let Some(l) = b.child().and_then(|c| c.downcast::<gtk::Label>().ok()) {
        l.set_xalign(0.0);
    }
    b
}

/// One menu entry: label, destructive?, action.
type MenuItem<'a> = (&'a str, bool, Box<dyn Fn()>);

/// A "⋮" menu with plain buttons (no actions plumbing needed).
fn menu(items: Vec<MenuItem>) -> gtk::MenuButton {
    let mb = gtk::MenuButton::new();
    mb.set_icon_name("view-more-symbolic");
    mb.add_css_class("flat");
    mb.set_valign(gtk::Align::Center);
    let pop = gtk::Popover::new();
    let bx = gtk::Box::new(gtk::Orientation::Vertical, 0);
    for (label, destructive, f) in items {
        let b = flat_button(label);
        if destructive {
            b.add_css_class("error");
        }
        let pop2 = pop.clone();
        b.connect_clicked(move |_| {
            pop2.popdown();
            f();
        });
        bx.append(&b);
    }
    pop.set_child(Some(&bx));
    mb.set_popover(Some(&pop));
    mb
}

impl TvsSection {
    /// Build the section and add its groups to `page`.
    pub fn new(ctx: &Rc<Ctx>, page: &adw::PreferencesPage) -> Rc<TvsSection> {
        let known = adw::PreferencesGroup::builder()
            .title("TVs")
            .description("● LAN   ● SmartThings. “Use” switches every page to that TV.")
            .build();
        let scan_btn = gtk::Button::with_label("Scan");
        scan_btn.add_css_class("flat");
        scan_btn.set_tooltip_text(Some("Find TVs on the network (SSDP) and in SmartThings"));
        known.set_header_suffix(Some(&scan_btn));
        page.add(&known);
        let found = adw::PreferencesGroup::builder()
            .title("Found")
            .description("TVs detected by the last scan that are not in the list yet")
            .visible(false)
            .build();
        page.add(&found);
        let this = Rc::new(TvsSection {
            ctx: ctx.weak(),
            known,
            known_rows: RefCell::new(Vec::new()),
            found,
            found_rows: RefCell::new(Vec::new()),
            scan_btn: scan_btn.clone(),
            candidates: RefCell::new(Vec::new()),
        });
        let w = Rc::downgrade(&this);
        scan_btn.connect_clicked(move |_| {
            if let Some(s) = w.upgrade() {
                s.scan();
            }
        });
        this.rebuild();
        this
    }

    /// Redraw the known-TV rows (names, current TV, status dots).
    pub fn rebuild(self: &Rc<Self>) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        for r in self.known_rows.borrow_mut().drain(..) {
            self.known.remove(&r);
        }
        let entries = ctx.fleet.entries();
        let current = ctx.tv().id();
        let has_st = ctx.fleet.has_smartthings();
        if entries.is_empty() {
            let r = adw::ActionRow::builder()
                .title("No TV yet")
                .subtitle("Press Scan, or set an address in Settings below")
                .build();
            self.known.add(&r);
            self.known_rows.borrow_mut().push(r);
        }
        for e in entries {
            let row = self.known_row(&ctx, &e, e.id == current, has_st);
            self.known.add(&row);
            self.known_rows.borrow_mut().push(row);
        }
        self.rebuild_found();
    }

    fn known_row(self: &Rc<Self>, ctx: &Rc<Ctx>, e: &TvEntry, current: bool, has_st: bool) -> adw::ActionRow {
        let dots = ctx.dots.borrow().get(&e.id).cloned().unwrap_or_default();
        let linked = has_st && e.smartthings_device_id.is_some();
        let host = if e.host.is_empty() { "cloud only".to_string() } else { e.host.clone() };
        let row = adw::ActionRow::builder()
            .title(esc(&e.display_name()))
            .subtitle(format!(
                "{} · {}",
                esc(&host),
                dots_markup(dots.power.as_ref(), linked, dots.cloud.as_ref(), true)
            ))
            .build();
        let icon = gtk::Image::from_icon_name(if current { "object-select-symbolic" } else { "video-display-symbolic" });
        if !current {
            icon.add_css_class("dim-label");
        }
        row.add_prefix(&icon);
        if current {
            let l = gtk::Label::new(Some("In use"));
            l.add_css_class("dim-label");
            row.add_suffix(&l);
        } else {
            let b = gtk::Button::with_label("Use");
            b.set_valign(gtk::Align::Center);
            let w = ctx.weak();
            let id = e.id.clone();
            b.connect_clicked(move |_| {
                if let Some(c) = w.upgrade() {
                    c.switch_tv(&id);
                }
            });
            row.add_suffix(&b);
        }
        let mut items: Vec<MenuItem> = Vec::new();
        let (w, id, name) = (Rc::downgrade(self), e.id.clone(), e.display_name());
        items.push(("Rename…", false, Box::new(move || {
            if let Some(s) = w.upgrade() {
                s.rename_dialog(&id, &name);
            }
        })));
        let (w, id) = (Rc::downgrade(self), e.id.clone());
        items.push(("Link SmartThings device…", false, Box::new(move || {
            if let Some(s) = w.upgrade() {
                s.link_dialog(&id);
            }
        })));
        if e.smartthings_device_id.is_some() {
            let (w, id) = (ctx.weak(), e.id.clone());
            items.push(("Unlink SmartThings device", false, Box::new(move || {
                if let Some(c) = w.upgrade() {
                    match c.fleet.unlink_smartthings(&id) {
                        Ok(()) => c.toast("Unlinked: it will not be matched automatically again"),
                        Err(e) => c.toast(&e.to_string()),
                    }
                }
            })));
        }
        for on in [true, false] {
            let (w, id) = (ctx.weak(), e.id.clone());
            items.push((if on { "Turn on" } else { "Turn off" }, false, Box::new(move || {
                let Some(c) = w.upgrade() else { return };
                let Ok(tv) = c.fleet.tv(&id) else { return };
                let name = tv.display_name();
                let w2 = c.weak();
                c.toast(&format!("{} {name}…", if on { "Turning on" } else { "Turning off" }));
                c.spawn_on(
                    tv,
                    move |tv| if on { tv.power_on(Duration::from_secs(20)) } else { tv.power_off() },
                    move |r| {
                        if let (Some(c), Err(e)) = (w2.upgrade(), r) {
                            c.toast(&format!("{name}: {e}"));
                        }
                    },
                );
            })));
        }
        let (w, id, name) = (ctx.weak(), e.id.clone(), e.display_name());
        items.push(("Remove…", true, Box::new(move || {
            let Some(c) = w.upgrade() else { return };
            let w2 = c.weak();
            let id = id.clone();
            c.confirm(
                &format!("Remove {name}?"),
                "Its pairing token is deleted: adding it again shows the approval prompt on the TV.",
                "Remove",
                true,
                move || {
                    let Some(c) = w2.upgrade() else { return };
                    let was_current = c.tv().id() == id;
                    match c.fleet.remove(&id) {
                        Ok(()) => {
                            if was_current {
                                if let Some(next) = c.fleet.active_id() {
                                    c.switch_tv(&next);
                                }
                            }
                        }
                        Err(e) => c.toast(&e.to_string()),
                    }
                },
            );
        })));
        row.add_suffix(&menu(items));
        row
    }

    fn rebuild_found(self: &Rc<Self>) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        for r in self.found_rows.borrow_mut().drain(..) {
            self.found.remove(&r);
        }
        let known = ctx.fleet.entries();
        let new: Vec<Candidate> = self
            .candidates
            .borrow()
            .iter()
            .filter(|c| c.known.as_ref().is_none_or(|(id, _)| !known.iter().any(|e| &e.id == id)))
            .filter(|c| !c.ip().is_some_and(|ip| known.iter().any(|e| e.host == ip)))
            .filter(|c| !c.device_id().is_some_and(|d| known.iter().any(|e| e.smartthings_device_id.as_deref() == Some(d))))
            .cloned()
            .collect();
        self.found.set_visible(!new.is_empty());
        for c in new {
            let mut sub = vec![c.presence.label().to_string()];
            if let Some(ip) = c.ip() {
                sub.push(ip.to_string());
            }
            if let Some(m) = &c.model {
                sub.push(m.clone());
            }
            if let (Presence::Both, Some(k)) = (c.presence, c.matched_by) {
                sub.push(format!("matched by {k:?}"));
            }
            let row = adw::ActionRow::builder().title(esc(&c.name)).subtitle(esc(&sub.join(" · "))).build();
            let b = gtk::Button::with_label("Add");
            b.add_css_class("suggested-action");
            b.set_valign(gtk::Align::Center);
            let w = ctx.weak();
            b.connect_clicked(move |_| {
                let Some(ctx) = w.upgrade() else { return };
                match ctx.fleet.add_candidate(&c) {
                    Ok(id) => {
                        let lan = c.lan.is_some();
                        ctx.toast_for(
                            &if lan {
                                format!("Added {}: press Use, then accept the prompt on that TV", c.name)
                            } else {
                                format!("Added {} (SmartThings only: no LAN address)", c.name)
                            },
                            6,
                        );
                        if ctx.fleet.entries().len() == 1 {
                            ctx.switch_tv(&id);
                        }
                    }
                    Err(e) => ctx.toast(&e.to_string()),
                }
            });
            row.add_suffix(&b);
            self.found.add(&row);
            self.found_rows.borrow_mut().push(row);
        }
    }

    /// LAN + SmartThings detection in the background.
    pub fn scan(self: &Rc<Self>) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        self.scan_btn.set_sensitive(false);
        self.scan_btn.set_label("Scanning…");
        let fleet = ctx.fleet.clone();
        let w = Rc::downgrade(self);
        ctx.spawn(
            move |_tv| fleet.scan(Duration::from_secs(4)),
            move |r| {
                let Some(s) = w.upgrade() else { return };
                s.scan_btn.set_sensitive(true);
                s.scan_btn.set_label("Scan");
                let Some(c) = s.ctx.upgrade() else { return };
                let mut msg = format!("{} TV(s) found", r.candidates.len());
                if !r.host_changes.is_empty() {
                    msg.push_str(&format!(", {} address(es) updated", r.host_changes.len()));
                }
                if let Some(e) = &r.lan_error {
                    msg.push_str(&format!(". LAN: {e}"));
                }
                if let Some(e) = &r.cloud_error {
                    msg.push_str(&format!(". SmartThings: {e}"));
                } else if !r.cloud_checked {
                    msg.push_str(" (LAN only: SmartThings not connected)");
                }
                c.toast_for(&msg, 6);
                *s.candidates.borrow_mut() = r.candidates;
                s.rebuild();
            },
        );
    }

    fn rename_dialog(self: &Rc<Self>, id: &str, name: &str) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        let d = adw::AlertDialog::new(Some("Rename TV"), None);
        let entry = gtk::Entry::new();
        entry.set_text(name);
        entry.set_activates_default(true);
        d.set_extra_child(Some(&entry));
        d.add_response("cancel", "Cancel");
        d.add_response("ok", "Rename");
        d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        d.set_default_response(Some("ok"));
        d.set_close_response("cancel");
        let (w, id) = (ctx.weak(), id.to_string());
        d.connect_response(None, move |_, resp| {
            if resp != "ok" {
                return;
            }
            if let Some(c) = w.upgrade() {
                if let Err(e) = c.fleet.rename(&id, &entry.text()) {
                    c.toast(&e.to_string());
                }
            }
        });
        d.present(Some(&ctx.window));
    }

    /// Pick one of the account's SmartThings TVs (fetched in the background) and link it.
    fn link_dialog(self: &Rc<Self>, id: &str) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        if !ctx.fleet.has_smartthings() {
            ctx.toast("Connect SmartThings on the Device page first");
            return;
        }
        let fleet = ctx.fleet.clone();
        let (w, id) = (ctx.weak(), id.to_string());
        ctx.spawn(
            move |_tv| fleet.scan_cloud(),
            move |res: tvremote_core::Result<Vec<CloudTv>>| {
                let Some(c) = w.upgrade() else { return };
                let tvs = match res {
                    Ok(t) if !t.is_empty() => t,
                    Ok(_) => return c.toast("No TV in this SmartThings account"),
                    Err(e) => return c.toast(&e.to_string()),
                };
                let current = c.fleet.config().entry(&id).and_then(|e| e.smartthings_device_id.clone());
                let names: Vec<String> = tvs
                    .iter()
                    .map(|t| {
                        let label = if t.device.label.is_empty() { &t.device.name } else { &t.device.label };
                        let state = match t.online() {
                            Some(true) => " · online",
                            Some(false) => " · offline",
                            None => "",
                        };
                        format!("{label} · {}{state}", t.model().unwrap_or("?"))
                    })
                    .collect();
                let refs: Vec<&str> = names.iter().map(String::as_str).collect();
                let dd = gtk::DropDown::from_strings(&refs);
                if let Some(p) = current.and_then(|d| tvs.iter().position(|t| t.device.device_id == d)) {
                    dd.set_selected(p as u32);
                }
                let d = adw::AlertDialog::new(
                    Some("Link a SmartThings device"),
                    Some("Power, volume and mute fall back to SmartThings when the LAN does not answer; picture and input go through it."),
                );
                d.set_extra_child(Some(&dd));
                d.add_response("cancel", "Cancel");
                d.add_response("ok", "Link");
                d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
                d.set_default_response(Some("ok"));
                d.set_close_response("cancel");
                let w2 = c.weak();
                let id2 = id.clone();
                d.connect_response(None, move |_, resp| {
                    if resp != "ok" {
                        return;
                    }
                    let Some(c) = w2.upgrade() else { return };
                    let Some(t) = tvs.get(dd.selected() as usize) else { return };
                    match c.fleet.link_smartthings(&id2, &t.device.device_id) {
                        Ok(()) => c.toast("Linked"),
                        Err(e) => c.toast(&e.to_string()),
                    }
                });
                d.present(Some(&c.window));
            },
        );
    }
}
