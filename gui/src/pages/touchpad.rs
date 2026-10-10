//! Touchpad & keyboard page: a pad that moves the TV pointer (throttled), and a text
//! entry that types into the focused TV field.
//!
//! On Wayland compositors with pointer constraints, a click on the pad *captures* the
//! mouse: the laptop pointer is locked and hidden, and relative motion steers the TV
//! pointer (see [`crate::pointer_lock`]). Elsewhere the pad falls back to dragging, with
//! "edge push" when the drag is held outside the pad.

use std::cell::Cell;
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, glib};
use tvremote_core::Key;

use crate::ctx::Ctx;
use crate::pointer_lock::PointerLock;

/// Pointer updates are flushed at most this often.
const FLUSH_INTERVAL: Duration = Duration::from_millis(40);
/// Don't queue more mouse moves than this while the TV is slow to answer.
const MAX_BACKLOG: usize = 3;
/// A drag shorter than this (px) is a tap.
const TAP_SLOP: f64 = 6.0;
/// Edge push: while a drag is held outside the pad, the TV pointer keeps moving that way
/// (the laptop cursor would otherwise stop at the screen edge). Pixels per flush for each
/// pixel outside the pad, and the cap on that distance.
const EDGE_GAIN: f64 = 0.15;
const EDGE_MAX_DIST: f64 = 160.0;
const PAD_CURSOR: &str = "crosshair";
const FALLBACK_TOAST: &str = "Mouse capture is not available on this display; dragging still works";

/// How far `(x, y)` lies outside a `w`×`h` box (0 inside), per axis, capped.
fn outside(x: f64, y: f64, w: f64, h: f64) -> (f64, f64) {
    let axis = |v: f64, len: f64| {
        if v < 0.0 {
            v.max(-EDGE_MAX_DIST)
        } else if v > len {
            (v - len).min(EDGE_MAX_DIST)
        } else {
            0.0
        }
    };
    (axis(x, w), axis(y, h))
}

/// Scale an accumulated delta by the pointer speed `k` and round it to whole TV pixels.
/// Returns the step to send and the unscaled sub-pixel remainder to keep, or `None` when
/// the step rounds to nothing.
fn scale_step(pending: (f64, f64), k: f64) -> Option<((i32, i32), (f64, f64))> {
    if k.is_nan() || k <= 0.0 {
        return None;
    }
    let (dx, dy) = pending;
    let (sx, sy) = ((dx * k).round(), (dy * k).round());
    if sx == 0.0 && sy == 0.0 {
        return None;
    }
    Some(((sx as i32, sy as i32), (dx - sx / k, dy - sy / k)))
}

/// What the pad is doing, for its label and styling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PadMode {
    /// No pointer lock on this display: drag to move.
    Drag,
    /// Capture available, not captured.
    Idle,
    /// Pointer locked; relative motion steers the TV.
    Captured,
}

fn pad_markup(mode: PadMode) -> &'static str {
    match mode {
        PadMode::Drag => "<span size='x-large' weight='bold'>Drag here</span>\nRight-click to click · keep dragging past the edge to keep going",
        PadMode::Idle => "<span size='x-large' weight='bold'>Click to capture the mouse</span>\nRight-click to click",
        PadMode::Captured => "<span size='x-large' weight='bold'>Mouse captured</span>\nmove to steer the TV pointer, click to click · Esc to release",
    }
}

fn pad_description(capture: bool) -> &'static str {
    if capture {
        "Click the pad to capture the mouse, then move it to steer the TV pointer; Esc releases. Works in the web browser and apps that show a pointer."
    } else {
        "Drag to move the TV pointer; tap or right-click to click. Works in the web browser and apps that show a pointer."
    }
}

/// Accumulates pointer deltas and flushes them to the TV every `FLUSH_INTERVAL`, at most
/// `MAX_BACKLOG` moves in flight, keeping the sub-pixel remainder.
struct Mover {
    ctx: Weak<Ctx>,
    speed: adw::SpinRow,
    pending: Cell<(f64, f64)>,
    /// Edge-push vector (drag fallback only).
    edge: Cell<(f64, f64)>,
    /// A drag is held or the mouse is captured.
    active: Cell<bool>,
    timer_running: Cell<bool>,
}

impl Mover {
    fn add(&self, dx: f64, dy: f64) {
        let (px, py) = self.pending.get();
        self.pending.set((px + dx, py + dy));
    }

    fn start(self: &Rc<Self>) {
        self.active.set(true);
        self.edge.set((0.0, 0.0));
        if self.timer_running.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(FLUSH_INTERVAL, move || match weak.upgrade() {
            Some(m) if m.flush() => glib::ControlFlow::Continue,
            Some(m) => {
                m.timer_running.set(false);
                glib::ControlFlow::Break
            }
            None => glib::ControlFlow::Break,
        });
    }

    fn stop(&self) {
        self.active.set(false);
        self.edge.set((0.0, 0.0));
    }

    /// One timer tick; returns whether the timer should keep running.
    fn flush(&self) -> bool {
        let Some(c) = self.ctx.upgrade() else { return false };
        if self.active.get() {
            let (ex, ey) = self.edge.get();
            self.add(ex * EDGE_GAIN, ey * EDGE_GAIN);
        }
        if c.remote_backlog() < MAX_BACKLOG {
            if let Some(((ix, iy), rest)) = scale_step(self.pending.get(), self.speed.value()) {
                self.pending.set(rest);
                c.remote(move |tv| tv.mouse_move(ix, iy));
            }
        }
        if self.active.get() || (c.remote_backlog() > 0 && self.pending.get() != (0.0, 0.0)) {
            true
        } else {
            self.pending.set((0.0, 0.0));
            false
        }
    }
}

/// Capture mode: pointer lock + UI state of the pad.
struct Capture {
    lock: PointerLock,
    mover: Rc<Mover>,
    area: gtk::DrawingArea,
    frame: gtk::Frame,
    label: gtk::Label,
}

impl Capture {
    fn captured(&self) -> bool {
        self.lock.is_locked()
    }

    fn enter(self: &Rc<Self>) -> bool {
        let mover = Rc::downgrade(&self.mover);
        let me = Rc::downgrade(self);
        let ok = self.lock.lock(
            move |dx, dy| {
                if let Some(m) = mover.upgrade() {
                    m.add(dx, dy);
                }
            },
            move || {
                if let Some(c) = me.upgrade() {
                    c.released();
                }
            },
        );
        if ok {
            self.mover.start();
            self.show(PadMode::Captured);
        }
        ok
    }

    /// Release the lock if held.
    fn release(&self) {
        if self.captured() {
            self.lock.unlock();
            self.released();
        }
    }

    /// The lock is gone (ours or the compositor's doing): back to idle.
    fn released(&self) {
        self.mover.stop();
        self.show(PadMode::Idle);
    }

    fn show(&self, mode: PadMode) {
        self.label.set_markup(pad_markup(mode));
        let captured = mode == PadMode::Captured;
        for w in [self.area.upcast_ref::<gtk::Widget>(), self.frame.upcast_ref()] {
            if captured {
                w.add_css_class("captured");
            } else {
                w.remove_css_class("captured");
            }
        }
        if captured {
            self.label.remove_css_class("dim-label");
            self.area.set_cursor_from_name(Some("none"));
        } else {
            self.label.add_css_class("dim-label");
            self.area.set_cursor_from_name(Some(PAD_CURSOR));
        }
    }
}

pub struct TouchpadPage {
    pub widget: gtk::Widget,
    entry: gtk::Entry,
    /// Owns the capture state (controllers only hold weak references).
    _capture: Option<Rc<Capture>>,
}

impl TouchpadPage {
    pub fn new(ctx: &Rc<Ctx>) -> Rc<TouchpadPage> {
        let page = adw::PreferencesPage::new();

        // --- touchpad ---------------------------------------------------------------------
        let pad_group = adw::PreferencesGroup::builder().title("Touchpad").build();

        let area = gtk::DrawingArea::new();
        area.set_content_height(280);
        area.set_hexpand(true);
        area.add_css_class("tv-touchpad");
        area.set_cursor_from_name(Some(PAD_CURSOR));
        let label = gtk::Label::new(None);
        label.set_justify(gtk::Justification::Center);
        label.set_wrap(true);
        label.add_css_class("dim-label");
        label.set_can_target(false);
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&area));
        overlay.add_overlay(&label);
        let frame = gtk::Frame::new(None);
        frame.set_child(Some(&overlay));
        frame.add_css_class("tv-touchpad-frame");
        pad_group.add(&frame);

        let speed = adw::SpinRow::with_range(0.5, 4.0, 0.25);
        speed.set_title("Pointer speed");
        speed.set_value(1.5);
        speed.set_digits(2);
        pad_group.add(&speed);
        page.add(&pad_group);

        let mover = Rc::new(Mover {
            ctx: ctx.weak(),
            speed: speed.clone(),
            pending: Cell::new((0.0, 0.0)),
            edge: Cell::new((0.0, 0.0)),
            active: Cell::new(false),
            timer_running: Cell::new(false),
        });

        let capture = match PointerLock::new(area.upcast_ref()) {
            Some(lock) => {
                pad_group.set_description(Some(pad_description(true)));
                label.set_markup(pad_markup(PadMode::Idle));
                let cap = Rc::new(Capture {
                    lock,
                    mover,
                    area: area.clone(),
                    frame: frame.clone(),
                    label: label.clone(),
                });
                install_capture(ctx, &cap);
                Some(cap)
            }
            None => {
                pad_group.set_description(Some(pad_description(false)));
                label.set_markup(pad_markup(PadMode::Drag));
                install_drag(ctx, &area, &mover);
                let weak = ctx.weak();
                let toasted = Cell::new(false);
                area.connect_map(move |_| {
                    if !toasted.replace(true) {
                        if let Some(c) = weak.upgrade() {
                            c.toast(FALLBACK_TOAST);
                        }
                    }
                });
                None
            }
        };

        // --- keyboard ---------------------------------------------------------------------
        let kb_group = adw::PreferencesGroup::builder()
            .title("Keyboard")
            .description("Types into the focused text field when the TV shows its own keyboard (browser, settings, most search boxes). YouTube and Netflix draw their own keyboard, so typing does not reach them: use the arrows there.")
            .build();
        let entry = gtk::Entry::builder()
            .placeholder_text("Text to type on the TV")
            .hexpand(true)
            .build();
        let send = gtk::Button::with_label("Send");
        send.add_css_class("suggested-action");
        let enter = gtk::Button::with_label("Enter");
        enter.set_tooltip_text(Some("Press OK/Enter on the TV"));
        let bs = gtk::Button::from_icon_name("edit-clear-symbolic");
        bs.set_tooltip_text(Some("Back"));
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&entry);
        row.append(&send);
        row.append(&enter);
        row.append(&bs);
        row.set_margin_top(6);
        kb_group.add(&row);
        page.add(&kb_group);

        let do_send = {
            let weak = ctx.weak();
            let entry = entry.clone();
            move || {
                let text = entry.text().to_string();
                if text.is_empty() {
                    return;
                }
                if let Some(c) = weak.upgrade() {
                    c.remote(move |tv| tv.send_text(&text));
                    entry.set_text("");
                }
            }
        };
        let s1 = do_send.clone();
        send.connect_clicked(move |_| s1());
        entry.connect_activate(move |_| do_send());
        let weak = ctx.weak();
        enter.connect_clicked(move |_| {
            if let Some(c) = weak.upgrade() {
                c.key(Key::Enter);
            }
        });
        let weak = ctx.weak();
        bs.connect_clicked(move |_| {
            if let Some(c) = weak.upgrade() {
                c.key(Key::Return);
            }
        });

        Rc::new(TouchpadPage { widget: page.upcast(), entry, _capture: capture })
    }

    /// The TV opened its on-screen keyboard: focus our entry.
    pub fn focus_entry(&self) {
        self.entry.grab_focus();
    }
}

/// Send one click to the TV.
fn tv_click(ctx: &Weak<Ctx>) {
    if let Some(c) = ctx.upgrade() {
        c.remote(|tv| tv.mouse_click());
    }
}

/// Capture mode: click captures, clicks while captured go to the TV, Esc / focus loss /
/// leaving the page releases.
fn install_capture(ctx: &Rc<Ctx>, cap: &Rc<Capture>) {
    let area = &cap.area;

    let primary = gtk::GestureClick::new();
    primary.set_button(gdk::BUTTON_PRIMARY);
    {
        let cap = Rc::downgrade(cap);
        let weak = ctx.weak();
        primary.connect_pressed(move |g, _, _, _| {
            let Some(cap) = cap.upgrade() else { return };
            g.set_state(gtk::EventSequenceState::Claimed);
            if cap.captured() {
                tv_click(&weak);
            } else if !cap.enter() {
                if let Some(c) = weak.upgrade() {
                    c.toast("Could not capture the mouse");
                }
            }
        });
    }
    area.add_controller(primary);

    // Right-click = click on the TV, captured or not.
    let secondary = gtk::GestureClick::new();
    secondary.set_button(gdk::BUTTON_SECONDARY);
    let weak = ctx.weak();
    secondary.connect_pressed(move |_, _, _, _| tv_click(&weak));
    area.add_controller(secondary);

    // Leaving the page (stack switch) or destroying the window unmaps the pad.
    {
        let cap = Rc::downgrade(cap);
        area.connect_unmap(move |_| {
            if let Some(cap) = cap.upgrade() {
                cap.release();
            }
        });
    }
    {
        let cap = Rc::downgrade(cap);
        ctx.window.connect_is_active_notify(move |w| {
            if !w.is_active() {
                if let Some(cap) = cap.upgrade() {
                    cap.release();
                }
            }
        });
    }
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let cap = Rc::downgrade(cap);
        keys.connect_key_pressed(move |_, keyval, _, _| match cap.upgrade() {
            Some(cap) if keyval == gdk::Key::Escape && cap.captured() => {
                cap.release();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
    }
    ctx.window.add_controller(keys);
}

/// Fallback: drag to move (with edge push past the pad), tap or right-click to click.
fn install_drag(ctx: &Rc<Ctx>, area: &gtk::DrawingArea, mover: &Rc<Mover>) {
    let right = gtk::GestureClick::new();
    right.set_button(gdk::BUTTON_SECONDARY);
    let weak = ctx.weak();
    right.connect_pressed(move |_, _, _, _| tv_click(&weak));
    area.add_controller(right);

    // Drag offset already consumed, and the drag start point (area coordinates).
    let last = Rc::new(Cell::new((0.0f64, 0.0f64)));
    let start = Rc::new(Cell::new((0.0f64, 0.0f64)));

    let drag = gtk::GestureDrag::new();
    drag.set_button(gdk::BUTTON_PRIMARY);
    {
        let last = last.clone();
        let start = start.clone();
        let mover = mover.clone();
        drag.connect_drag_begin(move |_, x, y| {
            last.set((0.0, 0.0));
            start.set((x, y));
            mover.start();
        });
    }
    {
        let mover = mover.clone();
        let area_w = area.clone();
        drag.connect_drag_update(move |_, ox, oy| {
            let (lx, ly) = last.get();
            mover.add(ox - lx, oy - ly);
            last.set((ox, oy));
            let (sx, sy) = start.get();
            mover.edge.set(outside(
                sx + ox,
                sy + oy,
                f64::from(area_w.width()),
                f64::from(area_w.height()),
            ));
        });
    }
    {
        let mover = mover.clone();
        let weak = ctx.weak();
        drag.connect_drag_end(move |_, ox, oy| {
            mover.stop();
            if ox.hypot(oy) < TAP_SLOP {
                mover.pending.set((0.0, 0.0));
                tv_click(&weak);
            }
        });
    }
    area.add_controller(drag);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_rounds_and_keeps_remainder() {
        let ((x, y), (rx, ry)) = scale_step((3.3, -1.0), 1.5).unwrap();
        assert_eq!((x, y), (5, -2)); // 4.95 → 5, -1.5 → -2 (round half away from zero)
        assert!((rx - (3.3 - 5.0 / 1.5)).abs() < 1e-9);
        assert!((ry - (-1.0 + 2.0 / 1.5)).abs() < 1e-9);
    }

    #[test]
    fn tiny_motion_accumulates_until_a_pixel() {
        let k = 1.0;
        let mut p = (0.0, 0.0);
        let mut sent = (0, 0);
        for _ in 0..10 {
            p.0 += 0.3;
            if let Some(((x, y), rest)) = scale_step(p, k) {
                sent.0 += x;
                sent.1 += y;
                p = rest;
            }
        }
        // 10 × 0.3 = 3 px in total: nothing is lost to rounding.
        assert!((sent.0 as f64 + p.0 - 3.0).abs() < 1e-9);
        assert_eq!(sent.1, 0);
    }

    #[test]
    fn zero_step_and_bad_speed() {
        assert!(scale_step((0.2, -0.2), 1.0).is_none());
        assert!(scale_step((10.0, 10.0), 0.0).is_none());
        assert!(scale_step((10.0, 10.0), f64::NAN).is_none());
    }

    #[test]
    fn labels_per_mode() {
        assert!(pad_markup(PadMode::Drag).contains("Drag here"));
        assert!(pad_markup(PadMode::Idle).contains("Click to capture the mouse"));
        assert!(pad_markup(PadMode::Idle).contains("Right-click to click"));
        assert!(pad_markup(PadMode::Captured).contains("Mouse captured"));
        assert!(pad_markup(PadMode::Captured).contains("Esc to release"));
        for m in [PadMode::Drag, PadMode::Idle, PadMode::Captured] {
            assert!(gtk::pango::parse_markup(pad_markup(m), '\0').is_ok(), "{m:?}");
        }
        assert!(pad_description(true).contains("Esc"));
        assert!(pad_description(false).contains("Drag"));
    }

    #[test]
    fn outside_is_zero_inside_and_capped() {
        assert_eq!(outside(10.0, 10.0, 100.0, 100.0), (0.0, 0.0));
        assert_eq!(outside(-5.0, 120.0, 100.0, 100.0), (-5.0, 20.0));
        assert_eq!(outside(-1000.0, 1000.0, 100.0, 100.0), (-EDGE_MAX_DIST, EDGE_MAX_DIST));
    }
}
