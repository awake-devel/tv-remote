//! Wayland pointer lock + relative motion for one widget's window.
//!
//! GTK 4 has no pointer-lock API, so this talks to the compositor directly through
//! `zwp_pointer_constraints_v1` (lock the pointer on our `wl_surface`) and
//! `zwp_relative_pointer_manager_v1` (unbounded relative motion while locked).
//!
//! It shares GTK's own `wl_display` connection but uses its **own event queue**:
//! GTK keeps reading the socket as usual, libwayland files the events for our objects
//! into our queue, and a short glib timeout drains it with `dispatch_pending` (which
//! never reads the socket, so GTK's events are never stolen).
//!
//! No `unsafe` is needed: gdk4-wayland hands out the `wl_display`, `wl_surface` and
//! `wl_pointer` proxies through safe wrappers, and the connection is rebuilt from the
//! display proxy's backend.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use gdk4_wayland::prelude::*;
use gdk4_wayland::wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use gdk4_wayland::wayland_client::protocol::wl_registry::WlRegistry;
use gdk4_wayland::wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use gdk4_wayland::{WaylandDevice, WaylandDisplay, WaylandSurface};
use gtk::glib;
use gtk::prelude::*;
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::{
    self, ZwpLockedPointerV1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::{
    Lifetime, ZwpPointerConstraintsV1,
};
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1;
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::{
    self, ZwpRelativePointerV1,
};

/// How often the private queue is drained while locked (~120 Hz).
const DISPATCH_INTERVAL: Duration = Duration::from_millis(8);

/// What the queue's handlers collected since the last drain.
#[derive(Default)]
struct State {
    /// Summed accelerated relative motion, surface-local pixels.
    motion: (f64, f64),
    /// The compositor sent `unlocked` (a oneshot lock is then defunct).
    unlocked: bool,
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpPointerConstraintsV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpPointerConstraintsV1,
        _: <ZwpPointerConstraintsV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpRelativePointerManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpRelativePointerManagerV1,
        _: <ZwpRelativePointerManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpLockedPointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_locked_pointer_v1::Event::Unlocked = event {
            state.unlocked = true;
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = event {
            state.motion.0 += dx;
            state.motion.1 += dy;
        }
    }
}

/// A lock in progress.
struct Active {
    locked: ZwpLockedPointerV1,
    relative: ZwpRelativePointerV1,
    on_motion: Rc<dyn Fn(f64, f64)>,
    on_unlocked: Rc<dyn Fn()>,
}

impl Active {
    fn destroy(self) {
        self.relative.destroy();
        self.locked.destroy();
    }
}

struct Inner {
    widget: glib::WeakRef<gtk::Widget>,
    conn: Connection,
    queue: RefCell<EventQueue<State>>,
    state: RefCell<State>,
    qh: QueueHandle<State>,
    constraints: ZwpPointerConstraintsV1,
    relative_mgr: ZwpRelativePointerManagerV1,
    _globals: GlobalList,
    active: RefCell<Option<Active>>,
    /// Bumped on every lock/unlock so a stale dispatch timer stops by itself.
    generation: Cell<u64>,
}

impl Inner {
    /// Destroy the protocol objects of the current lock, if any.
    fn teardown(&self) {
        let active = self.active.borrow_mut().take();
        if let Some(a) = active {
            self.generation.set(self.generation.get().wrapping_add(1));
            a.destroy();
            let _ = self.conn.flush();
        }
    }

    /// Drain our queue; returns whether the timer should keep running.
    fn tick(&self, generation: u64) -> bool {
        if self.generation.get() != generation || self.active.borrow().is_none() {
            return false;
        }
        let dispatched = {
            let mut queue = self.queue.borrow_mut();
            let mut state = self.state.borrow_mut();
            queue.dispatch_pending(&mut state).is_ok()
        };
        let (motion, unlocked) = {
            let mut st = self.state.borrow_mut();
            let out = (st.motion, st.unlocked || !dispatched);
            *st = State::default();
            out
        };
        // Clone the callbacks out so they run with no RefCell borrowed (they may call
        // back into `unlock`).
        let Some((on_motion, on_unlocked)) = self
            .active
            .borrow()
            .as_ref()
            .map(|a| (a.on_motion.clone(), a.on_unlocked.clone()))
        else {
            return false;
        };
        if motion != (0.0, 0.0) {
            on_motion(motion.0, motion.1);
        }
        if unlocked {
            self.teardown();
            on_unlocked();
            return false;
        }
        self.generation.get() == generation
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(a) = self.active.get_mut().take() {
            a.destroy();
        }
        self.constraints.destroy();
        self.relative_mgr.destroy();
        let _ = self.conn.flush();
    }
}

/// Pointer lock for the window that holds a widget. Cheap to clone (shared handle).
#[derive(Clone)]
pub struct PointerLock(Rc<Inner>);

impl PointerLock {
    /// `None` when the display is not Wayland or the compositor lacks either global.
    pub fn new(widget: &gtk::Widget) -> Option<PointerLock> {
        let display = widget.display().downcast::<WaylandDisplay>().ok()?;
        let wl_display = display.wl_display()?;
        let backend = wl_display.backend().upgrade()?;
        let conn = Connection::from_backend(backend);
        // One roundtrip on a private queue to list the globals.
        let (globals, queue) = registry_queue_init::<State>(&conn).ok()?;
        let qh = queue.handle();
        let constraints: ZwpPointerConstraintsV1 = globals.bind(&qh, 1..=1, ()).ok()?;
        let relative_mgr: ZwpRelativePointerManagerV1 = match globals.bind(&qh, 1..=1, ()) {
            Ok(m) => m,
            Err(_) => {
                constraints.destroy();
                let _ = conn.flush();
                return None;
            }
        };
        Some(PointerLock(Rc::new(Inner {
            widget: widget.downgrade(),
            conn,
            queue: RefCell::new(queue),
            state: RefCell::new(State::default()),
            qh,
            constraints,
            relative_mgr,
            _globals: globals,
            active: RefCell::new(None),
            generation: Cell::new(0),
        })))
    }

    pub fn is_locked(&self) -> bool {
        self.0.active.borrow().is_some()
    }

    /// Lock the pointer on the widget's window surface and stream relative motion
    /// (accelerated, surface pixels) to `on_motion`. `on_unlocked` runs if the
    /// compositor ends the lock (focus change, compositor policy); it does not run on
    /// [`PointerLock::unlock`]. Returns false if the surface or pointer is unavailable.
    pub fn lock(&self, on_motion: impl Fn(f64, f64) + 'static, on_unlocked: impl Fn() + 'static) -> bool {
        let inner = &self.0;
        inner.teardown();
        let Some(widget) = inner.widget.upgrade() else { return false };
        let Some(surface) = widget.native().and_then(|n| n.surface()) else { return false };
        let Some(wl_surface) = surface.downcast::<WaylandSurface>().ok().and_then(|s| s.wl_surface())
        else {
            return false;
        };
        let Some(wl_pointer) = widget
            .display()
            .default_seat()
            .and_then(|s| s.pointer())
            .and_then(|p| p.downcast::<WaylandDevice>().ok())
            .and_then(|d| d.wl_pointer())
        else {
            return false;
        };

        *inner.state.borrow_mut() = State::default();
        // Oneshot: once the compositor sends `unlocked`, the lock is over for good and
        // we tear it down; a new click makes a new lock.
        let locked = inner.constraints.lock_pointer(
            &wl_surface,
            &wl_pointer,
            None,
            Lifetime::Oneshot,
            &inner.qh,
            (),
        );
        let relative = inner.relative_mgr.get_relative_pointer(&wl_pointer, &inner.qh, ());
        if inner.conn.flush().is_err() {
            relative.destroy();
            locked.destroy();
            return false;
        }
        *inner.active.borrow_mut() = Some(Active {
            locked,
            relative,
            on_motion: Rc::new(on_motion),
            on_unlocked: Rc::new(on_unlocked),
        });
        let generation = inner.generation.get().wrapping_add(1);
        inner.generation.set(generation);

        let weak: Weak<Inner> = Rc::downgrade(inner);
        glib::timeout_add_local(DISPATCH_INTERVAL, move || match weak.upgrade() {
            Some(i) if i.tick(generation) => glib::ControlFlow::Continue,
            _ => glib::ControlFlow::Break,
        });
        true
    }

    /// Release the lock (no-op when not locked). Does not call `on_unlocked`.
    pub fn unlock(&self) {
        self.0.teardown();
    }
}
