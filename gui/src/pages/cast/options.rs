//! Cast options: Direct vs TV-friendly, and the TV-friendly selectors (audio track,
//! subtitles, video quality) with the live bitrate estimate.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use tvremote_core::cast::{
    direct_bitrate, estimated_bitrate, video_bitrate_estimate, AudioHandling, CastProfile,
    MediaInfo, VideoMode,
};

use crate::format::{audio_track_label, fmt_bps, quality_label, quality_steps, subtitle_label};

pub struct Options {
    pub group: adw::PreferencesGroup,
    mode: adw::ComboRow,
    audio: adw::ComboRow,
    subtitle: adw::ComboRow,
    quality: adw::ComboRow,
    estimate: adw::ActionRow,
    audio_idx: RefCell<Vec<Option<u32>>>,
    sub_idx: RefCell<Vec<Option<u32>>>,
    quality_vals: RefCell<Vec<Option<f64>>>,
    vaapi: Cell<Option<bool>>,
    updating: Cell<bool>,
    base: RefCell<CastProfile>,
    on_change: RefCell<Option<Box<dyn Fn()>>>,
}

impl Options {
    pub fn new() -> Rc<Options> {
        let group = adw::PreferencesGroup::builder()
            .title("How to cast")
            .build();
        let mode = adw::ComboRow::builder().title("Mode").build();
        mode.set_model(Some(&gtk::StringList::new(&[
            "Direct: file as-is, seekable",
            "TV-friendly: converted on the fly",
        ])));
        let audio = adw::ComboRow::builder().title("Audio track").build();
        let subtitle = adw::ComboRow::builder().title("Subtitles").build();
        let quality = adw::ComboRow::builder()
            .title("Video quality")
            .subtitle("Reduced re-encodes with VAAPI (HDR kept)")
            .build();
        let estimate = adw::ActionRow::builder().title("Estimated stream bitrate").build();
        estimate.add_css_class("property");
        group.add(&mode);
        group.add(&audio);
        group.add(&subtitle);
        group.add(&quality);
        group.add(&estimate);

        let this = Rc::new(Options {
            group,
            mode,
            audio,
            subtitle,
            quality,
            estimate,
            audio_idx: RefCell::new(Vec::new()),
            sub_idx: RefCell::new(Vec::new()),
            quality_vals: RefCell::new(Vec::new()),
            vaapi: Cell::new(None),
            updating: Cell::new(false),
            base: RefCell::new(CastProfile::default()),
            on_change: RefCell::new(None),
        });
        for row in [&this.mode, &this.audio, &this.subtitle, &this.quality] {
            let w = Rc::downgrade(&this);
            row.connect_selected_notify(move |_| {
                let Some(o) = w.upgrade() else { return };
                if o.updating.get() {
                    return;
                }
                o.sync_rows();
                let cb = o.on_change.borrow();
                if let Some(f) = cb.as_ref() {
                    f();
                }
            });
        }
        this
    }

    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        *self.on_change.borrow_mut() = Some(Box::new(f));
    }

    pub fn set_vaapi(&self, v: bool) {
        self.vaapi.set(Some(v));
        self.sync_rows();
    }

    /// Fill the selectors for `info` and select `tv_friendly` / `profile`.
    pub fn set_media(&self, info: &MediaInfo, tv_friendly: bool, profile: &CastProfile) {
        self.updating.set(true);
        *self.base.borrow_mut() = profile.clone();

        // Audio
        let mut labels: Vec<String> = Vec::new();
        let mut idx: Vec<Option<u32>> = Vec::new();
        for a in info.audio_streams() {
            labels.push(audio_track_label(a));
            idx.push(Some(a.index));
        }
        labels.push("No audio".into());
        idx.push(None);
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        self.audio.set_model(Some(&gtk::StringList::new(&refs)));
        *self.audio_idx.borrow_mut() = idx;

        // Subtitles (text only; image subs can't be kept as a soft track)
        let mut labels: Vec<String> = vec!["None".into()];
        let mut idx: Vec<Option<u32>> = vec![None];
        for s in info.subtitle_streams().filter(|s| s.is_text_subtitle()) {
            labels.push(subtitle_label(s));
            idx.push(Some(s.index));
        }
        let image_subs = info.subtitle_streams().filter(|s| !s.is_text_subtitle()).count();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        self.subtitle.set_model(Some(&gtk::StringList::new(&refs)));
        self.subtitle.set_subtitle(&if image_subs > 0 {
            format!("{image_subs} image-based track(s) (PGS/VobSub) can't be sent")
        } else {
            String::new()
        });
        *self.sub_idx.borrow_mut() = idx;

        // Quality
        let src = video_bitrate_estimate(info).map(|b| b as f64 / 1e6);
        // Offer the selected target and the VAAPI recommendation (the latter may be
        // selected later, once VAAPI is known).
        let target = |v: VideoMode| match v {
            VideoMode::Reduce { target_mbps } => Some(target_mbps),
            VideoMode::Copy => None,
        };
        let rec = target(profile.video).or(target(CastProfile::recommended(info, true).video));
        let mut vals: Vec<Option<f64>> = vec![None];
        vals.extend(quality_steps(src, rec).into_iter().map(Some));
        let labels: Vec<String> = vals
            .iter()
            .map(|v| match v {
                None => match src {
                    Some(s) => format!("Original ({s:.0} Mbps)"),
                    None => "Original".into(),
                },
                Some(m) => quality_label(*m),
            })
            .collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        self.quality.set_model(Some(&gtk::StringList::new(&refs)));
        *self.quality_vals.borrow_mut() = vals;

        self.select_profile(tv_friendly, profile);
        self.updating.set(false);
        self.sync_rows();
    }

    /// Select `profile` in the existing selectors (after a fix was applied).
    pub fn select_profile(&self, tv_friendly: bool, profile: &CastProfile) {
        let was = self.updating.replace(true);
        *self.base.borrow_mut() = profile.clone();
        self.mode.set_selected(u32::from(tv_friendly));
        let a = self
            .audio_idx
            .borrow()
            .iter()
            .position(|i| *i == profile.audio)
            .unwrap_or(0);
        self.audio.set_selected(a as u32);
        let s = self
            .sub_idx
            .borrow()
            .iter()
            .position(|i| *i == profile.subtitle)
            .unwrap_or(0);
        self.subtitle.set_selected(s as u32);
        let q = match profile.video {
            VideoMode::Copy => 0,
            VideoMode::Reduce { target_mbps } => self
                .quality_vals
                .borrow()
                .iter()
                .position(|v| v.is_some_and(|m| (m - target_mbps).abs() < 0.05))
                .unwrap_or(0),
        };
        self.quality.set_selected(q as u32);
        self.updating.set(was);
        self.sync_rows();
    }

    fn sync_rows(&self) {
        let tvf = self.mode.selected() == 1;
        self.audio.set_visible(tvf);
        self.subtitle.set_visible(tvf);
        self.quality.set_visible(tvf && self.vaapi.get() == Some(true));
    }

    /// Current choice: (TV-friendly?, profile).
    pub fn read(&self) -> (bool, CastProfile) {
        let tvf = self.mode.selected() == 1;
        let mut p = self.base.borrow().clone();
        p.audio = self
            .audio_idx
            .borrow()
            .get(self.audio.selected() as usize)
            .copied()
            .flatten();
        p.audio_handling = AudioHandling::Auto;
        p.subtitle = self
            .sub_idx
            .borrow()
            .get(self.subtitle.selected() as usize)
            .copied()
            .flatten();
        let q = if self.vaapi.get() == Some(true) {
            self.quality_vals
                .borrow()
                .get(self.quality.selected() as usize)
                .copied()
                .flatten()
        } else {
            None
        };
        p.video = match q {
            Some(m) => VideoMode::Reduce { target_mbps: m },
            None => VideoMode::Copy,
        };
        (tvf, p)
    }

    /// Update the "estimated stream bitrate" row for the current choice.
    pub fn update_estimate(&self, info: &MediaInfo) {
        let (tvf, p) = self.read();
        if tvf {
            let est = estimated_bitrate(info, &p);
            let mut parts = vec![format!("video {}", fmt_bps(est.video_bps))];
            if est.audio_bps > 0 {
                parts.push(format!("audio {}", fmt_bps(est.audio_bps)));
            }
            if est.subtitle_bps > 0 {
                parts.push(format!("subtitles {}", fmt_bps(est.subtitle_bps)));
            }
            self.estimate.set_subtitle(&format!(
                "≈ {} average ({})",
                fmt_bps(est.total_bps),
                parts.join(" + ")
            ));
        } else {
            let b = direct_bitrate(info);
            self.estimate.set_subtitle(&if b > 0 {
                format!("≈ {} average (whole file, every track)", fmt_bps(b))
            } else {
                "Unknown".into()
            });
        }
    }
}
