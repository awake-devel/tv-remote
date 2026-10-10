//! Network check card: demand (provisional, then from the packet analysis), capacity
//! measurement + "Test network", link hints, and the verdict with clickable fixes.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use tvremote_core::netcheck::{
    self, CapacityMeasurement, CapacityProgress, Fix, FixKind, LinkInfo,
};
use tvremote_core::{Demand, TvNetwork, Verdict};

use crate::format::{fmt_age, fmt_bps, fmt_mbps, verdict_css, verdict_title};

/// Capacity measurement + link, read on a worker thread.
#[derive(Debug, Clone)]
pub struct CapInfo {
    pub cap: Option<CapacityMeasurement>,
    pub age_secs: u64,
    pub stale: bool,
    pub link: Option<LinkInfo>,
    /// How the TV is connected (from the cached device info), `None` if unknown.
    pub tv: Option<TvNetwork>,
}

/// Blocking (reads the cache and runs `iw`): call from a worker.
pub fn load_cap_info(tv: Option<TvNetwork>) -> CapInfo {
    let link = netcheck::detect_link();
    let cap = netcheck::load_capacity();
    let now = netcheck::now_unix();
    let (age_secs, stale) = match &cap {
        Some(c) => (now.saturating_sub(c.measured_at), c.is_stale(now, link.as_ref())),
        None => (0, false),
    };
    CapInfo { cap, age_secs, stale, link, tv }
}

pub struct NetCard {
    pub group: adw::PreferencesGroup,
    demand_row: adw::ActionRow,
    analysis_row: adw::ActionRow,
    analysis_bar: gtk::ProgressBar,
    pub analysis_cancel: gtk::Button,
    capacity_row: adw::ActionRow,
    pub test_btn: gtk::Button,
    test_row: adw::ActionRow,
    pub test_cancel: gtk::Button,
    test_bar: gtk::ProgressBar,
    link_row: adw::ActionRow,
    verdict_row: adw::ActionRow,
    fix_rows: RefCell<Vec<adw::ActionRow>>,
}

impl NetCard {
    pub fn new() -> Rc<NetCard> {
        let group = adw::PreferencesGroup::builder()
            .title("Network check")
            .description("Will the Wi-Fi carry this stream smoothly?")
            .build();

        let demand_row = adw::ActionRow::builder().title("Stream demand").subtitle("—").build();
        demand_row.add_css_class("property");

        let analysis_row = adw::ActionRow::builder()
            .title("Measuring peaks in the file")
            .subtitle("Reading packet sizes with ffprobe (large remuxes take minutes)")
            .build();
        let analysis_bar = gtk::ProgressBar::new();
        analysis_bar.set_valign(gtk::Align::Center);
        analysis_bar.set_size_request(120, -1);
        analysis_bar.set_show_text(true);
        let analysis_cancel = gtk::Button::from_icon_name("process-stop-symbolic");
        analysis_cancel.set_tooltip_text(Some("Cancel the analysis (the provisional estimate stays)"));
        analysis_cancel.set_valign(gtk::Align::Center);
        analysis_cancel.add_css_class("flat");
        analysis_row.add_suffix(&analysis_bar);
        analysis_row.add_suffix(&analysis_cancel);
        analysis_row.set_visible(false);

        let capacity_row = adw::ActionRow::builder().title("Network capacity").subtitle("Not measured").build();
        capacity_row.add_css_class("property");
        let test_btn = gtk::Button::with_label("Test network");
        test_btn.set_valign(gtk::Align::Center);
        capacity_row.add_suffix(&test_btn);

        let test_row = adw::ActionRow::builder().title("Network test").build();
        let test_bar = gtk::ProgressBar::new();
        test_bar.set_valign(gtk::Align::Center);
        test_bar.set_size_request(160, -1);
        let test_cancel = gtk::Button::from_icon_name("process-stop-symbolic");
        test_cancel.set_tooltip_text(Some("Cancel the network test (stops the test pattern on the TV)"));
        test_cancel.set_valign(gtk::Align::Center);
        test_cancel.add_css_class("flat");
        test_row.add_suffix(&test_bar);
        test_row.add_suffix(&test_cancel);
        test_row.set_visible(false);

        let link_row = adw::ActionRow::builder().title("This PC's link").subtitle("—").build();
        link_row.add_css_class("property");

        let verdict_row = adw::ActionRow::builder().title("Verdict").subtitle("Pick a file").build();

        group.add(&demand_row);
        group.add(&analysis_row);
        group.add(&capacity_row);
        group.add(&test_row);
        group.add(&link_row);
        group.add(&verdict_row);

        Rc::new(NetCard {
            group,
            demand_row,
            analysis_row,
            analysis_bar,
            analysis_cancel,
            capacity_row,
            test_btn,
            test_row,
            test_cancel,
            test_bar,
            link_row,
            verdict_row,
            fix_rows: RefCell::new(Vec::new()),
        })
    }

    /// Nothing selected / not a local video.
    pub fn clear(&self, msg: &str) {
        self.demand_row.set_subtitle("—");
        self.analysis_row.set_visible(false);
        self.set_verdict_text(None, msg);
        self.clear_fixes();
    }

    pub fn set_demand(&self, d: &Demand) {
        let s = &d.stats;
        let text = if d.provisional {
            format!(
                "Provisional: peak ≈ {} (average {} × 1.5)",
                fmt_bps(s.peak_10s_bps),
                fmt_bps(s.avg_bps)
            )
        } else {
            format!(
                "Peak {} over 10 s, {} over 2 s, average {}",
                fmt_bps(s.peak_10s_bps),
                fmt_bps(s.peak_2s_bps),
                fmt_bps(s.avg_bps)
            )
        };
        self.demand_row.set_subtitle(&text);
    }

    /// `Some(fraction)` while the packet analysis runs, `None` when finished/cancelled.
    pub fn set_analysis(&self, p: Option<f32>) {
        match p {
            Some(f) => {
                self.analysis_row.set_visible(true);
                self.analysis_bar.set_fraction(f64::from(f.clamp(0.0, 1.0)));
                self.analysis_bar.set_text(Some(&format!("{:.0} %", f * 100.0)));
            }
            None => self.analysis_row.set_visible(false),
        }
    }

    pub fn set_cap_info(&self, c: &CapInfo) {
        let text = match &c.cap {
            Some(m) => {
                let mut t = format!(
                    "{} measured {} (best second {})",
                    fmt_mbps(m.mbps),
                    fmt_age(c.age_secs),
                    fmt_mbps(m.peak_mbps)
                );
                if c.stale {
                    t.push_str(" · stale: older than 24 h or another Wi-Fi link, test again");
                }
                t
            }
            None => "Not measured: run “Test network” for an accurate verdict".into(),
        };
        self.capacity_row.set_subtitle(&glib::markup_escape_text(&text));
        if c.stale {
            self.capacity_row.add_css_class("warning");
        } else {
            self.capacity_row.remove_css_class("warning");
        }
        let link = match &c.link {
            Some(l) => {
                let mut t = l.describe();
                t.push_str(match (l.wireless, c.tv) {
                    (true, Some(TvNetwork::Wireless)) => ". The TV is on Wi-Fi too, so the stream crosses the access point twice.",
                    (true, Some(TvNetwork::Wired)) => ". The TV is wired, so only this PC's Wi-Fi hop counts.",
                    (true, None) => ". If the TV is on Wi-Fi too, the stream crosses the access point twice.",
                    (false, Some(TvNetwork::Wireless)) => ". The TV is on Wi-Fi.",
                    (false, Some(TvNetwork::Wired)) => ". The TV is wired too.",
                    (false, None) => "",
                });
                t
            }
            None => "Unknown (no default route found)".into(),
        };
        self.link_row.set_subtitle(&glib::markup_escape_text(&link));
    }

    /// Network test progress; `None` hides the row.
    pub fn set_test_progress(&self, p: Option<&CapacityProgress>) {
        let Some(p) = p else {
            self.test_row.set_visible(false);
            self.test_btn.set_sensitive(true);
            return;
        };
        if !self.test_row.is_visible() {
            self.test_cancel.set_sensitive(true);
        }
        self.test_row.set_visible(true);
        self.test_btn.set_sensitive(false);
        match p {
            CapacityProgress::GeneratingClip(f) => {
                self.test_row.set_subtitle("Encoding the probe clip (first run only, about a minute)…");
                self.test_bar.set_fraction(f64::from(f.clamp(0.0, 1.0)));
            }
            CapacityProgress::Starting => {
                self.test_row.set_subtitle("Starting the test pattern on the TV…");
                self.test_bar.pulse();
            }
            CapacityProgress::Measuring { elapsed, mbps } => {
                self.test_row
                    .set_subtitle(&format!("Measuring… {elapsed:.0} s, {}", fmt_mbps(*mbps)));
                let total = netcheck::MEASURE_SECS as f64;
                self.test_bar.set_fraction((elapsed / total).clamp(0.0, 1.0));
            }
            CapacityProgress::Done => {
                self.test_row.set_subtitle("Done");
                self.test_bar.set_fraction(1.0);
            }
        }
    }

    /// The user asked to cancel the running network test.
    pub fn set_test_cancelling(&self) {
        self.test_cancel.set_sensitive(false);
        self.test_row.set_subtitle("Cancelling… (stopping the test pattern on the TV)");
    }

    fn set_verdict_text(&self, level: Option<tvremote_core::VerdictLevel>, text: &str) {
        for c in ["success", "warning", "error"] {
            self.verdict_row.remove_css_class(c);
        }
        match level {
            Some(l) => {
                self.verdict_row.set_title(verdict_title(l));
                self.verdict_row.add_css_class(verdict_css(l));
            }
            None => self.verdict_row.set_title("Verdict"),
        }
        self.verdict_row.set_subtitle(&glib::markup_escape_text(text));
    }

    fn clear_fixes(&self) {
        for r in self.fix_rows.borrow_mut().drain(..) {
            self.group.remove(&r);
        }
    }

    /// Show the verdict and its fixes. `apply` is called when a fix's button is clicked
    /// (only for fixes that carry a profile). Fixes needing VAAPI are hidden without it.
    pub fn set_verdict(&self, v: &Verdict, vaapi: Option<bool>, apply: Rc<dyn Fn(Fix)>) {
        self.set_verdict_text(Some(v.level), &v.explanation);
        self.clear_fixes();
        for fix in v.fixes.iter().filter(|f| !f.requires_vaapi || vaapi == Some(true)) {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&fix.description).as_str())
                .subtitle(format!(
                    "{} → {}",
                    fix.resulting_level.badge(),
                    fmt_mbps(fix.resulting_mbps)
                ))
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("dialog-information-symbolic"));
            match fix.kind {
                FixKind::DropExtraTracks { .. } | FixKind::ReduceVideo { .. } => {
                    let b = gtk::Button::with_label("Apply");
                    b.set_valign(gtk::Align::Center);
                    let fix = fix.clone();
                    let apply = apply.clone();
                    b.connect_clicked(move |_| apply(fix.clone()));
                    row.add_suffix(&b);
                    row.set_activatable_widget(Some(&b));
                }
                FixKind::CopyToTv => {
                    row.set_subtitle(&format!(
                        "{} · copy the file to a USB drive or the TV's storage and play it there",
                        fix.resulting_level.badge()
                    ));
                }
            }
            self.group.add(&row);
            self.fix_rows.borrow_mut().push(row);
        }
    }
}
