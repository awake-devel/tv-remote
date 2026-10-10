//! Shared UI context and the worker bridge.
//!
//! Every `tvremote_core` call that can block runs on a worker thread. Results come
//! back to the GTK main loop through `async-channel` + `glib::spawn_future_local`.
//!
//! * [`Ctx::spawn`]: one-off call on a fresh thread, result handed to a UI callback.
//! * [`Ctx::spawn_with_progress`]: same, with progress messages streamed to the UI.
//! * [`Ctx::remote`]: remote-control commands (keys, text, mouse) on a single serial
//!   worker, so rapid key presses reach the TV in order. Errors become toasts.
//!
//! Several TVs: [`Ctx::tv`] is the TV the pages control; [`Ctx::switch_tv`] changes it
//! (it becomes the active TV) and calls every [`Ctx::on_tv_changed`] listener so pages
//! rebind. Work already queued keeps the TV it was queued for.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;
use std::collections::HashMap;

use tvremote_core::{CloudState, DeviceInfo, Fleet, PowerState, Tv};

/// Last LAN power state and SmartThings state seen for one TV (status dots).
#[derive(Debug, Clone, Default)]
pub struct TvDots {
    pub power: Option<PowerState>,
    pub cloud: Option<CloudState>,
}

type RemoteJob = (Tv, Box<dyn FnOnce(&Tv) -> tvremote_core::Result<()> + Send>);
type TvListener = Box<dyn Fn()>;
type VaapiListener = Box<dyn Fn(bool)>;
type Navigator = Rc<dyn Fn(&str)>;

/// State shared by every page.
pub struct Ctx {
    /// All TVs.
    pub fleet: Fleet,
    tv: RefCell<Tv>,
    tv_listeners: RefCell<Vec<TvListener>>,
    /// Status dots per TV id, kept up to date by the window's event pump.
    pub dots: RefCell<HashMap<String, TvDots>>,
    pub window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    remote_tx: std::sync::mpsc::Sender<RemoteJob>,
    remote_backlog: Arc<AtomicUsize>,
    last_toast: RefCell<Option<(String, Instant)>>,
    /// `vaapi_available()`, probed once in the background (`None` until known).
    pub vaapi: Cell<Option<bool>>,
    vaapi_listeners: RefCell<Vec<VaapiListener>>,
    /// Last device info fetched by the Device page (used for hints elsewhere).
    pub device_info: RefCell<Option<DeviceInfo>>,
    navigator: RefCell<Option<Navigator>>,
}

impl Ctx {
    pub fn new(fleet: Fleet, window: adw::ApplicationWindow, toasts: adw::ToastOverlay) -> Rc<Ctx> {
        let (remote_tx, remote_rx) = std::sync::mpsc::channel::<RemoteJob>();
        let (err_tx, err_rx) = async_channel::unbounded::<String>();
        let backlog = Arc::new(AtomicUsize::new(0));
        let tv = fleet.active();
        let ctx = Rc::new(Ctx {
            fleet,
            tv: RefCell::new(tv),
            tv_listeners: RefCell::new(Vec::new()),
            dots: RefCell::new(HashMap::new()),
            window,
            toasts,
            remote_tx,
            remote_backlog: backlog.clone(),
            last_toast: RefCell::new(None),
            vaapi: Cell::new(None),
            vaapi_listeners: RefCell::new(Vec::new()),
            device_info: RefCell::new(None),
            navigator: RefCell::new(None),
        });

        // Serial remote-command worker.
        let spawned = std::thread::Builder::new()
            .name("tv-remote-cmds".into())
            .spawn(move || {
                for (tv, job) in remote_rx {
                    let res = job(&tv);
                    backlog.fetch_sub(1, Ordering::SeqCst);
                    if let Err(e) = res {
                        if err_tx.send_blocking(e.to_string()).is_err() {
                            break;
                        }
                    }
                }
            });
        if let Err(e) = spawned {
            ctx.toast(&format!("Cannot start the remote worker: {e}"));
        }
        let weak = Rc::downgrade(&ctx);
        glib::spawn_future_local(async move {
            while let Ok(msg) = err_rx.recv().await {
                match weak.upgrade() {
                    Some(c) => c.toast(&msg),
                    None => break,
                }
            }
        });

        // Probe VAAPI once in the background (runs a tiny local ffmpeg encode).
        let weak = Rc::downgrade(&ctx);
        ctx.spawn(
            |_tv| tvremote_core::cast::vaapi_available(),
            move |ok| {
                if let Some(c) = weak.upgrade() {
                    c.vaapi.set(Some(ok));
                    for l in c.vaapi_listeners.borrow().iter() {
                        l(ok);
                    }
                }
            },
        );
        ctx
    }

    pub fn weak(self: &Rc<Self>) -> Weak<Ctx> {
        Rc::downgrade(self)
    }

    /// The TV the pages control.
    pub fn tv(&self) -> Tv {
        self.tv.borrow().clone()
    }

    /// Control TV `id` from now on: it becomes the active TV (saved), its remote channel
    /// starts, and every [`Ctx::on_tv_changed`] listener runs. A running cast keeps
    /// playing on the TV it started on.
    pub fn switch_tv(self: &Rc<Self>, id: &str) {
        if self.fleet.tv(id).is_ok_and(|t| t.ptr_eq(&self.tv())) {
            return;
        }
        let tv = match self.fleet.tv(id) {
            Ok(tv) => tv,
            Err(e) => return self.toast(&e.to_string()),
        };
        *self.tv.borrow_mut() = tv.clone();
        *self.device_info.borrow_mut() = None;
        let fleet = self.fleet.clone();
        let idc = id.to_string();
        let w = self.weak();
        self.spawn_on(
            tv,
            move |tv| {
                let r = fleet.set_active(&idc);
                if !tv.host().trim().is_empty() {
                    tv.connect();
                }
                r
            },
            move |r| {
                if let (Some(c), Err(e)) = (w.upgrade(), r) {
                    c.toast(&format!("Could not save the active TV: {e}"));
                }
            },
        );
        let listeners = self.tv_listeners.borrow();
        for l in listeners.iter() {
            l();
        }
    }

    /// Run `f` after every TV switch.
    pub fn on_tv_changed(&self, f: impl Fn() + 'static) {
        self.tv_listeners.borrow_mut().push(Box::new(f));
    }

    /// Show a toast; the same message is not repeated within 3 s.
    pub fn toast(&self, msg: &str) {
        self.toast_for(msg, 4);
    }

    /// Toast with a custom timeout (seconds).
    pub fn toast_for(&self, msg: &str, timeout: u32) {
        let now = Instant::now();
        {
            let mut last = self.last_toast.borrow_mut();
            if let Some((m, t)) = last.as_ref() {
                if m == msg && now.duration_since(*t) < Duration::from_secs(3) {
                    return;
                }
            }
            *last = Some((msg.to_string(), now));
        }
        let t = adw::Toast::new(msg);
        t.set_timeout(timeout);
        self.toasts.add_toast(t);
    }

    /// Toast with a button that navigates to a page.
    pub fn toast_nav(self: &Rc<Self>, msg: &str, button: &str, page: &'static str) {
        let t = adw::Toast::new(msg);
        t.set_timeout(6);
        t.set_button_label(Some(button));
        let weak = self.weak();
        t.connect_button_clicked(move |_| {
            if let Some(c) = weak.upgrade() {
                c.navigate(page);
            }
        });
        self.toasts.add_toast(t);
    }

    /// Run `work` on a worker thread and hand its result to `done` on the UI thread.
    pub fn spawn<T, F, C>(self: &Rc<Self>, work: F, done: C)
    where
        T: Send + 'static,
        F: FnOnce(Tv) -> T + Send + 'static,
        C: FnOnce(T) + 'static,
    {
        self.spawn_on(self.tv(), work, done)
    }

    /// Like [`Ctx::spawn`] for a given TV (not necessarily the current one).
    pub fn spawn_on<T, F, C>(self: &Rc<Self>, tv: Tv, work: F, done: C)
    where
        T: Send + 'static,
        F: FnOnce(Tv) -> T + Send + 'static,
        C: FnOnce(T) + 'static,
    {
        let (tx, rx) = async_channel::bounded::<T>(1);
        let spawned = std::thread::Builder::new()
            .name("tv-worker".into())
            .spawn(move || {
                let _ = tx.send_blocking(work(tv));
            });
        if let Err(e) = spawned {
            self.toast(&format!("Cannot start a worker thread: {e}"));
            return;
        }
        let weak = self.weak();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(v) => done(v),
                Err(_) => {
                    if let Some(c) = weak.upgrade() {
                        c.toast("Internal error: a background task stopped unexpectedly");
                    }
                }
            }
        });
    }

    /// Like [`Ctx::spawn`], with progress values sent from the worker.
    pub fn spawn_with_progress<T, P, F, PC, C>(self: &Rc<Self>, work: F, on_progress: PC, done: C)
    where
        T: Send + 'static,
        P: Send + 'static,
        F: FnOnce(Tv, &dyn Fn(P)) -> T + Send + 'static,
        PC: Fn(P) + 'static,
        C: FnOnce(T) + 'static,
    {
        enum Msg<P, T> {
            Progress(P),
            Done(T),
        }
        let tv = self.tv();
        let (tx, rx) = async_channel::unbounded::<Msg<P, T>>();
        let spawned = std::thread::Builder::new()
            .name("tv-worker-progress".into())
            .spawn(move || {
                let ptx = tx.clone();
                let progress = move |p: P| {
                    let _ = ptx.send_blocking(Msg::Progress(p));
                };
                let res = work(tv, &progress);
                let _ = tx.send_blocking(Msg::Done(res));
            });
        if let Err(e) = spawned {
            self.toast(&format!("Cannot start a worker thread: {e}"));
            return;
        }
        let weak = self.weak();
        glib::spawn_future_local(async move {
            let mut done = Some(done);
            while let Ok(m) = rx.recv().await {
                match m {
                    Msg::Progress(p) => on_progress(p),
                    Msg::Done(v) => {
                        if let Some(d) = done.take() {
                            d(v);
                        }
                        return;
                    }
                }
            }
            if done.is_some() {
                if let Some(c) = weak.upgrade() {
                    c.toast("Internal error: a background task stopped unexpectedly");
                }
            }
        });
    }

    /// Queue a remote-control command on the serial worker; errors become toasts.
    pub fn remote(&self, job: impl FnOnce(&Tv) -> tvremote_core::Result<()> + Send + 'static) {
        self.remote_backlog.fetch_add(1, Ordering::SeqCst);
        if self.remote_tx.send((self.tv(), Box::new(job))).is_err() {
            self.remote_backlog.fetch_sub(1, Ordering::SeqCst);
            self.toast("The remote worker stopped; restart the app");
        }
    }

    /// Number of queued, unfinished remote commands (used to throttle mouse moves).
    pub fn remote_backlog(&self) -> usize {
        self.remote_backlog.load(Ordering::SeqCst)
    }

    /// Send a key click.
    pub fn key(&self, key: tvremote_core::Key) {
        self.remote(move |tv| tv.send_key(&key));
    }

    /// Run `f` now if VAAPI is known, and every time it becomes known.
    pub fn on_vaapi(&self, f: impl Fn(bool) + 'static) {
        if let Some(v) = self.vaapi.get() {
            f(v);
        }
        self.vaapi_listeners.borrow_mut().push(Box::new(f));
    }

    pub fn set_navigator(&self, f: Navigator) {
        *self.navigator.borrow_mut() = Some(f);
    }

    /// Switch the visible page (`remote`, `touchpad`, `apps`, `cast`, `picture`, `device`).
    pub fn navigate(&self, page: &str) {
        let nav = self.navigator.borrow().clone();
        if let Some(n) = nav {
            n(page);
        }
    }

    /// Ask for confirmation with an `adw::AlertDialog`; `on_yes` runs if confirmed.
    pub fn confirm(
        &self,
        heading: &str,
        body: &str,
        yes_label: &str,
        destructive: bool,
        on_yes: impl Fn() + 'static,
    ) {
        let d = adw::AlertDialog::new(Some(heading), Some(body));
        d.add_response("cancel", "Cancel");
        d.add_response("yes", yes_label);
        d.set_response_appearance(
            "yes",
            if destructive {
                adw::ResponseAppearance::Destructive
            } else {
                adw::ResponseAppearance::Suggested
            },
        );
        d.set_default_response(Some("cancel"));
        d.set_close_response("cancel");
        d.connect_response(None, move |_, resp| {
            if resp == "yes" {
                on_yes();
            }
        });
        d.present(Some(&self.window));
    }
}
