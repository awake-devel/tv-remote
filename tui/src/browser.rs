//! File-browser state for the cast flow: bookmarks, folder navigation, media filtering.
//! Everything here is local filesystem work (no TV traffic); ffprobe runs elsewhere.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

/// Media families we list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Video,
    Audio,
    Image,
}

const VIDEO_EXT: &[&str] = &[
    "mkv", "mp4", "m4v", "mov", "avi", "webm", "ts", "m2ts", "mts", "mpg", "mpeg", "wmv", "flv",
    "3gp", "ogv",
];
const AUDIO_EXT: &[&str] = &[
    "mp3", "flac", "m4a", "aac", "ogg", "opus", "wav", "wma", "alac", "ape", "mka",
];
const IMAGE_EXT: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp", "heic"];

/// Media kind from the file extension (case-insensitive).
pub fn media_kind(path: &Path) -> Option<MediaKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if VIDEO_EXT.contains(&ext.as_str()) {
        Some(MediaKind::Video)
    } else if AUDIO_EXT.contains(&ext.as_str()) {
        Some(MediaKind::Audio)
    } else if IMAGE_EXT.contains(&ext.as_str()) {
        Some(MediaKind::Image)
    } else {
        None
    }
}

/// One row of the file list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    /// `..`
    Parent,
    Dir,
    File {
        size: u64,
        kind: MediaKind,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub kind: EntryKind,
}

impl Entry {
    pub fn is_file(&self) -> bool {
        matches!(self.kind, EntryKind::File { .. })
    }
}

/// Raw directory item before filtering (lets the filter/sort logic be unit-tested).
#[derive(Debug, Clone)]
pub struct RawItem {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Natural, case-insensitive ordering: `ep2` < `ep10`.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(c);
                    bi.next();
                }
                let ta = na.trim_start_matches('0');
                let tb = nb.trim_start_matches('0');
                let o = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                ai.next();
                bi.next();
            }
        }
    }
}

/// Keep directories and media files (hidden ones only if asked), sorted: directories
/// first, then files, each in natural order. A `..` entry leads when `dir` has a parent.
pub fn build_entries(dir: &Path, items: Vec<RawItem>, show_hidden: bool) -> Vec<Entry> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for it in items {
        if !show_hidden && it.name.starts_with('.') {
            continue;
        }
        let path = dir.join(&it.name);
        if it.is_dir {
            dirs.push(Entry {
                name: it.name,
                path,
                kind: EntryKind::Dir,
            });
        } else if let Some(kind) = media_kind(&path) {
            files.push(Entry {
                name: it.name,
                path,
                kind: EntryKind::File {
                    size: it.size,
                    kind,
                },
            });
        }
    }
    dirs.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    files.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    let mut out = Vec::with_capacity(dirs.len() + files.len() + 1);
    if let Some(p) = dir.parent() {
        out.push(Entry {
            name: "..".into(),
            path: p.to_path_buf(),
            kind: EntryKind::Parent,
        });
    }
    out.extend(dirs);
    out.extend(files);
    out
}

/// Read a directory (follows symlinks for the dir/file decision).
pub fn read_dir(dir: &Path) -> std::io::Result<Vec<RawItem>> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let Ok(e) = e else { continue };
        let name = e.file_name().to_string_lossy().to_string();
        let md = std::fs::metadata(e.path()).or_else(|_| e.metadata());
        let Ok(md) = md else { continue };
        v.push(RawItem {
            name,
            is_dir: md.is_dir(),
            size: md.len(),
        });
    }
    Ok(v)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub label: String,
    pub path: PathBuf,
}

fn subdirs(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Bookmarks: the user's video and download folders (French names first, English
/// fallbacks), home, then mounted drives under `mount_roots`. Only existing folders.
pub fn bookmarks_from(home: &Path, mount_roots: &[PathBuf]) -> Vec<Bookmark> {
    let mut out = Vec::new();
    let mut push = |label: String, path: PathBuf| {
        if path.is_dir() && !out.iter().any(|b: &Bookmark| b.path == path) {
            out.push(Bookmark { label, path });
        }
    };
    for (fr, en) in [("Vidéos", "Videos"), ("Téléchargements", "Downloads")] {
        let p = home.join(fr);
        if p.is_dir() {
            push(fr.to_string(), p);
        } else {
            push(en.to_string(), home.join(en));
        }
    }
    push("Home".into(), home.to_path_buf());
    for root in mount_roots {
        for d in subdirs(root) {
            let name = d
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            push(format!("Drive: {name}"), d);
        }
    }
    out
}

/// Mount roots: `/run/media/$USER`, `/media/$USER` and `/media`.
pub fn default_mount_roots() -> Vec<PathBuf> {
    let user = std::env::var("USER").unwrap_or_default();
    let mut v = Vec::new();
    if !user.is_empty() {
        v.push(PathBuf::from("/run/media").join(&user));
        v.push(PathBuf::from("/media").join(&user));
    }
    v.push(PathBuf::from("/media"));
    v
}

/// Which pane has the focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Bookmarks,
    Files,
}

/// Browser state.
#[derive(Debug)]
pub struct Browser {
    pub cwd: PathBuf,
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub bookmarks: Vec<Bookmark>,
    pub bm_selected: usize,
    pub focus: Pane,
    pub show_hidden: bool,
    pub error: Option<String>,
}

impl Browser {
    pub fn new(home: &Path) -> Browser {
        let bookmarks = bookmarks_from(home, &default_mount_roots());
        let start = bookmarks
            .first()
            .map(|b| b.path.clone())
            .unwrap_or_else(|| home.to_path_buf());
        let mut b = Browser {
            cwd: start.clone(),
            entries: Vec::new(),
            selected: 0,
            bookmarks,
            bm_selected: 0,
            focus: Pane::Files,
            show_hidden: false,
            error: None,
        };
        b.open(&start);
        b
    }

    /// Re-scan bookmarks (drives may have been mounted).
    pub fn refresh_bookmarks(&mut self, home: &Path) {
        self.bookmarks = bookmarks_from(home, &default_mount_roots());
        self.bm_selected = self.bm_selected.min(self.bookmarks.len().saturating_sub(1));
    }

    /// Open a directory; on error the current listing stays and `error` is set.
    pub fn open(&mut self, dir: &Path) {
        match read_dir(dir) {
            Ok(items) => {
                let prev = self.cwd.clone();
                self.cwd = dir.to_path_buf();
                self.entries = build_entries(dir, items, self.show_hidden);
                // When going up, re-select the folder we came from.
                self.selected = self
                    .entries
                    .iter()
                    .position(|e| e.kind == EntryKind::Dir && e.path == prev)
                    .unwrap_or(0);
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{}: {e}", dir.display())),
        }
    }

    pub fn reload(&mut self) {
        let sel = self.selected;
        let cwd = self.cwd.clone();
        self.open(&cwd);
        self.selected = sel.min(self.entries.len().saturating_sub(1));
    }

    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.reload();
    }

    pub fn move_by(&mut self, delta: isize) {
        let (len, sel) = match self.focus {
            Pane::Files => (self.entries.len(), &mut self.selected),
            Pane::Bookmarks => (self.bookmarks.len(), &mut self.bm_selected),
        };
        if len == 0 {
            return;
        }
        let n = (*sel as isize + delta).clamp(0, len as isize - 1);
        *sel = n as usize;
    }

    pub fn home_end(&mut self, end: bool) {
        let len = match self.focus {
            Pane::Files => self.entries.len(),
            Pane::Bookmarks => self.bookmarks.len(),
        };
        let v = if end { len.saturating_sub(1) } else { 0 };
        match self.focus {
            Pane::Files => self.selected = v,
            Pane::Bookmarks => self.bm_selected = v,
        }
    }

    pub fn parent(&mut self) {
        if let Some(p) = self.cwd.parent().map(Path::to_path_buf) {
            self.open(&p);
        }
    }

    pub fn current(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    /// Activate the selection: opens folders/bookmarks, returns a chosen file.
    pub fn activate(&mut self) -> Option<PathBuf> {
        match self.focus {
            Pane::Bookmarks => {
                if let Some(b) = self.bookmarks.get(self.bm_selected).cloned() {
                    self.open(&b.path);
                    self.focus = Pane::Files;
                }
                None
            }
            Pane::Files => {
                let e = self.current()?.clone();
                match e.kind {
                    EntryKind::Parent | EntryKind::Dir => {
                        self.open(&e.path);
                        None
                    }
                    EntryKind::File { .. } => Some(e.path),
                }
            }
        }
    }

    /// Media files in the current listing (for background probing).
    pub fn media_files(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .filter(|e| e.is_file())
            .map(|e| e.path.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str, is_dir: bool) -> RawItem {
        RawItem {
            name: name.into(),
            is_dir,
            size: 10,
        }
    }

    #[test]
    fn natural_order() {
        let mut v = vec!["Ep10.mkv", "ep2.mkv", "Ep1.mkv", "alpha", "Beta"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["alpha", "Beta", "Ep1.mkv", "ep2.mkv", "Ep10.mkv"]);
        assert_eq!(natural_cmp("a007", "a7"), Ordering::Equal);
    }

    #[test]
    fn kinds() {
        assert_eq!(media_kind(Path::new("/x/Film.MKV")), Some(MediaKind::Video));
        assert_eq!(media_kind(Path::new("a.flac")), Some(MediaKind::Audio));
        assert_eq!(media_kind(Path::new("a.JPG")), Some(MediaKind::Image));
        assert_eq!(media_kind(Path::new("a.srt")), None);
        assert_eq!(media_kind(Path::new("noext")), None);
    }

    #[test]
    fn filter_and_sort() {
        let items = vec![
            raw("notes.txt", false),
            raw("b.mkv", false),
            raw(".hidden.mkv", false),
            raw("Series", true),
            raw(".cache", true),
            raw("a10.mp4", false),
            raw("a9.mp4", false),
            raw("Anime", true),
        ];
        let e = build_entries(Path::new("/m/Vidéos"), items.clone(), false);
        let names: Vec<&str> = e.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(
            names,
            ["..", "Anime", "Series", "a9.mp4", "a10.mp4", "b.mkv"]
        );
        assert_eq!(e[0].kind, EntryKind::Parent);
        assert_eq!(e[0].path, PathBuf::from("/m"));
        assert!(e[3].is_file());

        let e = build_entries(Path::new("/"), items, true);
        let names: Vec<&str> = e.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(
            names,
            [
                ".cache",
                "Anime",
                "Series",
                ".hidden.mkv",
                "a9.mp4",
                "a10.mp4",
                "b.mkv"
            ]
        );
    }

    #[test]
    fn bookmarks_and_navigation() {
        let root = std::env::temp_dir().join(format!("tvremote-tui-test-{}", std::process::id()));
        let home = root.join("home");
        let mnt = root.join("mnt");
        std::fs::create_dir_all(home.join("Vidéos/Sub")).unwrap();
        std::fs::create_dir_all(home.join("Downloads")).unwrap();
        std::fs::create_dir_all(mnt.join("USB")).unwrap();
        std::fs::write(home.join("Vidéos/film.mkv"), b"x").unwrap();
        std::fs::write(home.join("Vidéos/readme.txt"), b"x").unwrap();

        let bm = bookmarks_from(&home, &[mnt.clone(), root.join("missing")]);
        let labels: Vec<&str> = bm.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, ["Vidéos", "Downloads", "Home", "Drive: USB"]);

        let mut b = Browser {
            cwd: home.clone(),
            entries: vec![],
            selected: 0,
            bookmarks: bm,
            bm_selected: 0,
            focus: Pane::Bookmarks,
            show_hidden: false,
            error: None,
        };
        assert_eq!(b.activate(), None); // opens Vidéos
        assert_eq!(b.cwd, home.join("Vidéos"));
        assert_eq!(b.focus, Pane::Files);
        let names: Vec<&str> = b.entries.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["..", "Sub", "film.mkv"]);
        b.move_by(5);
        assert_eq!(b.activate(), Some(home.join("Vidéos/film.mkv")));
        b.move_by(-1);
        b.activate(); // into Sub
        assert_eq!(b.cwd, home.join("Vidéos/Sub"));
        b.parent();
        assert_eq!(b.cwd, home.join("Vidéos"));
        assert_eq!(b.current().map(|e| e.name.as_str()), Some("Sub"));
        assert_eq!(b.media_files(), vec![home.join("Vidéos/film.mkv")]);
        b.open(&root.join("does-not-exist"));
        assert!(b.error.is_some());
        assert_eq!(b.cwd, home.join("Vidéos"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
