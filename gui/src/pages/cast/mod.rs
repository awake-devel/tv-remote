//! Cast page: media browser on the left; on the right the selected file, the
//! TV-friendly selectors, the network check, the Cast button, a URL field and the
//! now-playing card.

mod browser;
mod netcard;
mod options;
pub mod playing;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use adw::prelude::*;
use gtk::{gdk, glib};
use tvremote_core::cast::{check_compat, CastMode, CastRequest, CastSource, CompatReport, MediaInfo};
use tvremote_core::netcheck::{CapacityProgress, DemandJob, Fix, FixKind, PacketProfile};
use tvremote_core::{CastEvent, Error, Event, Verdict, VerdictLevel};

use crate::ctx::Ctx;
use crate::format::{
    compat_badge, compat_css, fmt_mbps, human_size, looks_like_url, media_kind, media_summary,
    MediaKind,
};
use browser::Browser;
use netcard::{load_cap_info, NetCard};
use options::Options;
use playing::NowPlaying;

struct Selection {
    info: MediaInfo,
    report: CompatReport,
    packets: Option<Arc<PacketProfile>>,
    job: Option<DemandJob>,
    verdict: Option<Verdict>,
}

enum Target {
    None,
    Probing,
    Image(PathBuf),
    Media(Box<Selection>),
}

enum JobMsg {
    Progress(f32),
    Done(Option<Result<Arc<PacketProfile>, String>>),
}

pub struct CastPage {
    pub widget: gtk::Widget,
    ctx: Weak<Ctx>,
    browser: Rc<Browser>,
    file_row: adw::ActionRow,
    issues: adw::ExpanderRow,
    issue_rows: RefCell<Vec<adw::ActionRow>>,
    options: Rc<Options>,
    net: Rc<NetCard>,
    cast_btn: gtk::Button,
    cast_note: gtk::Label,
    playing: Rc<NowPlaying>,
    target: RefCell<Target>,
    sel_gen: Cell<u64>,
    verdict_gen: Cell<u64>,
    casting: Cell<bool>,
    /// Cancel flag of the running network test.
    net_test_cancel: RefCell<Option<Arc<AtomicBool>>>,
}

impl CastPage {
    pub fn new(ctx: &Rc<Ctx>) -> Rc<CastPage> {
        let browser = Browser::new(ctx);
        let options = Options::new();
        let net = NetCard::new();
        let playing = NowPlaying::new(ctx);

        // --- right pane -----------------------------------------------------------------------
        let details = gtk::Box::new(gtk::Orientation::Vertical, 24);
        details.set_margin_top(18);
        details.set_margin_bottom(18);
        details.set_margin_start(12);
        details.set_margin_end(12);

        let file_group = adw::PreferencesGroup::builder().title("Selected").build();
        let file_row = adw::ActionRow::builder()
            .title("No file selected")
            .subtitle("Pick a file on the left, drop one on this page, or use Open…")
            .build();
        file_row.add_prefix(&gtk::Image::from_icon_name("video-x-generic-symbolic"));
        file_row.set_subtitle_selectable(true);
        let issues = adw::ExpanderRow::builder().title("Compatibility").build();
        issues.set_visible(false);
        file_group.add(&file_row);
        file_group.add(&issues);
        details.append(&file_group);

        options.group.set_visible(false);
        details.append(&options.group);
        details.append(&net.group);

        let cast_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let cast_btn = gtk::Button::with_label("Cast to the TV");
        cast_btn.add_css_class("pill");
        cast_btn.add_css_class("suggested-action");
        cast_btn.set_halign(gtk::Align::Center);
        cast_btn.set_sensitive(false);
        let cast_note = gtk::Label::new(Some("Available once the network check has a verdict."));
        cast_note.add_css_class("caption");
        cast_note.add_css_class("dim-label");
        cast_note.set_wrap(true);
        cast_note.set_justify(gtk::Justification::Center);
        cast_box.append(&cast_btn);
        cast_box.append(&cast_note);
        details.append(&cast_box);

        let url_group = adw::PreferencesGroup::builder()
            .title("Cast a link")
            .description("An http(s) media URL the TV fetches itself (always direct).")
            .build();
        let url = adw::EntryRow::builder()
            .title("Media URL")
            .show_apply_button(true)
            .build();
        url.add_prefix(&gtk::Image::from_icon_name("web-browser-symbolic"));
        url_group.add(&url);
        details.append(&url_group);

        details.append(&playing.group);

        let clamp = adw::Clamp::builder().maximum_size(720).child(&details).build();
        let right = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .hexpand(true)
            .child(&clamp)
            .build();

        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_wide_handle(true);
        browser.widget.set_size_request(320, -1);
        paned.set_start_child(Some(&browser.widget));
        paned.set_end_child(Some(&right));
        paned.set_resize_start_child(false);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        paned.set_position(400);

        let this = Rc::new(CastPage {
            widget: paned.clone().upcast(),
            ctx: ctx.weak(),
            browser: browser.clone(),
            file_row,
            issues,
            issue_rows: RefCell::new(Vec::new()),
            options: options.clone(),
            net: net.clone(),
            cast_btn: cast_btn.clone(),
            cast_note,
            playing,
            target: RefCell::new(Target::None),
            sel_gen: Cell::new(0),
            verdict_gen: Cell::new(0),
            casting: Cell::new(false),
            net_test_cancel: RefCell::new(None),
        });

        // Browser selection.
        let w = Rc::downgrade(&this);
        browser.connect_select(move |path, cached| {
            if let Some(p) = w.upgrade() {
                p.select(path, cached);
            }
        });
        // Selector changes → estimate + verdict.
        let w = Rc::downgrade(&this);
        options.connect_changed(move || {
            if let Some(p) = w.upgrade() {
                p.recompute();
            }
        });
        // VAAPI known → quality row, badges, fixes.
        let w = Rc::downgrade(&this);
        ctx.on_vaapi(move |v| {
            if let Some(p) = w.upgrade() {
                p.options.set_vaapi(v);
                p.apply_vaapi_recommendation(v);
                p.refresh_compat();
                p.recompute();
            }
        });
        // Cast button.
        let w = Rc::downgrade(&this);
        cast_btn.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.on_cast_clicked();
            }
        });
        // URL.
        let w = Rc::downgrade(&this);
        url.connect_apply(move |row| {
            let Some(p) = w.upgrade() else { return };
            let text = row.text().trim().to_string();
            if !looks_like_url(&text) {
                if let Some(c) = p.ctx.upgrade() {
                    c.toast("Enter an http:// or https:// link");
                }
                return;
            }
            p.start_cast(CastRequest::url(text));
        });
        // Analysis cancel.
        let w = Rc::downgrade(&this);
        net.analysis_cancel.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                if let Target::Media(sel) = &*p.target.borrow() {
                    if let Some(j) = &sel.job {
                        j.cancel();
                    }
                }
            }
        });
        // Network test.
        let w = Rc::downgrade(&this);
        net.test_cancel.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                if let Some(flag) = &*p.net_test_cancel.borrow() {
                    flag.store(true, Ordering::SeqCst);
                    p.net.set_test_cancelling();
                }
            }
        });
        let w = Rc::downgrade(&this);
        net.test_btn.connect_clicked(move |_| {
            let Some(p) = w.upgrade() else { return };
            let Some(c) = p.ctx.upgrade() else { return };
            let w2 = Rc::downgrade(&p);
            c.confirm(
                "Test the network?",
                "This casts a noise test pattern to the TV for about 10 seconds and stops whatever is being cast. \
                 The first run also encodes a probe clip on this PC, which takes about a minute.",
                "Run the test",
                false,
                move || {
                    if let Some(p) = w2.upgrade() {
                        p.run_network_test();
                    }
                },
            );
        });

        // Drag and drop: files select, text URLs go to the URL field.
        let drop = gtk::DropTarget::new(glib::Type::INVALID, gdk::DragAction::COPY);
        drop.set_types(&[gdk::FileList::static_type(), glib::Type::STRING]);
        let w = Rc::downgrade(&this);
        let url2 = url.clone();
        drop.connect_drop(move |_, value, _, _| {
            let Some(p) = w.upgrade() else { return false };
            if let Ok(list) = value.get::<gdk::FileList>() {
                if let Some(path) = list.files().first().and_then(|f| f.path()) {
                    p.browser.select_path(path);
                    return true;
                }
                return false;
            }
            if let Ok(s) = value.get::<String>() {
                let s = s.trim().to_string();
                if looks_like_url(&s) {
                    url2.set_text(&s);
                    return true;
                }
                if let Some(path) = s.strip_prefix("file://").map(PathBuf::from) {
                    p.browser.select_path(path);
                    return true;
                }
            }
            false
        });
        paned.add_controller(drop);

        // Initial capacity / link info, and any running cast (e.g. after a restart).
        this.refresh_cap_info();
        let w = Rc::downgrade(&this);
        ctx.spawn(
            |tv| tv.current_cast(),
            move |s| {
                if let (Some(p), Some(s)) = (w.upgrade(), s) {
                    p.playing.attach(s);
                }
            },
        );
        this
    }

    fn refresh_cap_info(self: &Rc<Self>) {
        let Some(c) = self.ctx.upgrade() else { return };
        let net = self.net.clone();
        c.spawn(|tv| load_cap_info(tv.tv_network()), move |ci| net.set_cap_info(&ci));
    }

    fn cancel_job(&self) {
        if let Target::Media(sel) = &*self.target.borrow() {
            if let Some(j) = &sel.job {
                j.cancel();
            }
        }
        self.net.set_analysis(None);
    }

    fn set_cast_enabled(&self, on: bool, note: &str) {
        self.cast_btn.set_sensitive(on && !self.casting.get());
        self.cast_note.set_text(note);
    }

    fn clear_issues(&self) {
        for r in self.issue_rows.borrow_mut().drain(..) {
            self.issues.remove(&r);
        }
    }

    fn select(self: &Rc<Self>, path: PathBuf, cached: Option<MediaInfo>) {
        let Some(c) = self.ctx.upgrade() else { return };
        self.cancel_job();
        let gen = self.sel_gen.get().wrapping_add(1);
        self.sel_gen.set(gen);
        self.verdict_gen.set(self.verdict_gen.get().wrapping_add(1));

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        self.file_row.set_title(&glib::markup_escape_text(&name));
        self.file_row.set_subtitle(&glib::markup_escape_text(&path.display().to_string()));
        self.clear_issues();
        self.issues.set_visible(false);
        self.options.group.set_visible(false);

        if media_kind(&path) == Some(MediaKind::Image) {
            *self.target.borrow_mut() = Target::Image(path);
            self.net.clear("Images are small: no streaming check needed.");
            self.set_cast_enabled(true, "Images are cast directly.");
            return;
        }
        if let Some(info) = cached {
            self.set_media(info);
            return;
        }
        *self.target.borrow_mut() = Target::Probing;
        self.net.clear("Analysing the file…");
        self.set_cast_enabled(false, "Analysing the file…");
        let w = Rc::downgrade(self);
        c.spawn(
            move |tv| tv.probe_media(&path),
            move |res| {
                let Some(p) = w.upgrade() else { return };
                if p.sel_gen.get() != gen {
                    return;
                }
                match res {
                    Ok(info) => {
                        p.browser.remember(&info);
                        p.set_media(info);
                    }
                    Err(e) => {
                        *p.target.borrow_mut() = Target::None;
                        p.net.clear(&format!("Cannot analyse this file: {e}"));
                        p.set_cast_enabled(false, "This file cannot be analysed.");
                        if let Some(c) = p.ctx.upgrade() {
                            c.toast(&format!("ffprobe: {e}"));
                        }
                    }
                }
            },
        );
    }

    fn set_media(self: &Rc<Self>, info: MediaInfo) {
        let Some(c) = self.ctx.upgrade() else { return };
        let gen = self.sel_gen.get();
        let report = check_compat(&info);
        let mut sub = info.size.map(human_size).unwrap_or_default();
        let summary = media_summary(&info);
        if !summary.is_empty() {
            if !sub.is_empty() {
                sub.push_str(" · ");
            }
            sub.push_str(&summary);
        }
        self.file_row.set_subtitle(&glib::markup_escape_text(&sub));

        let tv_friendly = report.needs_tv_friendly();
        // Until the VAAPI probe answers, recommend copying; `on_vaapi` upgrades to a
        // reduced profile if the file needs a re-encode and VAAPI turns out to work.
        let vaapi = c.vaapi.get() == Some(true);
        let profile = tvremote_core::CastProfile::recommended(&info, vaapi);
        self.options.set_media(&info, tv_friendly, &profile);
        self.options.group.set_visible(true);

        // Background packet analysis (cached by path + size + mtime in core).
        let (tx, rx) = async_channel::unbounded::<JobMsg>();
        let tx2 = tx.clone();
        // `analyze_demand` only spawns its own thread and returns at once.
        let job = c.tv().analyze_demand(
            &info,
            move |p| {
                let _ = tx.send_blocking(JobMsg::Progress(p));
            },
            move |res| {
                let msg = match res {
                    Ok(p) => Some(Ok(p)),
                    Err(Error::Cancelled) => None,
                    Err(e) => Some(Err(e.to_string())),
                };
                let _ = tx2.send_blocking(JobMsg::Done(msg));
            },
        );
        self.net.set_analysis(Some(0.0));
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(m) = rx.recv().await {
                let Some(p) = w.upgrade() else { return };
                if p.sel_gen.get() != gen {
                    return;
                }
                match m {
                    JobMsg::Progress(f) => p.net.set_analysis(Some(f)),
                    JobMsg::Done(res) => {
                        p.net.set_analysis(None);
                        match res {
                            Some(Ok(prof)) => {
                                if let Target::Media(sel) = &mut *p.target.borrow_mut() {
                                    sel.packets = Some(prof);
                                }
                                p.recompute();
                            }
                            Some(Err(e)) => {
                                if let Some(c) = p.ctx.upgrade() {
                                    c.toast(&format!("Peak analysis failed: {e}"));
                                }
                            }
                            None => {
                                if let Some(c) = p.ctx.upgrade() {
                                    c.toast("Peak analysis cancelled: using the provisional estimate");
                                }
                            }
                        }
                        return;
                    }
                }
            }
        });

        *self.target.borrow_mut() = Target::Media(Box::new(Selection {
            info,
            report,
            packets: None,
            job: Some(job),
            verdict: None,
        }));
        self.refresh_compat();
        self.recompute();
    }

    /// The VAAPI probe answered after a file was selected: if the file needs a
    /// re-encode and the user still has the original quality, select the recommended
    /// reduced quality.
    fn apply_vaapi_recommendation(&self, vaapi: bool) {
        if !vaapi {
            return;
        }
        let rec = {
            let target = self.target.borrow();
            let Target::Media(sel) = &*target else { return };
            tvremote_core::CastProfile::recommended(&sel.info, true)
        };
        let (tvf, cur) = self.options.read();
        if matches!(rec.video, tvremote_core::cast::VideoMode::Reduce { .. })
            && cur.video == tvremote_core::cast::VideoMode::Copy
        {
            self.options.select_profile(tvf, &tvremote_core::CastProfile { video: rec.video, ..cur });
        }
    }

    /// Compatibility expander (depends on VAAPI availability).
    fn refresh_compat(&self) {
        let vaapi = self.ctx.upgrade().and_then(|c| c.vaapi.get());
        let target = self.target.borrow();
        let Target::Media(sel) = &*target else { return };
        self.clear_issues();
        let level = match vaapi {
            Some(v) => sel.report.effective_level(v),
            None => sel.report.level,
        };
        let (badge, text) = compat_badge(level);
        self.issues.set_title(&format!("Compatibility: {badge} {text}"));
        for c in ["success", "warning", "error"] {
            self.issues.remove_css_class(c);
        }
        self.issues.add_css_class(compat_css(level));
        let mut notes: Vec<String> = sel.report.issues.iter().map(|i| {
            let fix = if i.fixable { " (TV-friendly fixes this)" } else { "" };
            format!("{}{fix}", i.message)
        }).collect();
        if sel.report.needs_video_reencode && vaapi == Some(false) {
            notes.push("The video must be re-encoded, but VAAPI hardware encoding is not available on this PC.".into());
        }
        self.issues.set_subtitle(&if notes.is_empty() {
            "No problems found".to_string()
        } else {
            format!("{} note(s)", notes.len())
        });
        self.issues.set_enable_expansion(!notes.is_empty());
        for n in notes {
            let r = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&n).as_str())
                .build();
            r.set_title_lines(0);
            self.issues.add_row(&r);
            self.issue_rows.borrow_mut().push(r);
        }
        self.issues.set_visible(true);
    }

    fn current_mode(&self) -> CastMode {
        let (tvf, profile) = self.options.read();
        if tvf {
            CastMode::TvFriendly(profile)
        } else {
            CastMode::Direct
        }
    }

    /// Recompute the estimate (instantly) and the verdict (on a worker).
    fn recompute(self: &Rc<Self>) {
        let Some(c) = self.ctx.upgrade() else { return };
        let (info, packets) = {
            let mut t = self.target.borrow_mut();
            let Target::Media(sel) = &mut *t else { return };
            sel.verdict = None;
            (sel.info.clone(), sel.packets.clone())
        };
        self.options.update_estimate(&info);
        let mode = self.current_mode();
        let gen = self.verdict_gen.get().wrapping_add(1);
        self.verdict_gen.set(gen);
        self.set_cast_enabled(false, "Checking the network…");
        let w = Rc::downgrade(self);
        c.spawn(
            move |tv| {
                let tv_net = tv.tv_network();
                let (d, v) = tv.network_verdict(&info, &mode, packets.as_deref(), tv_net);
                (d, v, load_cap_info(tv_net))
            },
            move |(demand, verdict, ci)| {
                let Some(p) = w.upgrade() else { return };
                if p.verdict_gen.get() != gen {
                    return;
                }
                p.net.set_demand(&demand);
                p.net.set_cap_info(&ci);
                let vaapi = p.ctx.upgrade().and_then(|c| c.vaapi.get());
                let w2 = Rc::downgrade(&p);
                let apply: Rc<dyn Fn(Fix)> = Rc::new(move |fix: Fix| {
                    if let Some(p) = w2.upgrade() {
                        p.apply_fix(fix);
                    }
                });
                p.net.set_verdict(&verdict, vaapi, apply);
                let note = match verdict.level {
                    VerdictLevel::Ok => format!("Verdict: ✅ peak {}", fmt_mbps(verdict.demand_mbps)),
                    VerdictLevel::Risky => format!("Verdict: ⚠ peak {} — may stutter", fmt_mbps(verdict.demand_mbps)),
                    VerdictLevel::Bad => format!(
                        "Verdict: ❌ peak {} — you will be asked to confirm",
                        fmt_mbps(verdict.demand_mbps)
                    ),
                };
                if let Target::Media(sel) = &mut *p.target.borrow_mut() {
                    sel.verdict = Some(verdict);
                }
                p.set_cast_enabled(true, &note);
            },
        );
    }

    fn apply_fix(self: &Rc<Self>, fix: Fix) {
        let profile = match &fix.kind {
            FixKind::DropExtraTracks { profile } => profile.clone(),
            FixKind::ReduceVideo { profile, .. } => profile.clone(),
            FixKind::CopyToTv => return,
        };
        self.options.select_profile(true, &profile);
        if let Some(c) = self.ctx.upgrade() {
            c.toast(&format!("Applied: {}", fix.description));
        }
        self.recompute();
    }

    fn on_cast_clicked(self: &Rc<Self>) {
        let Some(c) = self.ctx.upgrade() else { return };
        let req_and_level = match &*self.target.borrow() {
            Target::Image(p) => Some((CastRequest::file(p.clone()), None, false)),
            Target::Media(sel) => {
                let Some(v) = &sel.verdict else {
                    c.toast("Wait for the network verdict");
                    return;
                };
                let mode = self.current_mode();
                // Only when the direct cast would not play (not for an untested codec or an
                // extra DTS track next to a playable default one).
                let direct_problem = matches!(mode, CastMode::Direct) && sel.report.needs_tv_friendly();
                Some((
                    CastRequest {
                        source: CastSource::File(sel.info.path.clone()),
                        mode,
                        title: None,
                        media: Some(sel.info.clone()),
                    },
                    Some(v.level),
                    direct_problem,
                ))
            }
            Target::None | Target::Probing => None,
        };
        let Some((req, level, direct_problem)) = req_and_level else { return };

        let w = Rc::downgrade(self);
        let go = move |req: CastRequest| {
            if let Some(p) = w.upgrade() {
                p.start_cast(req);
            }
        };
        if level == Some(VerdictLevel::Bad) {
            let req2 = req.clone();
            c.confirm(
                "The network will probably not keep up",
                "The verdict is ❌: playback will most likely stutter. Try one of the suggested fixes, or cast anyway.",
                "Cast anyway",
                true,
                move || go(req2.clone()),
            );
        } else if direct_problem {
            let req2 = req.clone();
            c.confirm(
                "This file needs TV-friendly mode",
                "The compatibility check found problems the TV can't handle directly (for example DTS audio). Switch the mode to TV-friendly, or cast directly anyway.",
                "Cast directly anyway",
                true,
                move || go(req2.clone()),
            );
        } else {
            go(req);
        }
    }

    fn start_cast(self: &Rc<Self>, req: CastRequest) {
        let Some(c) = self.ctx.upgrade() else { return };
        if self.casting.replace(true) {
            return;
        }
        let was_enabled = self.cast_btn.is_sensitive();
        self.cast_btn.set_sensitive(false);
        self.cast_btn.set_label("Starting…");
        c.toast("Starting the cast…");
        let w = Rc::downgrade(self);
        c.spawn(
            move |tv| tv.cast(req),
            move |res| {
                let Some(p) = w.upgrade() else { return };
                p.casting.set(false);
                p.cast_btn.set_label("Cast to the TV");
                let has_verdict = match &*p.target.borrow() {
                    Target::Media(sel) => sel.verdict.is_some(),
                    Target::Image(_) => true,
                    _ => false,
                };
                p.cast_btn.set_sensitive(was_enabled || has_verdict);
                match res {
                    Ok(session) => p.playing.attach(session),
                    Err(e) => {
                        if let Some(c) = p.ctx.upgrade() {
                            c.toast(&format!("Cast failed: {e}"));
                        }
                    }
                }
            },
        );
    }

    fn run_network_test(self: &Rc<Self>) {
        let Some(c) = self.ctx.upgrade() else { return };
        self.net.set_test_progress(Some(&CapacityProgress::Starting));
        let cancel = Arc::new(AtomicBool::new(false));
        *self.net_test_cancel.borrow_mut() = Some(cancel.clone());
        let net = self.net.clone();
        let w = Rc::downgrade(self);
        c.spawn_with_progress(
            move |tv, progress: &dyn Fn(CapacityProgress)| tv.measure_network(&cancel, progress),
            move |p: CapacityProgress| {
                // Keep the "Cancelling…" text once the user cancelled.
                if !net.test_cancel.is_sensitive() {
                    return;
                }
                net.set_test_progress(Some(&p))
            },
            move |res| {
                let Some(p) = w.upgrade() else { return };
                p.net_test_cancel.borrow_mut().take();
                p.net.set_test_progress(None);
                if let Some(c) = p.ctx.upgrade() {
                    match res {
                        Ok(m) => c.toast_for(
                            &format!("Network capacity: {} (best second {})", fmt_mbps(m.mbps), fmt_mbps(m.peak_mbps)),
                            6,
                        ),
                        Err(Error::Cancelled) => c.toast("Network test cancelled: nothing was saved"),
                        Err(e) => c.toast(&format!("Network test failed: {e}")),
                    }
                }
                p.refresh_cap_info();
                p.recompute();
            },
        );
    }

    /// Another TV is controlled now: new casts go to it; a running cast stays attached
    /// (its controls act on the TV it plays on).
    pub fn on_tv_changed(self: &Rc<Self>) {
        self.refresh_cap_info();
    }

    /// `tv_id`: the TV the event came from (cast events are handled for every TV, so
    /// the now-playing card follows the session wherever it plays).
    pub fn handle_event(self: &Rc<Self>, tv_id: &str, ev: &Event) {
        let Event::Cast(ce) = ev else { return };
        self.playing.handle(ce);
        let Some(c) = self.ctx.upgrade() else { return };
        match ce {
            CastEvent::Stall { message, suggestion } => {
                c.toast_for(&format!("{message}. {suggestion}"), 10);
            }
            CastEvent::Error(m) => c.toast(&format!("Cast: {m}")),
            CastEvent::NeverStarted => c.toast_for(
                "The TV never started playing: the format is probably unsupported. Try TV-friendly mode.",
                8,
            ),
            CastEvent::Loading { .. } if !self.casting.get() => {
                // Started elsewhere (e.g. the TUI in this process is unlikely, but a
                // network test or another page): attach to the current session.
                let w = Rc::downgrade(self);
                let tv = c.fleet.tv(tv_id).unwrap_or_else(|_| c.tv());
                c.spawn_on(
                    tv,
                    |tv| tv.current_cast(),
                    move |s| {
                        if let (Some(p), Some(s)) = (w.upgrade(), s) {
                            p.playing.attach(s);
                        }
                    },
                );
            }
            _ => {}
        }
    }
}

impl CastPage {
    /// True while a cast session started or found by this page is active.
    pub fn is_casting(&self) -> bool {
        self.playing.is_active()
    }
}
