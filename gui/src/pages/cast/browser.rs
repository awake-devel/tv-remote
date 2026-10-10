//! Built-in media browser: places (Vidéos, Téléchargements, mounted drives), folders and
//! media files with size; ffprobe runs lazily in the background (bounded concurrency,
//! cached by path + size + mtime) and fills in resolution / codec and a compatibility badge.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::SystemTime;

use adw::prelude::*;
use gtk::glib;
use tvremote_core::cast::{check_compat, MediaInfo};

use crate::ctx::Ctx;
use crate::format::{compat_badge, compat_css, human_size, media_kind, media_summary, MediaKind};

/// Concurrent ffprobe processes.
const MAX_PROBES: usize = 2;

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
    mtime: Option<SystemTime>,
    kind: Option<MediaKind>,
}

type CacheKey = (PathBuf, u64, Option<SystemTime>);
type SelectFn = Box<dyn Fn(PathBuf, Option<MediaInfo>)>;

/// A bookmark in the places dropdown.
#[derive(Debug, Clone)]
pub struct Place {
    pub name: String,
    pub path: PathBuf,
}

/// Bookmarks: ~/Vidéos, ~/Téléchargements (XDG dirs), home, mounted drives.
pub fn places() -> Vec<Place> {
    let home = glib::home_dir();
    let mut v = Vec::new();
    let mut push = |name: String, path: PathBuf| {
        if path.is_dir() && !v.iter().any(|p: &Place| p.path == path) {
            v.push(Place { name, path });
        }
    };
    let videos = glib::user_special_dir(glib::UserDirectory::Videos)
        .unwrap_or_else(|| home.join("Vidéos"));
    push(dir_label(&videos, "Videos"), videos);
    push("Vidéos".into(), home.join("Vidéos"));
    let dl = glib::user_special_dir(glib::UserDirectory::Downloads)
        .unwrap_or_else(|| home.join("Téléchargements"));
    push(dir_label(&dl, "Downloads"), dl);
    push("Téléchargements".into(), home.join("Téléchargements"));
    push("Home".into(), home.clone());
    let user = glib::user_name().to_string_lossy().to_string();
    for root in [
        PathBuf::from("/run/media").join(&user),
        PathBuf::from("/media").join(&user),
        PathBuf::from("/media"),
    ] {
        if let Ok(rd) = std::fs::read_dir(&root) {
            let mut mounts: Vec<PathBuf> = rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n != user.as_str()))
                .collect();
            mounts.sort();
            for m in mounts {
                let n = m
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                push(format!("💾 {n}"), m);
            }
        }
    }
    v
}

fn dir_label(p: &Path, fallback: &str) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| fallback.to_string())
}

fn list_dir(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let Ok(e) = e else { continue };
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = e.path();
        // Follow symlinks for the type and size.
        let Ok(md) = std::fs::metadata(&path) else { continue };
        let is_dir = md.is_dir();
        let kind = if is_dir { None } else { media_kind(&path) };
        if !is_dir && kind.is_none() {
            continue;
        }
        out.push(Entry {
            name,
            path,
            is_dir,
            size: md.len(),
            mtime: md.modified().ok(),
            kind,
        });
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

pub struct Browser {
    pub widget: gtk::Widget,
    ctx: Weak<Ctx>,
    places: RefCell<Vec<Place>>,
    places_dd: gtk::DropDown,
    path_label: gtk::Label,
    list: gtk::ListBox,
    stack: gtk::Stack,
    empty: adw::StatusPage,
    up: gtk::Button,
    cwd: RefCell<Option<PathBuf>>,
    gen: Cell<u64>,
    rows: RefCell<HashMap<PathBuf, (adw::ActionRow, gtk::Label, Entry)>>,
    cache: RefCell<HashMap<CacheKey, Result<MediaInfo, String>>>,
    queue: RefCell<VecDeque<Entry>>,
    active: Cell<usize>,
    suppress_place: Cell<bool>,
    on_select: RefCell<Option<SelectFn>>,
}

impl Browser {
    pub fn new(ctx: &Rc<Ctx>) -> Rc<Browser> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.set_margin_top(6);
        bar.set_margin_bottom(6);
        bar.set_margin_start(6);
        bar.set_margin_end(6);
        let up = gtk::Button::from_icon_name("go-up-symbolic");
        up.set_tooltip_text(Some("Parent folder"));
        up.add_css_class("flat");
        let places_dd = gtk::DropDown::from_strings(&[]);
        places_dd.set_hexpand(true);
        places_dd.set_tooltip_text(Some("Places"));
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.set_tooltip_text(Some("Refresh places and folder"));
        refresh.add_css_class("flat");
        let open = gtk::Button::with_label("Open…");
        open.set_tooltip_text(Some("Choose a file with the system file chooser"));
        bar.append(&up);
        bar.append(&places_dd);
        bar.append(&refresh);
        bar.append(&open);
        root.append(&bar);

        let path_label = gtk::Label::new(None);
        path_label.set_xalign(0.0);
        path_label.set_ellipsize(gtk::pango::EllipsizeMode::Start);
        path_label.add_css_class("caption");
        path_label.add_css_class("dim-label");
        path_label.set_margin_start(12);
        path_label.set_margin_end(12);
        path_label.set_margin_bottom(6);
        root.append(&path_label);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        list.set_selection_mode(gtk::SelectionMode::Single);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();
        let empty = adw::StatusPage::builder()
            .icon_name("folder-videos-symbolic")
            .title("No media here")
            .description("Folders and video, audio and image files appear here. You can also drop a file on this page.")
            .build();
        empty.add_css_class("compact");
        let spinner = adw::Spinner::new();
        spinner.set_size_request(32, 32);
        spinner.set_halign(gtk::Align::Center);
        spinner.set_valign(gtk::Align::Center);
        let stack = gtk::Stack::new();
        stack.add_named(&scroll, Some("list"));
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&spinner, Some("loading"));
        root.append(&stack);

        let this = Rc::new(Browser {
            widget: root.upcast(),
            ctx: ctx.weak(),
            places: RefCell::new(Vec::new()),
            places_dd,
            path_label,
            list,
            stack,
            empty,
            up: up.clone(),
            cwd: RefCell::new(None),
            gen: Cell::new(0),
            rows: RefCell::new(HashMap::new()),
            cache: RefCell::new(HashMap::new()),
            queue: RefCell::new(VecDeque::new()),
            active: Cell::new(0),
            suppress_place: Cell::new(false),
            on_select: RefCell::new(None),
        });

        let w = Rc::downgrade(&this);
        up.connect_clicked(move |_| {
            let Some(b) = w.upgrade() else { return };
            let parent = b.cwd.borrow().as_ref().and_then(|p| p.parent().map(Path::to_path_buf));
            if let Some(p) = parent {
                b.open_dir(p);
            }
        });
        let w = Rc::downgrade(&this);
        refresh.connect_clicked(move |_| {
            if let Some(b) = w.upgrade() {
                b.reload_places();
                let cwd = b.cwd.borrow().clone();
                if let Some(d) = cwd {
                    b.open_dir(d);
                }
            }
        });
        let w = Rc::downgrade(&this);
        this.places_dd.connect_selected_notify(move |dd| {
            let Some(b) = w.upgrade() else { return };
            if b.suppress_place.get() {
                return;
            }
            let p = b.places.borrow().get(dd.selected() as usize).map(|p| p.path.clone());
            if let Some(p) = p {
                b.open_dir(p);
            }
        });
        let w = Rc::downgrade(&this);
        open.connect_clicked(move |_| {
            let Some(b) = w.upgrade() else { return };
            let Some(ctx) = b.ctx.upgrade() else { return };
            let dialog = gtk::FileDialog::builder().title("Choose a file to cast").modal(true).build();
            if let Some(d) = b.cwd.borrow().as_ref() {
                dialog.set_initial_folder(Some(&gtk::gio::File::for_path(d)));
            }
            let w2 = Rc::downgrade(&b);
            dialog.open(Some(&ctx.window), gtk::gio::Cancellable::NONE, move |res| {
                let Some(b) = w2.upgrade() else { return };
                match res {
                    Ok(file) => match file.path() {
                        Some(p) => b.select_path(p),
                        None => {
                            if let Some(c) = b.ctx.upgrade() {
                                c.toast("Only local files can be cast (paste remote links in the URL field)");
                            }
                        }
                    },
                    Err(e) => {
                        if !e.matches(gtk::DialogError::Dismissed) && !e.matches(gtk::DialogError::Cancelled) {
                            if let Some(c) = b.ctx.upgrade() {
                                c.toast(&format!("File chooser: {e}"));
                            }
                        }
                    }
                }
            });
        });
        let w = Rc::downgrade(&this);
        this.list.connect_row_activated(move |_, row| {
            let Some(b) = w.upgrade() else { return };
            let entry = b
                .rows
                .borrow()
                .values()
                .find(|(r, _, _)| r.upcast_ref::<gtk::ListBoxRow>() == row)
                .map(|(_, _, e)| e.clone());
            let Some(e) = entry else { return };
            if e.is_dir {
                b.open_dir(e.path);
            } else {
                b.emit_select(&e);
            }
        });

        // Re-render badges when VAAPI availability becomes known.
        let w = Rc::downgrade(&this);
        ctx.on_vaapi(move |_| {
            if let Some(b) = w.upgrade() {
                b.refresh_badges();
            }
        });

        this.reload_places();
        let first = this.places.borrow().first().map(|p| p.path.clone());
        if let Some(p) = first {
            this.open_dir(p);
        }
        this
    }

    pub fn connect_select(&self, f: impl Fn(PathBuf, Option<MediaInfo>) + 'static) {
        *self.on_select.borrow_mut() = Some(Box::new(f));
    }

    fn reload_places(&self) {
        let pl = places();
        let names: Vec<&str> = pl.iter().map(|p| p.name.as_str()).collect();
        self.suppress_place.set(true);
        self.places_dd.set_model(Some(&gtk::StringList::new(&names)));
        let cwd = self.cwd.borrow().clone();
        let sel = cwd
            .and_then(|c| pl.iter().position(|p| p.path == c))
            .unwrap_or(0);
        self.places_dd.set_selected(sel as u32);
        self.suppress_place.set(false);
        *self.places.borrow_mut() = pl;
    }

    /// Select a file from outside the list (file chooser, drag-and-drop).
    pub fn select_path(self: &Rc<Self>, path: PathBuf) {
        let Ok(md) = std::fs::metadata(&path) else {
            if let Some(c) = self.ctx.upgrade() {
                c.toast("Cannot read that file");
            }
            return;
        };
        if md.is_dir() {
            self.open_dir(path);
            return;
        }
        let e = Entry {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            kind: media_kind(&path),
            is_dir: false,
            size: md.len(),
            mtime: md.modified().ok(),
            path,
        };
        self.emit_select(&e);
    }

    fn emit_select(&self, e: &Entry) {
        let cached = self
            .cache
            .borrow()
            .get(&(e.path.clone(), e.size, e.mtime))
            .and_then(|r| r.as_ref().ok().cloned());
        if let Some(f) = self.on_select.borrow().as_ref() {
            f(e.path.clone(), cached);
        }
    }

    /// Store a probe made elsewhere (e.g. by the details pane) in the cache.
    pub fn remember(&self, info: &MediaInfo) {
        if let Ok(md) = std::fs::metadata(&info.path) {
            self.cache
                .borrow_mut()
                .insert((info.path.clone(), md.len(), md.modified().ok()), Ok(info.clone()));
        }
    }

    pub fn open_dir(self: &Rc<Self>, dir: PathBuf) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        let gen = self.gen.get().wrapping_add(1);
        self.gen.set(gen);
        self.queue.borrow_mut().clear();
        self.path_label.set_text(&dir.display().to_string());
        self.up.set_sensitive(dir.parent().is_some());
        *self.cwd.borrow_mut() = Some(dir.clone());
        // Keep the places dropdown in sync when we land on a bookmark.
        let idx = self.places.borrow().iter().position(|p| p.path == dir);
        if let Some(i) = idx {
            self.suppress_place.set(true);
            self.places_dd.set_selected(i as u32);
            self.suppress_place.set(false);
        }
        self.stack.set_visible_child_name("loading");
        let w = Rc::downgrade(self);
        let d2 = dir.clone();
        ctx.spawn(
            move |_tv| list_dir(&d2),
            move |res| {
                let Some(b) = w.upgrade() else { return };
                if b.gen.get() != gen {
                    return;
                }
                match res {
                    Ok(entries) => b.populate(entries),
                    Err(e) => {
                        b.clear_rows();
                        b.empty.set_title("Cannot open this folder");
                        b.empty.set_description(Some(&e.to_string()));
                        b.stack.set_visible_child_name("empty");
                    }
                }
            },
        );
    }

    fn clear_rows(&self) {
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        self.rows.borrow_mut().clear();
    }

    fn populate(self: &Rc<Self>, entries: Vec<Entry>) {
        self.clear_rows();
        if entries.is_empty() {
            self.empty.set_title("No media here");
            self.empty.set_description(Some(
                "Folders and video, audio and image files appear here. You can also drop a file on this page.",
            ));
            self.stack.set_visible_child_name("empty");
            return;
        }
        self.stack.set_visible_child_name("list");
        let mut to_probe = Vec::new();
        for e in entries {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&e.name).as_str())
                .activatable(true)
                .build();
            row.set_title_lines(2);
            let icon = if e.is_dir {
                "folder-symbolic"
            } else {
                e.kind.map(MediaKind::icon_name).unwrap_or("text-x-generic-symbolic")
            };
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            let badge = gtk::Label::new(None);
            badge.add_css_class("tv-badge");
            if e.is_dir {
                row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            } else {
                row.set_subtitle(&human_size(e.size));
                row.add_suffix(&badge);
                if matches!(e.kind, Some(MediaKind::Video | MediaKind::Audio)) {
                    to_probe.push(e.clone());
                }
            }
            self.list.append(&row);
            self.rows.borrow_mut().insert(e.path.clone(), (row, badge, e));
        }
        // Fill from cache, queue the rest.
        for e in to_probe {
            let key = (e.path.clone(), e.size, e.mtime);
            if self.cache.borrow().contains_key(&key) {
                self.render_row(&e.path);
            } else {
                self.queue.borrow_mut().push_back(e);
            }
        }
        self.pump();
    }

    fn pump(self: &Rc<Self>) {
        let Some(ctx) = self.ctx.upgrade() else { return };
        while self.active.get() < MAX_PROBES {
            let Some(e) = self.queue.borrow_mut().pop_front() else { break };
            if let Some((row, _, _)) = self.rows.borrow().get(&e.path) {
                row.set_subtitle(&format!("{} · analysing…", human_size(e.size)));
            }
            self.active.set(self.active.get() + 1);
            let w = Rc::downgrade(self);
            let path = e.path.clone();
            ctx.spawn(
                move |tv| tv.probe_media(&path).map_err(|err| err.to_string()),
                move |res| {
                    let Some(b) = w.upgrade() else { return };
                    b.active.set(b.active.get().saturating_sub(1));
                    b.cache
                        .borrow_mut()
                        .insert((e.path.clone(), e.size, e.mtime), res);
                    b.render_row(&e.path);
                    b.pump();
                },
            );
        }
    }

    fn render_row(&self, path: &Path) {
        let rows = self.rows.borrow();
        let Some((row, badge, e)) = rows.get(path) else { return };
        let key = (e.path.clone(), e.size, e.mtime);
        let cache = self.cache.borrow();
        let Some(res) = cache.get(&key) else { return };
        let vaapi = self.ctx.upgrade().and_then(|c| c.vaapi.get());
        match res {
            Ok(info) => {
                let summary = media_summary(info);
                row.set_subtitle(&glib::markup_escape_text(&if summary.is_empty() {
                    human_size(e.size)
                } else {
                    format!("{} · {}", human_size(e.size), summary)
                }));
                let report = check_compat(info);
                let level = match vaapi {
                    Some(v) => report.effective_level(v),
                    None => report.level,
                };
                let (b, text) = compat_badge(level);
                badge.set_text(b);
                badge.set_tooltip_text(Some(text));
                for c in ["success", "warning", "error"] {
                    badge.remove_css_class(c);
                }
                badge.add_css_class(compat_css(level));
            }
            Err(err) => {
                row.set_subtitle(&glib::markup_escape_text(&format!(
                    "{} · cannot analyse",
                    human_size(e.size)
                )));
                badge.set_text("?");
                badge.set_tooltip_text(Some(err));
            }
        }
    }

    fn refresh_badges(&self) {
        let paths: Vec<PathBuf> = self.rows.borrow().keys().cloned().collect();
        for p in paths {
            self.render_row(&p);
        }
    }
}
