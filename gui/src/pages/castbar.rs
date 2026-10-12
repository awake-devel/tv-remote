//! Remote page now-playing bar: shown only while something casts on the controlled TV
//! (this app's session or a cast started elsewhere), with the title, state,
//! play/pause and the VLC-like seek bar.
//!
//! This app's own session is followed through `Event::Cast` (position every second).
//! Casts started elsewhere send no events, so while the page is shown the bar polls
//! [`tvremote_core::Tv::now_playing`] every 3 s (no I/O for an own session, two SOAP
//! calls otherwise). That poll also keeps the renderer state fresh for ← →
//! ([`Ctx::media_key`]). Nothing is polled while the page is hidden or the TV is off.

use std::cell::Cell;
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use tvremote_core::dlna::TransportState;
use tvremote_core::{CastEvent, CastStatus, Event, PowerState};

use crate::ctx::Ctx;
use crate::pages::cast::playing::{state_text, NO_PAUSE_LIVE, NO_SEEK_LIVE};
use crate::seekbar::SeekBar;

/// Poll interval while the Remote page is shown.
const POLL: Duration = Duration::from_secs(3);

/// Tooltip of the greyed-out seek bar for a live cast started elsewhere.
const NO_SEEK_ELSEWHERE: &str = "This cast is a live stream (the TV reports no length): seeking is not possible";

pub struct CastBar {
    pub widget: gtk::Revealer,
    ctx: Weak<Ctx>,
    title: gtk::Label,
    subtitle: gtk::Label,
    play_btn: gtk::Button,
    bar: Rc<SeekBar>,
    /// What is shown is this app's own session (its events update it).
    own: Cell<bool>,
    in_flight: Cell<bool>,
    /// Buffering before the TV knows the length ([`CastStatus::is_loading`]).
    loading: Cell<bool>,
}

impl CastBar {
    pub fn new(ctx: &Rc<Ctx>) -> Rc<CastBar> {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
        card.add_css_class("card");
        card.add_css_class("tv-castbar");
        card.set_margin_top(24);
        card.set_margin_start(12);
        card.set_margin_end(12);

        let top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let play_btn = gtk::Button::from_icon_name("media-playback-pause-symbolic");
        play_btn.add_css_class("circular");
        play_btn.add_css_class("suggested-action");
        play_btn.set_size_request(40, 40);
        play_btn.set_valign(gtk::Align::Center);
        play_btn.set_tooltip_text(Some("Pause / resume the cast"));
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_valign(gtk::Align::Center);
        text.set_hexpand(true);
        let title = gtk::Label::new(None);
        title.add_css_class("heading");
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        let subtitle = gtk::Label::new(None);
        subtitle.add_css_class("caption");
        subtitle.add_css_class("dim-label");
        subtitle.set_xalign(0.0);
        text.append(&title);
        text.append(&subtitle);
        top.append(&play_btn);
        top.append(&text);
        card.append(&top);
        let bar = SeekBar::new();
        card.append(&bar.widget);

        let widget = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .child(&card)
            .build();

        let this = Rc::new(CastBar {
            widget,
            ctx: ctx.weak(),
            title,
            subtitle,
            play_btn,
            bar,
            own: Cell::new(false),
            in_flight: Cell::new(false),
            loading: Cell::new(false),
        });

        let w = Rc::downgrade(&this);
        this.play_btn.connect_clicked(move |_| {
            let Some(cb) = w.upgrade() else { return };
            let Some(c) = cb.ctx.upgrade() else { return };
            let w2 = Rc::downgrade(&cb);
            let wc = c.weak();
            c.spawn(
                |tv| tv.play_pause(),
                move |res| match (res, w2.upgrade()) {
                    (Ok(s), Some(cb)) => cb.set_state(&s),
                    (Err(e), _) => {
                        if let Some(c) = wc.upgrade() {
                            c.toast(&format!("Playback: {e}"));
                        }
                    }
                    _ => {}
                },
            );
        });
        let w = Rc::downgrade(&this);
        this.bar.connect_seek(move |to| {
            let Some(cb) = w.upgrade() else { return };
            let Some(c) = cb.ctx.upgrade() else { return };
            let w2 = Rc::downgrade(&cb);
            let wc = c.weak();
            c.spawn(
                move |tv| tv.seek_to(to),
                move |res| {
                    if let Some(cb) = w2.upgrade() {
                        cb.bar.seek_done();
                    }
                    if let (Some(c), Err(e)) = (wc.upgrade(), res) {
                        c.toast(&format!("Seek: {e}"));
                    }
                },
            );
        });

        // Poll while the page is shown; at once when it appears.
        let w = Rc::downgrade(&this);
        this.widget.connect_map(move |_| {
            if let Some(cb) = w.upgrade() {
                cb.refresh();
            }
        });
        let w = Rc::downgrade(&this);
        glib::timeout_add_local(POLL, move || match w.upgrade() {
            Some(cb) => {
                if cb.widget.is_mapped() {
                    cb.refresh();
                }
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
        this
    }

    /// Another TV is controlled now.
    pub fn on_tv_changed(self: &Rc<Self>) {
        self.show(None, false);
        if self.widget.is_mapped() {
            self.refresh();
        }
    }

    /// Cast events of this app's sessions (`tv_id`: the TV they come from).
    pub fn handle_event(self: &Rc<Self>, tv_id: &str, ev: &Event) {
        let Event::Cast(ce) = ev else { return };
        let Some(c) = self.ctx.upgrade() else { return };
        if tv_id != c.tv().id() {
            return;
        }
        match ce {
            CastEvent::Position { position, duration } if self.own.get() => {
                if duration.is_some_and(|d| !d.is_zero()) {
                    self.loading.set(false);
                }
                self.bar.set_position(*position, *duration)
            }
            CastEvent::State(s) if self.own.get() => self.set_state(s),
            CastEvent::Loading { .. } => {
                // The session is registered just after this event: look a bit later.
                let w = Rc::downgrade(self);
                glib::timeout_add_local_once(Duration::from_millis(800), move || {
                    if let Some(cb) = w.upgrade() {
                        cb.refresh();
                    }
                });
            }
            CastEvent::Ended | CastEvent::Stopped | CastEvent::NeverStarted => {
                self.show(None, false);
                self.refresh();
            }
            _ => {}
        }
    }

    /// Ask the controlled TV what is casting (skipped while the TV is known off).
    fn refresh(self: &Rc<Self>) {
        let Some(c) = self.ctx.upgrade() else { return };
        if self.in_flight.get() {
            return;
        }
        let id = c.tv().id();
        let power = c.dots.borrow().get(&id).and_then(|d| d.power.clone());
        if matches!(power, Some(PowerState::Off | PowerState::Standby)) {
            self.show(None, false);
            return;
        }
        self.in_flight.set(true);
        let w = Rc::downgrade(self);
        let wc = c.weak();
        c.spawn(
            |tv| (tv.current_cast().is_some(), tv.now_playing()),
            move |(own, res)| {
                let Some(cb) = w.upgrade() else { return };
                cb.in_flight.set(false);
                // The TV was switched meanwhile: the next poll asks the new one.
                if wc.upgrade().is_some_and(|c| c.tv().id() == id) {
                    cb.show(res.ok().flatten(), own);
                }
            },
        );
    }

    fn show(&self, st: Option<CastStatus>, own: bool) {
        let Some(st) = st else {
            self.own.set(false);
            self.widget.set_reveal_child(false);
            self.bar.reset();
            return;
        };
        self.own.set(own);
        self.title.set_text(&st.title);
        self.title.set_tooltip_text(Some(&st.title));
        // At the start of a cast the TV buffers with no length yet: loading, not live.
        let reason = if st.tv_friendly {
            NO_SEEK_LIVE
        } else if st.is_loading() {
            "Loading on the TV…"
        } else {
            NO_SEEK_ELSEWHERE
        };
        self.bar.set_seekable(st.seekable, reason);
        // The TV refuses Pause on a live (TV-friendly) stream.
        self.play_btn.set_sensitive(!st.tv_friendly);
        self.play_btn.set_tooltip_text(Some(if st.tv_friendly { NO_PAUSE_LIVE } else { "Pause / resume the cast" }));
        self.bar.set_position(st.position, st.duration);
        self.loading.set(st.is_loading());
        self.set_state(&st.state.unwrap_or(TransportState::Transitioning));
        self.widget.set_reveal_child(true);
    }

    fn set_state(&self, s: &TransportState) {
        let what = if self.loading.get() { "Loading on the TV…" } else { state_text(s) };
        self.subtitle.set_text(&if self.own.get() {
            what.to_string()
        } else {
            format!("{what} · cast from another device")
        });
        let playing = matches!(s, TransportState::Playing | TransportState::Transitioning);
        self.play_btn.set_icon_name(if playing {
            "media-playback-pause-symbolic"
        } else {
            "media-playback-start-symbolic"
        });
    }
}
