//! Rendering. Pure drawing from `App` state; never calls into core except cheap
//! in-memory getters (`CastSession::status`).

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Gauge, LineGauge, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use tvremote_core::cast::VideoMode;
use tvremote_core::dlna::TransportState;
use tvremote_core::netcheck::{now_unix, FixKind};
use tvremote_core::{CloudState, ConnectionState, PowerState, VerdictLevel};

use crate::app::{
    file_name, visible_fixes, App, ConfirmKind, Overlay, PickerRow, ProbeState, PromptKind, Screen,
};
use crate::browser::{EntryKind, Pane};
use crate::castflow::{Analysis, ProfileForm, ROW_AUDIO, ROW_MODE, ROW_SUB, ROW_VIDEO};
use crate::util::{
    compat_badge, ellipsize, fmt_age, fmt_duration, human_size, mbps, verdict_badge,
};
use crate::worker::Level;

const ACCENT: Color = Color::Cyan;
const DIM: Style = Style::new().fg(Color::DarkGray);

fn key_style() -> Style {
    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
}

/// Reverse video: readable whatever colours the terminal theme maps "blue" to.
fn sel_style() -> Style {
    Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
}

fn level_color(l: VerdictLevel) -> Color {
    match l {
        VerdictLevel::Ok => Color::Green,
        VerdictLevel::Risky => Color::Yellow,
        VerdictLevel::Bad => Color::Red,
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let [status, body, log, hints] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Min(6),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    draw_status(f, app, status);
    match app.screen {
        Screen::Main => draw_legend(f, body),
        Screen::Apps => draw_apps(f, app, body),
        Screen::Browser => draw_browser(f, app, body),
        Screen::Profile => draw_profile(f, app, body),
        Screen::Verdict => draw_verdict(f, app, body),
    }
    draw_log(f, app, log);
    draw_hints(f, app, hints);
    match app.overlay.clone() {
        Overlay::Help => draw_help(f),
        Overlay::Prompt(p) => draw_prompt(f, p.kind, &p.buf),
        Overlay::Confirm(c) => draw_confirm(f, c),
        Overlay::TvPicker => draw_picker(f, app),
        Overlay::None => {
            if let Some(n) = &app.net_test {
                draw_net_test(f, &n.stage, n.ratio);
            }
        }
    }
}

// ---------------------------------------------------------------------- status strip

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let name = app.tv.display_name();
    let others = app.tv_list.len().saturating_sub(1);
    let mut title = vec![
        format!(" {} ", if name.is_empty() { "TV du salon".to_string() } else { name }).bold().fg(ACCENT),
        format!("· {} ", if app.host.is_empty() { "cloud only" } else { &app.host }).into(),
    ];
    if others > 0 {
        title.push(Span::styled(format!("· T: {others} other TV{} ", if others > 1 { "s" } else { "" }), DIM));
    }
    let block = Block::bordered().title(Line::from(title));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [l1, l2, l3] = Layout::vertical([Constraint::Length(1); 3]).areas(inner);

    let (conn, cc) = match &app.conn {
        ConnectionState::Connected => ("connected".to_string(), Color::Green),
        ConnectionState::Connecting => ("connecting… (approve on the TV if asked)".into(), Color::Yellow),
        ConnectionState::Disconnected => ("disconnected".into(), Color::DarkGray),
        ConnectionState::Retrying(r) => (format!("retrying ({})", ellipsize(r, 30)), Color::Yellow),
        ConnectionState::Unauthorized => ("refused: approve on the TV".into(), Color::Red),
    };
    let (power, pc) = match &app.power {
        Some(PowerState::On) => ("on".to_string(), Color::Green),
        Some(PowerState::Standby) => ("standby".into(), Color::Yellow),
        Some(PowerState::Off) => ("off / unreachable".into(), Color::Red),
        Some(PowerState::Unknown(s)) => (s.clone(), Color::Yellow),
        None => ("…".into(), Color::DarkGray),
    };
    let vol = match (app.volume, app.muted) {
        (Some(v), Some(true)) => format!("{v} (muted)"),
        (Some(v), _) => v.to_string(),
        (None, _) => "?".into(),
    };
    f.render_widget(
        Line::from(vec![
            "Remote ".into(),
            Span::styled(format!("● {conn}"), Style::new().fg(cc)),
            "   Power ".into(),
            Span::styled(power, Style::new().fg(pc)),
            "   Volume ".into(),
            Span::styled(vol, Style::new().bold()),
        ]),
        l1,
    );

    match app.session.as_ref().map(|s| s.status()) {
        Some(st) => {
            let state = match &st.state {
                Some(TransportState::Playing) => "▶ playing",
                Some(TransportState::Paused) => "⏸ paused",
                Some(TransportState::Transitioning) => "… buffering",
                Some(TransportState::Stopped) => "■ stopped",
                Some(TransportState::NoMedia) => "no media",
                Some(TransportState::Other(_)) | None => "loading",
            };
            let pos = st.position.map(fmt_duration).unwrap_or_else(|| "–".into());
            let dur = st.duration.map(fmt_duration).unwrap_or_else(|| "–".into());
            let mut spans = vec![
                "Now playing ".into(),
                Span::styled(ellipsize(&st.title, 50), Style::new().bold()),
                format!("  {state}  {pos} / {dur}").into(),
            ];
            if let Some(m) = st.last_mbps {
                spans.push(format!("  ⇣ {m:.1} Mbps").fg(ACCENT));
            }
            if st.tv_friendly {
                spans.push("  [TV-friendly, no seek]".fg(Color::DarkGray));
            }
            f.render_widget(Line::from(spans), l2);
            let ratio = match (st.position, st.duration) {
                (Some(p), Some(d)) if !d.is_zero() => {
                    (p.as_secs_f64() / d.as_secs_f64()).clamp(0.0, 1.0)
                }
                _ => 0.0,
            };
            f.render_widget(
                LineGauge::default()
                    .ratio(ratio)
                    .filled_style(Style::new().fg(ACCENT))
                    .label(""),
                l3,
            );
        }
        None => f.render_widget(
            Line::styled("Nothing cast from here. Press c to cast a file.", DIM),
            l2,
        ),
    }
}

// ---------------------------------------------------------------------- main legend

const LEGEND: &[(&str, &str)] = &[
    ("↑ ↓ ← →", "D-pad"),
    ("Enter", "OK"),
    ("Backspace", "Back"),
    ("h", "Home"),
    ("x", "Exit"),
    ("M  S  I  G", "Menu Source Info Guide"),
    ("+  -", "Volume up / down"),
    ("m", "Mute"),
    ("PgUp PgDn", "Channel + / −"),
    ("0-9", "Number keys"),
    ("F2-F5", "Red Green Yellow Blue"),
    ("p", "Power off / on (Wake-on-LAN)"),
    ("t", "Type text on the TV"),
    ("a", "Apps (launch, YouTube)"),
    ("c", "Cast a file or URL"),
    ("T", "Switch TV / scan for TVs"),
    ("n", "Network test"),
    ("space", "Play / pause"),
    ("s", "Stop"),
    ("<  >", "Rewind / fast forward"),
    ("?", "Help"),
    ("q  Esc", "Quit"),
];

fn legend_lines(items: &[(&str, &str)]) -> Vec<Line<'static>> {
    items
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(format!("{k:>11}  "), key_style()),
                Span::raw(d.to_string()),
            ])
        })
        .collect()
}

fn draw_legend(f: &mut Frame, area: Rect) {
    let block = Block::bordered().title(" Remote ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let half = LEGEND.len().div_ceil(2);
    if inner.width >= 80 {
        let [a, b] = Layout::horizontal([Constraint::Percentage(50); 2]).areas(inner);
        f.render_widget(Paragraph::new(legend_lines(&LEGEND[..half])), a);
        f.render_widget(Paragraph::new(legend_lines(&LEGEND[half..])), b);
    } else {
        f.render_widget(Paragraph::new(legend_lines(LEGEND)), inner);
    }
}

// ---------------------------------------------------------------------- apps

fn draw_apps(f: &mut Frame, app: &mut App, area: Rect) {
    let title = if app.apps_loading {
        " Apps (loading…) "
    } else {
        " Apps "
    };
    let items: Vec<ListItem> = app
        .apps
        .iter()
        .map(|a| {
            ListItem::new(Line::from(vec![
                Span::raw(a.name.clone()),
                Span::styled(format!("  {}", a.app_id), DIM),
            ]))
        })
        .collect();
    let empty = items.is_empty();
    let list = List::new(items)
        .block(Block::bordered().title(title))
        .highlight_style(sel_style())
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, area, &mut app.apps_state);
    if empty && !app.apps_loading {
        let inner = area.inner(ratatui::layout::Margin::new(2, 1));
        f.render_widget(
            Paragraph::new("No apps loaded. Press r to fetch them (the TV must be on).").style(DIM),
            inner,
        );
    }
}

// ---------------------------------------------------------------------- browser

fn draw_browser(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(b) = &app.browser else { return };
    let [left, right] =
        Layout::horizontal([Constraint::Length(28), Constraint::Min(30)]).areas(area);

    let bm_items: Vec<ListItem> = b
        .bookmarks
        .iter()
        .map(|m| ListItem::new(m.label.clone()))
        .collect();
    let bm_focus = b.focus == Pane::Bookmarks;
    let bm = List::new(bm_items)
        .block(
            Block::bordered()
                .title(" Places ")
                .border_style(if bm_focus {
                    Style::new().fg(ACCENT)
                } else {
                    Style::new()
                }),
        )
        .highlight_style(if bm_focus {
            sel_style()
        } else {
            Style::new().add_modifier(Modifier::BOLD)
        })
        .highlight_symbol("› ");
    app.bm_state.select(Some(b.bm_selected));
    f.render_stateful_widget(bm, left, &mut app.bm_state);

    let width = right.width.saturating_sub(4) as usize;
    // Names get the room they need; the details column keeps at least 20 columns.
    let longest = b.entries.iter().map(|e| e.name.chars().count() + 1).max().unwrap_or(16);
    let name_w = longest.min(width.saturating_sub(6 + 10 + 20)).max(16);
    let items: Vec<ListItem> = b
        .entries
        .iter()
        .map(|e| match &e.kind {
            EntryKind::Parent => ListItem::new(Line::styled("      ../", DIM)),
            EntryKind::Dir => ListItem::new(Line::from(vec![
                Span::raw("      "),
                Span::styled(
                    format!("{}/", ellipsize(&e.name, name_w)),
                    Style::new().fg(Color::Blue).bold(),
                ),
            ])),
            EntryKind::File { size, .. } => {
                let (badge, detail, bstyle) = match app.probes.get(&e.path) {
                    Some(ProbeState::Done(info, rep)) => {
                        let level = match app.vaapi {
                            Some(v) => rep.effective_level(v),
                            None => rep.level,
                        };
                        let color = match level {
                            tvremote_core::CompatLevel::Plays => Color::Green,
                            tvremote_core::CompatLevel::NeedsTvFriendly => Color::Yellow,
                            tvremote_core::CompatLevel::Unsupported => Color::Red,
                        };
                        (
                            compat_badge(level, app.emoji).to_string(),
                            media_summary(info),
                            Style::new().fg(color),
                        )
                    }
                    Some(ProbeState::Failed(_)) => (
                        "?".into(),
                        "ffprobe failed".into(),
                        Style::new().fg(Color::Red),
                    ),
                    None => ("…".into(), String::new(), DIM),
                };
                ListItem::new(Line::from(vec![
                    // Pad by display width: ✅ is 2 columns, ⚠ only 1.
                    Span::styled(
                        format!("{badge}{}", " ".repeat(6usize.saturating_sub(Span::raw(badge.as_str()).width()))),
                        bstyle,
                    ),
                    Span::raw(format!("{:<name_w$}", ellipsize(&e.name, name_w))),
                    Span::styled(format!(" {:>9}", human_size(*size)), DIM),
                    Span::styled(format!("  {detail}"), Style::new().fg(Color::Gray)),
                ]))
            }
        })
        .collect();
    let files_focus = b.focus == Pane::Files;
    let title = format!(
        " {} ",
        ellipsize(
            &b.cwd.display().to_string(),
            right.width.saturating_sub(6) as usize
        )
    );
    let list = List::new(items)
        .block(Block::bordered().title(title).border_style(if files_focus {
            Style::new().fg(ACCENT)
        } else {
            Style::new()
        }))
        .highlight_style(if files_focus {
            sel_style()
        } else {
            Style::new().add_modifier(Modifier::BOLD)
        })
        .highlight_symbol("");
    app.files_state.select(if b.entries.is_empty() {
        None
    } else {
        Some(b.selected)
    });
    f.render_stateful_widget(list, right, &mut app.files_state);
}

/// `3840×2160 hevc HDR10 · dts` style summary for the file list.
fn media_summary(info: &tvremote_core::MediaInfo) -> String {
    let mut parts = Vec::new();
    if let Some(v) = info.video() {
        let mut s = String::new();
        if let (Some(w), Some(h)) = (v.width, v.height) {
            s.push_str(&format!("{w}×{h} "));
        }
        s.push_str(&v.codec);
        if v.hdr != tvremote_core::cast::HdrKind::Sdr {
            s.push_str(&format!(" {:?}", v.hdr).to_uppercase());
        }
        parts.push(s);
    }
    let audio: Vec<String> = info
        .audio_streams()
        .take(3)
        .map(|a| a.codec.clone())
        .collect();
    if !audio.is_empty() {
        let more = info.audio_streams().count().saturating_sub(3);
        let mut a = audio.join(",");
        if more > 0 {
            a.push_str(&format!(" +{more}"));
        }
        parts.push(a);
    }
    if let Some(d) = info.duration() {
        parts.push(fmt_duration(d));
    }
    parts.join(" · ")
}

// ---------------------------------------------------------------------- profile form

fn form_row<'a>(selected: bool, enabled: bool, label: &'a str, value: String) -> Line<'a> {
    let marker = if selected { "▶ " } else { "  " };
    let vstyle = if selected {
        sel_style()
    } else if enabled {
        Style::new().bold()
    } else {
        DIM
    };
    Line::from(vec![
        Span::raw(marker),
        Span::styled(format!("{label:<14}"), Style::new().fg(ACCENT)),
        Span::styled(format!("◀ {value} ▶"), vstyle),
    ])
}

fn draw_profile(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(form) = &app.form else { return };
    let title = format!(" Cast: {} ", ellipsize(&file_name(&form.info.path), 60));
    let mut lines = file_header(form, app.emoji, app.vaapi);
    lines.push(Line::raw(""));
    let fr = form.friendly;
    lines.push(form_row(
        form.row == ROW_MODE,
        true,
        "Mode",
        if fr {
            "TV-friendly (ffmpeg, no seeking)".into()
        } else {
            "Direct (file as-is, seekable)".into()
        },
    ));
    lines.push(form_row(
        form.row == ROW_AUDIO,
        fr,
        "Audio",
        form.audio_text(),
    ));
    lines.push(form_row(
        form.row == ROW_SUB,
        fr,
        "Subtitles",
        form.sub_text(),
    ));
    let video = match form.vaapi {
        None => format!("{}  (checking VAAPI…)", form.video_text()),
        Some(false) => format!("{}  (no VAAPI: reduce unavailable)", form.video_text()),
        Some(true) => form.video_text(),
    };
    lines.push(form_row(form.row == ROW_VIDEO, fr, "Video", video));
    if form.image_subs > 0 {
        lines.push(Line::styled(
            format!(
                "                {} image-based subtitle track(s) (PGS/VobSub) cannot be kept.",
                form.image_subs
            ),
            DIM,
        ));
    }
    lines.push(Line::raw(""));
    let est = form.estimate_bps();
    let mut est_line = vec![
        Span::raw("Estimated stream bitrate: "),
        Span::styled(mbps(est), Style::new().bold().fg(ACCENT)),
    ];
    if fr {
        let p = form.profile();
        let e = tvremote_core::cast::estimated_bitrate(&form.info, &p);
        est_line.push(Span::styled(
            format!(
                "  (video {}, audio {})",
                mbps(e.video_bps),
                mbps(e.audio_bps)
            ),
            DIM,
        ));
    } else {
        est_line.push(Span::styled("  (all tracks, container average)", DIM));
    }
    lines.push(Line::from(est_line));
    if let Some(r) = form.blocked_reason() {
        lines.push(Line::styled(r, Style::new().fg(Color::Red)));
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(title)),
        area,
    );
}

fn file_header(form: &ProfileForm, emoji: bool, vaapi: Option<bool>) -> Vec<Line<'static>> {
    let info = &form.info;
    let level = vaapi
        .map(|v| form.report.effective_level(v))
        .unwrap_or(form.report.level);
    let mut lines = vec![Line::from(vec![
        Span::raw(format!("{}  ", compat_badge(level, emoji))),
        Span::styled(media_summary(info), Style::new().bold()),
        Span::styled(
            format!(
                "  {} · {}",
                info.size.map(human_size).unwrap_or_default(),
                info.format_name
            ),
            DIM,
        ),
    ])];
    for i in form.report.issues.iter().take(4) {
        lines.push(Line::styled(
            format!(
                "   • {}{}",
                i.message,
                if i.fixable {
                    " (TV-friendly fixes this)"
                } else {
                    ""
                }
            ),
            Style::new().fg(Color::Gray),
        ));
    }
    lines
}

// ---------------------------------------------------------------------- verdict

fn draw_verdict(f: &mut Frame, app: &mut App, area: Rect) {
    let emoji = app.emoji;
    let vaapi = app.vaapi;
    let Some(v) = &app.verdict else { return };
    let title = format!(
        " Network check: {} ",
        ellipsize(&file_name(&v.form.info.path), 60)
    );
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let show_gauge = v.analysis == Analysis::Running;
    let [gauge_area, text_area] = Layout::vertical([
        Constraint::Length(if show_gauge { 1 } else { 0 }),
        Constraint::Min(1),
    ])
    .areas(inner);
    if show_gauge {
        f.render_widget(
            Gauge::default()
                .ratio(v.progress.clamp(0.0, 1.0) as f64)
                .label(format!(
                    "Analysing packets {:.0} %  (Esc cancels)",
                    v.progress * 100.0
                ))
                .gauge_style(Style::new().fg(ACCENT).bg(Color::Black)),
            gauge_area,
        );
    }

    let mut lines: Vec<Line> = Vec::new();
    let mode = match v.form.mode() {
        tvremote_core::CastMode::Direct => "Direct cast (all tracks)".to_string(),
        tvremote_core::CastMode::TvFriendly(p) => {
            let video = match p.video {
                VideoMode::Copy => "video copied".to_string(),
                VideoMode::Reduce { target_mbps } => {
                    format!("video reduced to {target_mbps:.0} Mbps")
                }
            };
            format!(
                "TV-friendly: {} · {} · subtitles: {}",
                v.form.audio_text(),
                video,
                v.form.sub_text()
            )
        }
    };
    lines.push(Line::from(vec!["Profile   ".fg(ACCENT), Span::raw(mode)]));
    match &v.analysis {
        Analysis::Cancelled => lines.push(Line::styled(
            "          Packet analysis cancelled: provisional estimate.",
            DIM,
        )),
        Analysis::Failed(e) => lines.push(Line::styled(
            format!("          Packet analysis failed: {e}"),
            Style::new().fg(Color::Red),
        )),
        _ => {}
    }

    match &v.bundle {
        None => lines.push(Line::styled("Computing…", DIM)),
        Some(b) => {
            let d = &b.demand;
            lines.push(Line::from(vec![
                "Demand    ".fg(ACCENT),
                Span::styled(
                    format!("peak {:.1} Mbps", d.peak_mbps()),
                    Style::new().bold(),
                ),
                Span::raw(if d.provisional {
                    format!(" (provisional: average {} × 1.5)", mbps(d.stats.avg_bps))
                } else {
                    format!(
                        " over 10 s · {} over 2 s · average {}",
                        mbps(d.stats.peak_2s_bps),
                        mbps(d.stats.avg_bps)
                    )
                }),
            ]));
            let cap_line = match &b.capacity {
                Some(c) => {
                    let age = fmt_age(now_unix().saturating_sub(c.measured_at));
                    let mut spans = vec![
                        "Capacity  ".fg(ACCENT),
                        Span::styled(format!("{:.1} Mbps", c.mbps), Style::new().bold()),
                        Span::raw(format!(" measured {age}")),
                    ];
                    if b.verdict.capacity_stale {
                        spans.push(Span::styled(
                            " (stale: press n to re-run the network test)",
                            Style::new().fg(Color::Yellow),
                        ));
                    }
                    Line::from(spans)
                }
                None => Line::from(vec![
                    "Capacity  ".fg(ACCENT),
                    Span::styled("not measured", Style::new().fg(Color::Yellow)),
                    Span::raw(" (press n to run the network test)"),
                ]),
            };
            lines.push(cap_line);
            if let Some(l) = &b.link {
                lines.push(Line::from(vec![
                    "Link      ".fg(ACCENT),
                    Span::raw(l.describe()),
                ]));
            }
            lines.push(Line::raw(""));
            let lv = b.verdict.level;
            lines.push(Line::from(vec![
                "Verdict   ".fg(ACCENT),
                Span::styled(
                    format!(
                        "{} {}",
                        verdict_badge(lv, emoji),
                        match lv {
                            VerdictLevel::Ok => "should play smoothly",
                            VerdictLevel::Risky => "might stutter",
                            VerdictLevel::Bad => "will most likely stutter",
                        }
                    ),
                    Style::new().fg(level_color(lv)).bold(),
                ),
            ]));
            lines.push(Line::raw(b.verdict.explanation.clone()));
            let fixes = visible_fixes(b, vaapi);
            if !fixes.is_empty() {
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "Fixes (press the number to apply):",
                    Style::new().bold(),
                ));
                for (i, fx) in fixes.iter().enumerate().take(9) {
                    let applicable = !matches!(fx.kind, FixKind::CopyToTv);
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!(" {} ", i + 1),
                            if applicable { key_style() } else { DIM },
                        ),
                        Span::styled(
                            format!("{} ", verdict_badge(fx.resulting_level, emoji)),
                            Style::new().fg(level_color(fx.resulting_level)),
                        ),
                        Span::raw(fx.description.clone()),
                        Span::styled(format!("  → {:.1} Mbps", fx.resulting_mbps), DIM),
                        Span::styled(if applicable { "" } else { "  (manual)" }, DIM),
                    ]));
                }
            }
        }
    }
    lines.push(Line::raw(""));
    if v.casting {
        lines.push(Line::styled(
            "Starting the cast… (up to 15 s)",
            Style::new().fg(ACCENT).bold(),
        ));
    } else if v.confirm_bad {
        lines.push(Line::styled(
            "This will most likely stutter. Type y to cast anyway, any other key to cancel.",
            Style::new().fg(Color::Red).bold(),
        ));
    } else {
        lines.push(Line::styled(
            "Enter: cast   Backspace: back to the profile   ↑↓: scroll",
            DIM,
        ));
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((v.scroll, 0)),
        text_area,
    );
}

// ---------------------------------------------------------------------- log + hints

fn draw_log(f: &mut Frame, app: &App, area: Rect) {
    if let Some((_, level, msg)) = app.log.back() {
        let color = match level {
            Level::Info => Color::Gray,
            Level::Warn => Color::Yellow,
            Level::Error => Color::Red,
        };
        f.render_widget(
            Line::styled(format!(" {msg}"), Style::new().fg(color)),
            area,
        );
    }
}

fn draw_hints(f: &mut Frame, app: &App, area: Rect) {
    let h = match (&app.overlay, app.screen) {
        (Overlay::Prompt(_), _) => "Enter: send   Esc: cancel   Ctrl-U: clear",
        (Overlay::Confirm(_), _) => "y: yes   any other key: no",
        (Overlay::Help, _) => "any key: close",
        (Overlay::TvPicker, _) => "↑↓ select · Enter switch / add · s scan LAN + SmartThings · Esc close",
        (_, Screen::Main) => "c cast · a apps · t type · T TVs · n network test · ? help · q quit",
        (_, Screen::Apps) => "↑↓ select · Enter launch · r refresh · y YouTube URL · Esc back",
        (_, Screen::Browser) => {
            "↑↓ move · Enter open/choose · Backspace up · Tab places · u URL · . hidden · Esc back"
        }
        (_, Screen::Profile) => "↑↓ row · ←→ change · Enter network check · Esc back",
        (_, Screen::Verdict) => {
            "Enter cast · 1-9 apply fix · n network test · Esc cancel analysis/back · ↑↓ scroll"
        }
    };
    f.render_widget(Line::styled(format!(" {h}"), DIM), area);
}

// ---------------------------------------------------------------------- overlays

fn popup(area: Rect, w: u16, h: u16) -> Rect {
    let [v] = Layout::vertical([Constraint::Length(h.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [r] = Layout::horizontal([Constraint::Length(w.min(area.width))])
        .flex(Flex::Center)
        .areas(v);
    r
}

fn draw_help(f: &mut Frame) {
    let mut lines = vec![Line::styled(
        "Main view: remote keys",
        Style::new().bold().fg(ACCENT),
    )];
    lines.extend(legend_lines(LEGEND));
    lines.push(Line::raw(""));
    lines.push(Line::styled("Cast flow", Style::new().bold().fg(ACCENT)));
    lines.extend(legend_lines(&[
        (
            "c",
            "File browser: Tab switches places/files, . shows hidden",
        ),
        ("u", "Type a URL instead (in the browser)"),
        ("Enter", "Choose the file → profile form"),
        ("← →", "Change audio / subtitles / quality"),
        ("Enter", "Network check, then Enter casts"),
        ("1-9", "Apply a suggested fix"),
        ("y", "Confirm casting a ❌ verdict"),
    ]));
    lines.push(Line::raw(""));
    lines.push(Line::styled("Badges: ✅/OK plays · ⚠/WARN needs TV-friendly or risky · ❌/BAD won't play / will stutter", DIM));
    let h = lines.len() as u16 + 2;
    let area = popup(f.area(), 78, h);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Help ")
                .border_style(Style::new().fg(ACCENT)),
        ),
        area,
    );
}

fn draw_prompt(f: &mut Frame, kind: PromptKind, buf: &str) {
    let title = match kind {
        PromptKind::Text => " Type text on the TV ",
        PromptKind::Url => " Cast a URL (http/https) ",
        PromptKind::YouTube => " YouTube URL or video id ",
    };
    let area = popup(f.area(), 70, 3);
    f.render_widget(Clear, area);
    let w = area.width.saturating_sub(4) as usize;
    let shown: String = {
        let n = buf.chars().count();
        buf.chars().skip(n.saturating_sub(w)).collect()
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(shown),
            Span::styled("█", Style::new().fg(ACCENT)),
        ]))
        .block(
            Block::bordered()
                .title(title)
                .border_style(Style::new().fg(ACCENT)),
        ),
        area,
    );
}

fn draw_confirm(f: &mut Frame, c: ConfirmKind) {
    let (title, text) = match c {
        ConfirmKind::NetTest => (
            " Network test ",
            "This plays a noise test pattern on the TV for about 10 seconds to measure how fast \
             the TV can pull data from this PC. Anything playing on the TV is interrupted.\n\n\
             The first run also encodes a 150 Mbps probe clip (~375 MB, about a minute).\n\n\
             Press y to start, any other key to cancel.",
        ),
        ConfirmKind::QuitWhileCasting => (
            " Quit? ",
            "A cast is running from this program; quitting stops it.\n\nPress y to stop and quit, any other key to stay.",
        ),
    };
    let area = popup(f.area(), 66, 11);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(Text::raw(text))
            .wrap(Wrap { trim: true })
            .block(
                Block::bordered()
                    .title(title)
                    .border_style(Style::new().fg(Color::Yellow)),
            ),
        area,
    );
}

/// Two status dots: LAN (power from REST polling) and SmartThings.
fn tv_dots(app: &App, id: &str, has_cloud: bool) -> Vec<Span<'static>> {
    let (lan, lc) = match app.tv_power.get(id) {
        Some(PowerState::On) => ("LAN on", Color::Green),
        Some(PowerState::Standby) => ("LAN standby", Color::Yellow),
        Some(PowerState::Off) => ("LAN off", Color::Red),
        Some(PowerState::Unknown(_)) => ("LAN ?", Color::Yellow),
        None => ("LAN …", Color::DarkGray),
    };
    let (cl, cc) = match (has_cloud, app.tv_cloud.get(id)) {
        (false, _) => ("cloud –", Color::DarkGray),
        (_, Some(CloudState::Online)) => ("cloud online", Color::Green),
        (_, Some(CloudState::Offline)) => ("cloud offline", Color::Red),
        (_, Some(CloudState::Unknown(_))) => ("cloud ?", Color::Yellow),
        (_, Some(CloudState::NotLinked)) => ("cloud –", Color::DarkGray),
        (_, None) => ("cloud …", Color::DarkGray),
    };
    vec![
        Span::styled(format!("● {lan:<12}"), Style::new().fg(lc)),
        Span::styled(format!("● {cl}"), Style::new().fg(cc)),
    ]
}

fn draw_picker(f: &mut Frame, app: &mut App) {
    let rows = app.picker_rows();
    let current = app.tv.id();
    let st = app.fleet.has_smartthings();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| match r {
            PickerRow::Known(e) => {
                let mark = if e.id == current { "▶ " } else { "  " };
                let mut spans = vec![
                    Span::raw(mark),
                    Span::styled(format!("{:<22}", ellipsize(&e.display_name(), 22)), Style::new().bold()),
                    Span::styled(format!("{:<16}", if e.host.is_empty() { "cloud only".into() } else { e.host.clone() }), DIM),
                ];
                spans.extend(tv_dots(app, &e.id, st && e.smartthings_device_id.is_some()));
                ListItem::new(Line::from(spans))
            }
            PickerRow::New(c) => ListItem::new(Line::from(vec![
                Span::styled("+ ", Style::new().fg(Color::Green)),
                Span::raw(format!("{:<22}", ellipsize(&c.name, 22))),
                Span::styled(format!("{:<16}", c.ip().unwrap_or("–")), DIM),
                Span::styled(format!("new · {}", c.presence.label()), Style::new().fg(Color::Green)),
            ])),
        })
        .collect();
    let h = (items.len() as u16 + 4).clamp(6, 20);
    let area = popup(f.area(), 76, h);
    f.render_widget(Clear, area);
    let title = if app.scanning { " TVs (scanning…) " } else { " TVs " };
    let block = Block::bordered()
        .title(title)
        .title_bottom(Line::styled(" Enter: switch / add   s: scan   Esc: close ", DIM))
        .border_style(Style::new().fg(ACCENT));
    if items.is_empty() {
        f.render_widget(Paragraph::new("No TV yet: press s to scan.").block(block), area);
        return;
    }
    f.render_stateful_widget(
        List::new(items).block(block).highlight_style(sel_style()),
        area,
        &mut app.picker_state,
    );
}

fn draw_net_test(f: &mut Frame, stage: &str, ratio: f64) {
    let area = popup(f.area(), 66, 4);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Network test running (Esc: cancel) ")
        .border_style(Style::new().fg(ACCENT));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [a, b] = Layout::vertical([Constraint::Length(1); 2]).areas(inner);
    f.render_widget(Line::raw(stage.to_string()), a);
    f.render_widget(
        Gauge::default()
            .ratio(ratio.clamp(0.0, 1.0))
            .gauge_style(Style::new().fg(ACCENT)),
        b,
    );
}
