//! Now-playing card: title, state, position / duration, seek bar (direct casts only),
//! play/pause/stop and a throughput sparkline. Driven by `Event::Cast`.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use tvremote_core::dlna::TransportState;
use tvremote_core::{CastEvent, CastSession, CastStatus};

use crate::ctx::Ctx;
use crate::format::{fmt_duration, fmt_mbps, sparkline_points};

/// Samples kept for the sparkline (one per second).
const MAX_SAMPLES: usize = 90;

pub struct NowPlaying {
    pub group: adw::PreferencesGroup,
    ctx: Weak<Ctx>,
    title_row: adw::ActionRow,
    pos_label: gtk::Label,
    seek: gtk::Scale,
    seek_note: gtk::Label,
    play_btn: gtk::Button,
    stop_btn: gtk::Button,
    spark: gtk::DrawingArea,
    rate_label: gtk::Label,
    samples: Rc<RefCell<VecDeque<f64>>>,
    session: RefCell<Option<CastSession>>,
    seekable: Cell<bool>,
    playing: Cell<bool>,
    seek_timer: RefCell<Option<glib::SourceId>>,
    seek_pending: Cell<bool>,
}

fn state_text(s: &TransportState) -> &'static str {
    match s {
        TransportState::Playing => "Playing",
        TransportState::Paused => "Paused",
        TransportState::Stopped => "Stopped",
        TransportState::Transitioning => "Buffering…",
        TransportState::NoMedia => "No media",
        TransportState::Other(_) => "Unknown state",
    }
}

impl NowPlaying {
    pub fn new(ctx: &Rc<Ctx>) -> Rc<NowPlaying> {
        let group = adw::PreferencesGroup::builder().title("Now playing").build();
        let title_row = adw::ActionRow::builder().title("Nothing is being cast").build();
        title_row.add_prefix(&gtk::Image::from_icon_name("video-display-symbolic"));
        group.add(&title_row);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        body.set_margin_top(12);
        body.set_margin_bottom(12);
        body.set_margin_start(12);
        body.set_margin_end(12);

        let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
        seek.set_draw_value(false);
        seek.set_hexpand(true);
        let pos_label = gtk::Label::new(Some("0:00 / 0:00"));
        pos_label.add_css_class("numeric");
        pos_label.add_css_class("dim-label");
        let seek_line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        seek_line.append(&seek);
        seek_line.append(&pos_label);
        body.append(&seek_line);
        let seek_note = gtk::Label::new(Some("TV-friendly streams are live: seeking is not possible."));
        seek_note.add_css_class("caption");
        seek_note.add_css_class("dim-label");
        seek_note.set_xalign(0.0);
        seek_note.set_visible(false);
        body.append(&seek_note);

        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        controls.set_halign(gtk::Align::Center);
        let play_btn = gtk::Button::from_icon_name("media-playback-pause-symbolic");
        play_btn.add_css_class("circular");
        play_btn.add_css_class("suggested-action");
        play_btn.set_size_request(48, 48);
        play_btn.set_tooltip_text(Some("Pause / resume"));
        let stop_btn = gtk::Button::from_icon_name("media-playback-stop-symbolic");
        stop_btn.add_css_class("circular");
        stop_btn.set_size_request(48, 48);
        stop_btn.set_tooltip_text(Some("Stop casting"));
        controls.append(&play_btn);
        controls.append(&stop_btn);
        body.append(&controls);

        let spark = gtk::DrawingArea::new();
        spark.set_content_height(64);
        spark.set_hexpand(true);
        let rate_label = gtk::Label::new(Some("Throughput: —"));
        rate_label.add_css_class("caption");
        rate_label.add_css_class("dim-label");
        rate_label.set_xalign(0.0);
        body.append(&spark);
        body.append(&rate_label);
        group.add(&body);

        let samples: Rc<RefCell<VecDeque<f64>>> = Rc::new(RefCell::new(VecDeque::new()));
        let s2 = samples.clone();
        spark.set_draw_func(move |_, cr, w, h| {
            let v: Vec<f64> = s2.borrow().iter().copied().collect();
            let (pts, _max) = sparkline_points(&v, 10.0);
            let (w, h) = (f64::from(w), f64::from(h));
            let accent = adw::StyleManager::default().accent_color_rgba();
            let (r, g, b) = (
                f64::from(accent.red()),
                f64::from(accent.green()),
                f64::from(accent.blue()),
            );
            // Baseline
            cr.set_source_rgba(r, g, b, 0.25);
            cr.set_line_width(1.0);
            cr.move_to(0.0, h - 0.5);
            cr.line_to(w, h - 0.5);
            let _ = cr.stroke();
            if pts.len() < 2 {
                return;
            }
            let pad = 2.0;
            let map = |(x, y): (f64, f64)| (x * w, h - pad - y * (h - 2.0 * pad));
            // Fill
            let (x0, _) = map(pts[0]);
            cr.move_to(x0, h);
            for p in &pts {
                let (x, y) = map(*p);
                cr.line_to(x, y);
            }
            if let Some(last) = pts.last() {
                let (xl, _) = map(*last);
                cr.line_to(xl, h);
            }
            cr.close_path();
            cr.set_source_rgba(r, g, b, 0.2);
            let _ = cr.fill();
            // Line
            for (i, p) in pts.iter().enumerate() {
                let (x, y) = map(*p);
                if i == 0 {
                    cr.move_to(x, y);
                } else {
                    cr.line_to(x, y);
                }
            }
            cr.set_source_rgba(r, g, b, 1.0);
            cr.set_line_width(2.0);
            let _ = cr.stroke();
        });

        let this = Rc::new(NowPlaying {
            group,
            ctx: ctx.weak(),
            title_row,
            pos_label,
            seek,
            seek_note,
            play_btn,
            stop_btn,
            spark,
            rate_label,
            samples,
            session: RefCell::new(None),
            seekable: Cell::new(false),
            playing: Cell::new(false),
            seek_timer: RefCell::new(None),
            seek_pending: Cell::new(false),
        });
        this.set_active(false);

        let w = Rc::downgrade(&this);
        this.play_btn.connect_clicked(move |_| {
            let Some(np) = w.upgrade() else { return };
            let Some(s) = np.session.borrow().clone() else { return };
            let Some(c) = np.ctx.upgrade() else { return };
            let pause = np.playing.get();
            let wc = c.weak();
            c.spawn(
                move |_| if pause { s.pause() } else { s.resume() },
                move |res| {
                    if let (Some(c), Err(e)) = (wc.upgrade(), res) {
                        c.toast(&format!("Playback: {e}"));
                    }
                },
            );
        });
        let w = Rc::downgrade(&this);
        this.stop_btn.connect_clicked(move |_| {
            let Some(np) = w.upgrade() else { return };
            let Some(s) = np.session.borrow().clone() else { return };
            let Some(c) = np.ctx.upgrade() else { return };
            let wc = c.weak();
            c.spawn(
                move |_| s.stop(),
                move |res| {
                    if let (Some(c), Err(e)) = (wc.upgrade(), res) {
                        c.toast(&format!("Stop: {e}"));
                    }
                },
            );
        });
        let w = Rc::downgrade(&this);
        this.seek.connect_change_value(move |_, _, value| {
            let Some(np) = w.upgrade() else { return glib::Propagation::Stop };
            if !np.seekable.get() {
                return glib::Propagation::Stop;
            }
            np.seek_pending.set(true);
            if let Some(id) = np.seek_timer.borrow_mut().take() {
                id.remove();
            }
            let w2 = Rc::downgrade(&np);
            let id = glib::timeout_add_local_once(Duration::from_millis(400), move || {
                let Some(np) = w2.upgrade() else { return };
                np.seek_timer.borrow_mut().take();
                np.do_seek(value);
            });
            *np.seek_timer.borrow_mut() = Some(id);
            glib::Propagation::Proceed
        });
        this
    }

    fn do_seek(self: &Rc<Self>, secs: f64) {
        let Some(s) = self.session.borrow().clone() else { return };
        let Some(c) = self.ctx.upgrade() else { return };
        let w = Rc::downgrade(self);
        let wc = c.weak();
        let to = Duration::from_secs_f64(secs.max(0.0));
        c.spawn(
            move |_| s.seek(to),
            move |res| {
                if let Some(np) = w.upgrade() {
                    np.seek_pending.set(false);
                }
                if let (Some(c), Err(e)) = (wc.upgrade(), res) {
                    c.toast(&format!("Seek: {e}"));
                }
            },
        );
    }

    fn set_active(&self, active: bool) {
        self.play_btn.set_sensitive(active);
        self.stop_btn.set_sensitive(active);
        self.seek.set_sensitive(active && self.seekable.get());
    }

    /// A session started (from this page or found at startup).
    pub fn attach(self: &Rc<Self>, session: CastSession) {
        *self.session.borrow_mut() = Some(session.clone());
        self.samples.borrow_mut().clear();
        self.spark.queue_draw();
        self.set_active(true);
        let Some(c) = self.ctx.upgrade() else { return };
        let w = Rc::downgrade(self);
        c.spawn(
            move |_| (session.status(), session.throughput()),
            move |(st, tp)| {
                let Some(np) = w.upgrade() else { return };
                {
                    let mut s = np.samples.borrow_mut();
                    for t in tp.iter().rev().take(MAX_SAMPLES).rev() {
                        s.push_back(t.mbps());
                    }
                }
                np.spark.queue_draw();
                np.apply_status(&st);
            },
        );
    }

    fn apply_status(&self, st: &CastStatus) {
        self.title_row.set_title(&glib::markup_escape_text(&st.title));
        self.seekable.set(st.seekable);
        self.seek.set_visible(st.seekable);
        self.seek_note.set_visible(st.tv_friendly);
        if let Some(s) = &st.state {
            self.set_state(s);
        }
        self.set_position(st.position, st.duration);
        if st.outcome.is_some() {
            self.finish("Finished");
        } else {
            self.set_active(true);
        }
    }

    fn set_state(&self, s: &TransportState) {
        self.title_row.set_subtitle(state_text(s));
        let playing = matches!(s, TransportState::Playing | TransportState::Transitioning);
        self.playing.set(playing);
        self.play_btn.set_icon_name(if playing {
            "media-playback-pause-symbolic"
        } else {
            "media-playback-start-symbolic"
        });
    }

    fn set_position(&self, pos: Option<Duration>, dur: Option<Duration>) {
        let p = pos.unwrap_or_default();
        let text = match dur {
            Some(d) if !d.is_zero() => format!("{} / {}", fmt_duration(p), fmt_duration(d)),
            _ => fmt_duration(p),
        };
        self.pos_label.set_text(&text);
        if self.seek_pending.get() || self.seek_timer.borrow().is_some() {
            return;
        }
        if let Some(d) = dur.filter(|d| !d.is_zero()) {
            self.seek.set_range(0.0, d.as_secs_f64());
        }
        self.seek.set_value(p.as_secs_f64());
    }

    fn finish(&self, what: &str) {
        self.title_row.set_subtitle(what);
        self.playing.set(false);
        self.play_btn.set_icon_name("media-playback-start-symbolic");
        self.play_btn.set_sensitive(false);
        self.stop_btn.set_sensitive(false);
        self.seek.set_sensitive(false);
        self.session.borrow_mut().take();
    }

    /// Handle a cast event from the bus. Toasts (stalls, errors) are shown by the caller.
    pub fn handle(&self, ev: &CastEvent) {
        match ev {
            CastEvent::Loading { title } => {
                self.title_row.set_title(&glib::markup_escape_text(title));
                self.title_row.set_subtitle("Loading on the TV…");
                self.samples.borrow_mut().clear();
                self.spark.queue_draw();
            }
            CastEvent::State(s) => self.set_state(s),
            CastEvent::Position { position, duration } => self.set_position(*position, *duration),
            CastEvent::Throughput(t) => {
                let mbps = t.mbps();
                {
                    let mut s = self.samples.borrow_mut();
                    s.push_back(mbps);
                    while s.len() > MAX_SAMPLES {
                        s.pop_front();
                    }
                }
                self.rate_label.set_text(&format!("Throughput: {} (last second)", fmt_mbps(mbps)));
                self.spark.queue_draw();
            }
            CastEvent::Ended => self.finish("Ended"),
            CastEvent::NeverStarted => self.finish("The TV never started playing"),
            CastEvent::Stopped => self.finish("Stopped"),
            CastEvent::Stall { .. } | CastEvent::Error(_) => {}
        }
    }
}

impl NowPlaying {
    /// A session is attached and not finished.
    pub fn is_active(&self) -> bool {
        self.session.borrow().is_some()
    }
}
