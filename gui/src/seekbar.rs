//! VLC / mpv-like seek bar: elapsed time, a slider, and the length (or the time left).
//! Shared by the Remote page's now-playing bar and the Cast page's now-playing card.
//!
//! * A click anywhere on the slider jumps straight to that point, and dragging scrubs
//!   (the elapsed label previews the target; the seek is sent on release). This is done
//!   by our own drag gesture, claimed in the capture phase, so it never depends on the
//!   `gtk-primary-button-warps-slider` setting (which GTK's own gesture follows: with
//!   it off, a click only moves by a page).
//! * Hovering shows the time under the pointer.
//! * Keyboard / scroll changes on the focused slider are sent after a short pause.
//! * Position updates are ignored while the user drags, and for a moment after a seek
//!   (the TV still reports the old position meanwhile), so the knob never jumps back.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;
use tvremote_core::SEEK_STEP;

use crate::format::{fmt_duration, x_to_fraction};

/// How long position updates are ignored after a seek is sent (until it is done).
const HOLD_SENDING: Duration = Duration::from_secs(4);
/// … and once the TV accepted it, while it settles on the new position.
const HOLD_SETTLE: Duration = Duration::from_millis(1500);

type SeekHandler = Rc<dyn Fn(Duration)>;

pub struct SeekBar {
    pub widget: gtk::Box,
    scale: gtk::Scale,
    elapsed: gtk::Label,
    end: gtk::Label,
    /// Length in seconds (0: unknown).
    length: Cell<f64>,
    /// Last position reported, seconds.
    position: Cell<f64>,
    seekable: Cell<bool>,
    dragging: Cell<bool>,
    /// Show the time left (`-1:23`) instead of the length; toggled by a click on it.
    show_left: Cell<bool>,
    hold_until: Cell<Option<Instant>>,
    debounce: RefCell<Option<glib::SourceId>>,
    on_seek: RefCell<Option<SeekHandler>>,
    /// Why seeking is off (tooltip).
    reason: RefCell<String>,
}

impl SeekBar {
    pub fn new() -> Rc<SeekBar> {
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
        scale.set_draw_value(false);
        scale.set_hexpand(true);
        scale.set_increments(SEEK_STEP.as_secs_f64(), 60.0);
        scale.set_has_tooltip(true);
        let elapsed = gtk::Label::new(Some("0:00"));
        let end = gtk::Label::new(Some("–"));
        for l in [&elapsed, &end] {
            l.add_css_class("numeric");
            l.add_css_class("dim-label");
        }
        end.set_tooltip_text(Some("Click: length / time left"));
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        widget.append(&elapsed);
        widget.append(&scale);
        widget.append(&end);

        let this = Rc::new(SeekBar {
            widget,
            scale,
            elapsed,
            end,
            length: Cell::new(0.0),
            position: Cell::new(0.0),
            seekable: Cell::new(false),
            dragging: Cell::new(false),
            show_left: Cell::new(false),
            hold_until: Cell::new(None),
            debounce: RefCell::new(None),
            on_seek: RefCell::new(None),
            reason: RefCell::new(String::new()),
        });
        this.set_seekable(false, "");

        // Click / drag: map the pointer to a time ourselves.
        let drag = gtk::GestureDrag::new();
        drag.set_button(gtk::gdk::BUTTON_PRIMARY);
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = Rc::downgrade(&this);
        drag.connect_drag_begin(move |g, x, _| {
            let Some(sb) = w.upgrade() else { return };
            if !sb.interactive() {
                return;
            }
            g.set_state(gtk::EventSequenceState::Claimed);
            sb.cancel_debounce();
            sb.dragging.set(true);
            sb.preview(sb.value_at(x));
        });
        let w = Rc::downgrade(&this);
        drag.connect_drag_update(move |g, dx, _| {
            let Some(sb) = w.upgrade() else { return };
            if let (true, Some((x, _))) = (sb.dragging.get(), g.start_point()) {
                sb.preview(sb.value_at(x + dx));
            }
        });
        let w = Rc::downgrade(&this);
        drag.connect_drag_end(move |g, dx, _| {
            let Some(sb) = w.upgrade() else { return };
            if !sb.dragging.replace(false) {
                return;
            }
            if let Some((x, _)) = g.start_point() {
                let v = sb.value_at(x + dx);
                sb.preview(v);
                sb.send(v);
            }
        });
        this.scale.add_controller(drag);

        // Keyboard / scroll on the focused slider: seek once the changes stop.
        let w = Rc::downgrade(&this);
        this.scale.connect_change_value(move |_, _, value| {
            let Some(sb) = w.upgrade() else { return glib::Propagation::Stop };
            if !sb.interactive() || sb.dragging.get() {
                return glib::Propagation::Stop;
            }
            let value = value.clamp(0.0, sb.length.get());
            sb.preview(value);
            sb.cancel_debounce();
            let w2 = Rc::downgrade(&sb);
            let id = glib::timeout_add_local_once(Duration::from_millis(400), move || {
                let Some(sb) = w2.upgrade() else { return };
                sb.debounce.borrow_mut().take();
                sb.send(value);
            });
            *sb.debounce.borrow_mut() = Some(id);
            glib::Propagation::Stop
        });

        // Hover: the time under the pointer (or why seeking is off).
        let w = Rc::downgrade(&this);
        this.scale.connect_query_tooltip(move |_, x, _, keyboard, tip| {
            let Some(sb) = w.upgrade() else { return false };
            if !sb.interactive() {
                let r = sb.reason.borrow();
                if r.is_empty() {
                    return false;
                }
                tip.set_text(Some(r.as_str()));
                return true;
            }
            let v = if keyboard { sb.scale.value() } else { sb.value_at(f64::from(x)) };
            tip.set_text(Some(&fmt_duration(Duration::from_secs_f64(v))));
            true
        });
        let motion = gtk::EventControllerMotion::new();
        let s2 = this.scale.clone();
        motion.connect_motion(move |_, _, _| s2.trigger_tooltip_query());
        this.scale.add_controller(motion);

        // Click on the length: toggle length / time left.
        let click = gtk::GestureClick::new();
        let w = Rc::downgrade(&this);
        click.connect_released(move |_, _, _, _| {
            if let Some(sb) = w.upgrade() {
                sb.show_left.set(!sb.show_left.get());
                sb.update_end();
            }
        });
        this.end.add_controller(click);
        this
    }

    /// Called with the target when the user picks a point (call [`SeekBar::seek_done`]
    /// once the TV answered).
    pub fn connect_seek(&self, f: impl Fn(Duration) + 'static) {
        *self.on_seek.borrow_mut() = Some(Rc::new(f));
    }

    /// The seek was sent (successfully or not): follow the TV again shortly.
    pub fn seek_done(&self) {
        self.hold_until.set(Some(Instant::now() + HOLD_SETTLE));
    }

    /// Enable seeking, or grey the slider out with `reason` as its tooltip.
    pub fn set_seekable(&self, seekable: bool, reason: &str) {
        self.seekable.set(seekable);
        *self.reason.borrow_mut() = reason.to_string();
        let on = self.interactive();
        self.scale.set_sensitive(on);
        // Insensitive widgets may not get tooltips: the row carries the reason too.
        self.widget.set_tooltip_text((!on && !reason.is_empty()).then_some(reason));
    }

    /// New position / length from the TV (ignored while dragging or just after a seek).
    pub fn set_position(&self, pos: Option<Duration>, dur: Option<Duration>) {
        let len = dur.filter(|d| !d.is_zero()).map(|d| d.as_secs_f64());
        if let Some(l) = len {
            let was = self.interactive();
            self.length.set(l);
            if was != self.interactive() {
                let r = self.reason.borrow().clone();
                self.set_seekable(self.seekable.get(), &r);
            }
        }
        if self.dragging.get()
            || self.debounce.borrow().is_some()
            || self.hold_until.get().is_some_and(|t| Instant::now() < t)
        {
            self.update_end();
            return;
        }
        self.hold_until.set(None);
        let p = pos.unwrap_or_default().as_secs_f64();
        self.position.set(p);
        let l = self.length.get();
        self.scale.set_range(0.0, if l > 0.0 { l } else { 1.0 });
        self.scale.set_value(if l > 0.0 { p.min(l) } else { 0.0 });
        self.elapsed.set_text(&fmt_duration(Duration::from_secs_f64(p)));
        self.update_end();
    }

    /// Back to "nothing playing".
    pub fn reset(&self) {
        self.cancel_debounce();
        self.dragging.set(false);
        self.hold_until.set(None);
        self.length.set(0.0);
        let r = self.reason.borrow().clone();
        self.set_seekable(self.seekable.get(), &r);
        self.set_position(None, None);
    }

    /// Seeking is on and the length is known.
    fn interactive(&self) -> bool {
        self.seekable.get() && self.length.get() > 0.0
    }

    /// The time (seconds) under `x` (slider coordinates).
    fn value_at(&self, x: f64) -> f64 {
        let (start, width, knob) = trough_geometry(&self.scale);
        x_to_fraction(x, start, width, knob) * self.length.get()
    }

    /// Show `v` on the knob and the elapsed label without seeking yet.
    fn preview(&self, v: f64) {
        self.scale.set_value(v);
        self.elapsed.set_text(&fmt_duration(Duration::from_secs_f64(v.max(0.0))));
        self.position.set(v);
        self.update_end();
    }

    fn send(&self, v: f64) {
        self.hold_until.set(Some(Instant::now() + HOLD_SENDING));
        let f = self.on_seek.borrow().clone();
        if let Some(f) = f {
            f(Duration::from_secs_f64(v.max(0.0)));
        }
    }

    fn cancel_debounce(&self) {
        if let Some(id) = self.debounce.borrow_mut().take() {
            id.remove();
        }
    }

    fn update_end(&self) {
        let l = self.length.get();
        if l <= 0.0 {
            self.end.set_text("–");
        } else if self.show_left.get() {
            let left = (l - self.position.get()).max(0.0);
            self.end.set_text(&format!("-{}", fmt_duration(Duration::from_secs_f64(left))));
        } else {
            self.end.set_text(&fmt_duration(Duration::from_secs_f64(l)));
        }
    }
}

/// The slider's trough (start x, width) and knob width, in the scale's coordinates.
/// Falls back to the whole widget if the theme's node names ever change.
fn trough_geometry(scale: &gtk::Scale) -> (f64, f64, f64) {
    let whole = (0.0, f64::from(scale.width()), 0.0);
    let Some(trough) = children(scale.upcast_ref()).find(|w| w.css_name() == "trough") else {
        return whole;
    };
    let Some(b) = trough.compute_bounds(scale) else { return whole };
    let knob = children(&trough)
        .find(|w| w.css_name() == "slider")
        .map(|s| f64::from(s.width()))
        .unwrap_or(0.0);
    (f64::from(b.x()), f64::from(b.width()), knob)
}

fn children(w: &gtk::Widget) -> impl Iterator<Item = gtk::Widget> {
    std::iter::successors(w.first_child(), |c| c.next_sibling())
}
