//! Cast flow state: the profile form (audio / subtitles / video quality) and the
//! network-verdict screen. Pure state; rendering is in `ui.rs`, threads in `worker.rs`.

use std::sync::Arc;

use tvremote_core::cast::{
    check_compat, direct_bitrate, estimated_bitrate, is_tv_safe_audio, video_bitrate_estimate,
    AudioHandling, CastProfile, CompatReport, MediaInfo, StreamContainer, StreamInfo, VideoMode,
};
use tvremote_core::netcheck::{DemandJob, PacketProfile};
use tvremote_core::{CastMode, CompatLevel};

use crate::worker::VerdictBundle;

/// Reduced-quality targets offered (Mbit/s), filtered to those below the source.
pub const REDUCE_STEPS: &[f64] = &[60.0, 40.0, 30.0, 20.0, 15.0, 10.0, 6.0];

/// Rows of the form.
pub const ROW_MODE: usize = 0;
pub const ROW_AUDIO: usize = 1;
pub const ROW_SUB: usize = 2;
pub const ROW_VIDEO: usize = 3;
pub const ROWS: usize = 4;

/// Audio track label; tracks the TV cannot decode (DTS, TrueHD) show the conversion.
pub fn audio_label(s: &StreamInfo) -> String {
    let mut l = s.label();
    if !is_tv_safe_audio(&s.codec) {
        l.push_str(" → AC3");
    }
    if s.default {
        l.push_str(" (default)");
    }
    l
}

/// Video quality label.
pub fn video_label(v: &VideoMode) -> String {
    match v {
        VideoMode::Copy => "Original (copied, HDR kept)".into(),
        VideoMode::Reduce { target_mbps } => format!("Reduced {target_mbps:.0} Mbps (hevc_vaapi)"),
    }
}

#[derive(Debug, Clone)]
pub struct ProfileForm {
    pub info: MediaInfo,
    pub report: CompatReport,
    /// TV-friendly (ffmpeg) instead of a direct cast.
    pub friendly: bool,
    pub audio_opts: Vec<Option<u32>>,
    pub audio_sel: usize,
    pub sub_opts: Vec<Option<u32>>,
    pub sub_sel: usize,
    /// Image-based subtitles that cannot be kept.
    pub image_subs: usize,
    pub video_opts: Vec<VideoMode>,
    pub video_sel: usize,
    /// `None` while the VAAPI probe is running.
    pub vaapi: Option<bool>,
    pub row: usize,
}

impl ProfileForm {
    pub fn new(info: MediaInfo, vaapi: Option<bool>) -> ProfileForm {
        let report = check_compat(&info);
        let rec = CastProfile::recommended(&info, vaapi == Some(true));
        let mut audio_opts: Vec<Option<u32>> =
            info.audio_streams().map(|s| Some(s.index)).collect();
        audio_opts.push(None);
        let mut sub_opts = vec![None];
        sub_opts.extend(
            info.subtitle_streams()
                .filter(|s| s.is_text_subtitle())
                .map(|s| Some(s.index)),
        );
        let image_subs = info
            .subtitle_streams()
            .filter(|s| !s.is_text_subtitle())
            .count();
        let mut f = ProfileForm {
            friendly: report.level != CompatLevel::Plays,
            report,
            audio_opts,
            audio_sel: 0,
            sub_opts,
            sub_sel: 0,
            image_subs,
            video_opts: vec![VideoMode::Copy],
            video_sel: 0,
            vaapi,
            row: ROW_MODE,
            info,
        };
        f.rebuild_video_opts();
        f.select_profile(&rec);
        // select_profile() switches to TV-friendly; keep the compat-based choice.
        f.friendly = f.report.level != CompatLevel::Plays;
        f
    }

    fn rebuild_video_opts(&mut self) {
        let current = self
            .video_opts
            .get(self.video_sel)
            .copied()
            .unwrap_or_default();
        let mut opts = vec![VideoMode::Copy];
        if self.vaapi == Some(true) {
            let src = video_bitrate_estimate(&self.info).map(|b| b as f64 / 1e6);
            for &m in REDUCE_STEPS {
                if src.is_none_or(|s| m < s * 0.95) {
                    opts.push(VideoMode::Reduce { target_mbps: m });
                }
            }
        }
        self.video_opts = opts;
        self.video_sel = self
            .video_opts
            .iter()
            .position(|v| *v == current)
            .unwrap_or(0);
    }

    /// VAAPI probe result arrived.
    pub fn set_vaapi(&mut self, ok: bool) {
        let was_unknown = self.vaapi.is_none();
        self.vaapi = Some(ok);
        self.rebuild_video_opts();
        // The recommended profile may need a re-encode, which we could not offer before.
        if was_unknown && ok && self.video_sel == 0 {
            let rec = CastProfile::recommended(&self.info, true);
            if matches!(rec.video, VideoMode::Reduce { .. }) {
                self.set_video(rec.video);
            }
        }
    }

    fn set_video(&mut self, v: VideoMode) {
        if let Some(i) = self.video_opts.iter().position(|x| *x == v) {
            self.video_sel = i;
        } else {
            self.video_opts.push(v);
            self.video_sel = self.video_opts.len() - 1;
        }
    }

    /// Select a profile (e.g. from a verdict fix); switches to TV-friendly.
    pub fn select_profile(&mut self, p: &CastProfile) {
        self.friendly = true;
        self.audio_sel = self
            .audio_opts
            .iter()
            .position(|a| *a == p.audio)
            .unwrap_or(0);
        self.sub_sel = self
            .sub_opts
            .iter()
            .position(|s| *s == p.subtitle)
            .unwrap_or(0);
        match p.video {
            VideoMode::Copy => self.video_sel = 0,
            VideoMode::Reduce { .. } if self.vaapi == Some(false) => self.video_sel = 0,
            v => self.set_video(v),
        }
    }

    pub fn profile(&self) -> CastProfile {
        CastProfile {
            audio: self.audio_opts.get(self.audio_sel).copied().flatten(),
            audio_handling: AudioHandling::Auto,
            subtitle: self.sub_opts.get(self.sub_sel).copied().flatten(),
            video: self
                .video_opts
                .get(self.video_sel)
                .copied()
                .unwrap_or_default(),
            container: StreamContainer::Matroska,
        }
    }

    pub fn mode(&self) -> CastMode {
        if self.friendly {
            CastMode::TvFriendly(self.profile())
        } else {
            CastMode::Direct
        }
    }

    /// Average bitrate of what would be sent (bit/s).
    pub fn estimate_bps(&self) -> u64 {
        if self.friendly {
            estimated_bitrate(&self.info, &self.profile()).total_bps
        } else {
            direct_bitrate(&self.info)
        }
    }

    pub fn move_row(&mut self, delta: isize) {
        self.row = (self.row as isize + delta).clamp(0, ROWS as isize - 1) as usize;
    }

    /// Cycle the value of the current row.
    pub fn cycle(&mut self, delta: isize) {
        fn step(sel: &mut usize, len: usize, d: isize) {
            if len > 0 {
                *sel = (*sel as isize + d).rem_euclid(len as isize) as usize;
            }
        }
        match self.row {
            ROW_MODE => self.friendly = !self.friendly,
            ROW_AUDIO => {
                step(&mut self.audio_sel, self.audio_opts.len(), delta);
                self.friendly = true;
            }
            ROW_SUB => {
                step(&mut self.sub_sel, self.sub_opts.len(), delta);
                self.friendly = true;
            }
            ROW_VIDEO => {
                step(&mut self.video_sel, self.video_opts.len(), delta);
                self.friendly = true;
            }
            _ => {}
        }
    }

    pub fn audio_text(&self) -> String {
        match self
            .audio_opts
            .get(self.audio_sel)
            .copied()
            .flatten()
            .and_then(|i| self.info.stream(i))
        {
            Some(s) => audio_label(s),
            None => "No audio".into(),
        }
    }

    pub fn sub_text(&self) -> String {
        match self
            .sub_opts
            .get(self.sub_sel)
            .copied()
            .flatten()
            .and_then(|i| self.info.stream(i))
        {
            Some(s) => format!("{} (soft SRT track)", s.label()),
            None => "None".into(),
        }
    }

    pub fn video_text(&self) -> String {
        self.video_opts
            .get(self.video_sel)
            .map(video_label)
            .unwrap_or_default()
    }

    /// True when the cast cannot work at all with the current settings.
    pub fn blocked_reason(&self) -> Option<String> {
        let needs_reencode = self.report.needs_video_reencode;
        let reduce = matches!(self.profile().video, VideoMode::Reduce { .. });
        if self.report.level == CompatLevel::Unsupported {
            return Some("This file cannot play on the TV, even converted.".into());
        }
        if needs_reencode && (!self.friendly || !reduce) {
            return Some(if self.vaapi == Some(false) {
                "The video must be re-encoded, but VAAPI (hevc_vaapi) is unavailable here.".into()
            } else {
                "The video must be re-encoded: pick a Reduced video quality.".into()
            });
        }
        None
    }
}

/// State of the background packet analysis.
#[derive(Debug, Clone, PartialEq)]
pub enum Analysis {
    Running,
    Done,
    Cancelled,
    Failed(String),
}

/// Network verdict screen.
pub struct VerdictView {
    pub form: ProfileForm,
    /// Generation of the last requested verdict (older results are ignored).
    pub gen: u64,
    pub bundle: Option<VerdictBundle>,
    pub job_id: u64,
    pub job: Option<DemandJob>,
    pub progress: f32,
    pub analysis: Analysis,
    pub packets: Option<Arc<PacketProfile>>,
    /// Waiting for `y` to cast a ❌ verdict anyway.
    pub confirm_bad: bool,
    /// A cast request is in flight.
    pub casting: bool,
    /// Vertical scroll of the verdict text.
    pub scroll: u16,
}

impl VerdictView {
    pub fn new(form: ProfileForm, gen: u64, job_id: u64) -> VerdictView {
        VerdictView {
            form,
            gen,
            bundle: None,
            job_id,
            job: None,
            progress: 0.0,
            analysis: Analysis::Running,
            packets: None,
            confirm_bad: false,
            casting: false,
            scroll: 0,
        }
    }

    pub fn cancel_analysis(&mut self) -> bool {
        if self.analysis == Analysis::Running {
            if let Some(j) = &self.job {
                j.cancel();
            }
            self.analysis = Analysis::Cancelled;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tvremote_core::cast::parse_ffprobe;

    fn remux() -> MediaInfo {
        let json = r#"{
          "format": {"format_name": "matroska,webm", "duration": "600.0", "size": "5000000000", "bit_rate": "66000000"},
          "streams": [
            {"index": 0, "codec_type": "video", "codec_name": "hevc", "width": 3840, "height": 2160,
             "avg_frame_rate": "24000/1001", "pix_fmt": "yuv420p10le", "color_transfer": "smpte2084",
             "tags": {"BPS": "60000000"}, "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "codec_name": "dts", "profile": "DTS-HD MA", "channels": 6,
             "channel_layout": "5.1(side)", "tags": {"language": "rus", "BPS": "3000000"}, "disposition": {"default": 1}},
            {"index": 2, "codec_type": "audio", "codec_name": "eac3", "channels": 6,
             "channel_layout": "5.1(side)", "tags": {"language": "eng", "BPS": "768000"}, "disposition": {"default": 0}},
            {"index": 3, "codec_type": "subtitle", "codec_name": "subrip", "tags": {"language": "fre"}},
            {"index": 4, "codec_type": "subtitle", "codec_name": "hdmv_pgs_subtitle", "tags": {"language": "eng"}}
          ]}"#;
        parse_ffprobe(json, Path::new("/v/film.mkv")).unwrap()
    }

    #[test]
    fn form_defaults_and_labels() {
        let f = ProfileForm::new(remux(), None);
        // DTS default track → needs TV-friendly.
        assert!(f.friendly);
        assert_eq!(f.audio_opts, vec![Some(1), Some(2), None]);
        assert_eq!(f.audio_sel, 0);
        assert!(f.audio_text().contains("→ AC3"));
        assert_eq!(f.sub_opts, vec![None, Some(3)]);
        assert_eq!(f.image_subs, 1);
        assert_eq!(f.video_opts, vec![VideoMode::Copy]);
        assert!(f.blocked_reason().is_none());
        // AC3 640k + 60 Mbps video.
        assert_eq!(f.estimate_bps(), 60_000_000 + 640_000);
    }

    #[test]
    fn cycling_and_vaapi() {
        let mut f = ProfileForm::new(remux(), None);
        f.row = ROW_AUDIO;
        f.cycle(1);
        assert_eq!(f.profile().audio, Some(2));
        assert!(!f.audio_text().contains("→ AC3"));
        f.cycle(1);
        assert_eq!(f.profile().audio, None);
        f.cycle(1);
        assert_eq!(f.profile().audio, Some(1));
        f.cycle(-1);
        assert_eq!(f.profile().audio, None);

        f.set_vaapi(true);
        // 60 Mbps source: steps strictly below ~57 Mbps.
        assert_eq!(
            f.video_opts.len(),
            1 + REDUCE_STEPS.iter().filter(|m| **m < 57.0).count()
        );
        f.row = ROW_VIDEO;
        f.cycle(1);
        assert_eq!(f.profile().video, VideoMode::Reduce { target_mbps: 40.0 });

        f.row = ROW_MODE;
        f.cycle(1);
        assert!(!f.friendly);
        assert_eq!(f.mode(), CastMode::Direct);
        assert_eq!(f.estimate_bps(), 66_000_000);
    }

    #[test]
    fn apply_fix_profile() {
        let mut f = ProfileForm::new(remux(), Some(true));
        f.friendly = false;
        let p = CastProfile {
            audio: Some(2),
            subtitle: Some(3),
            video: VideoMode::Reduce { target_mbps: 33.0 },
            ..Default::default()
        };
        f.select_profile(&p);
        assert!(f.friendly);
        assert_eq!(f.profile().audio, Some(2));
        assert_eq!(f.profile().subtitle, Some(3));
        assert_eq!(f.profile().video, VideoMode::Reduce { target_mbps: 33.0 });
        // Without VAAPI a reduce fix falls back to the original video.
        let mut g = ProfileForm::new(remux(), Some(false));
        g.select_profile(&p);
        assert_eq!(g.profile().video, VideoMode::Copy);
    }
}
