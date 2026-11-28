//! The egui desktop app: paste a link, preview it, pick what to download,
//! and watch the queue. Styled after torrenter: monospace text, dark
//! palettes, custom-painted progress bars with a sliding "busy" segment.
//!
//! The UI thread never blocks on the network: tool installs, video info and
//! thumbnails run on a worker thread with its own Tokio runtime, and
//! downloads run inside [`Session`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, FontFamily, FontId, RichText, Sense, Stroke, TextEdit, Vec2};

use crate::config;
use crate::cookies::{self, Browser, CookieSource};
use crate::progress::{self, format_eta, format_size, format_speed, JobSnapshot, ProgressHandle};
use crate::request::{clean_name, default_name, DownloadKind, DownloadRequest};
use crate::session::{Hydrate, Job, Session, SessionEvent};
use crate::slots::{DEFAULT_MAX_DOWNLOADS, MAX_DOWNLOADS_CAP};
use crate::tools::{self, Tool, ToolPaths};
use crate::trim::TrimRange;
use crate::update::{self, AppKind, AvailableUpdate};
use crate::ytdlp::{self, VideoInfo};

const WINDOW_INNER: [f32; 2] = [980.0, 680.0];
const WINDOW_MIN: [f32; 2] = [720.0, 480.0];
const INDETERMINATE_SPAN: f32 = 0.28;
const INDETERMINATE_HZ: f32 = 0.4;
const SLIDER_REPAINT: Duration = Duration::from_millis(16);
const THUMB: Vec2 = Vec2::new(208.0, 117.0);
const THUMB_MAX_BYTES: usize = 8 * 1024 * 1024;
/// "restart now": how long to wait for paused downloads to stop, then for
/// settings writes to finish (the same limits as quitting).
const RESTART_JOBS_WAIT: Duration = Duration::from_secs(5);
const RESTART_SAVES_WAIT: Duration = Duration::from_secs(2);

/// A "restart now" in progress.
struct Restart {
    /// Jobs this restart paused, resumed if the relaunch fails.
    paused: Vec<u64>,
    jobs_until: Instant,
}

/// Start of the loader toast while a ytd update downloads.
const INSTALLING_UPDATE: &str = "installing ytd ";
/// Toast shown while "update tools" asks GitHub what is out of date.
const CHECKING_TOOLS: &str = "checking tools…";
/// Header rows, as in torrenter: the URL box and the download-folder row.
const URL_H: f32 = 28.0;
const DEST_H: f32 = 22.0;
const GUTTER: f32 = 12.0;
/// Room the × glyph takes at the right end of the URL box (beyond the usual
/// 8 px text margin), so typed text stops short of it.
const CLEAR_GLYPH_ROOM: f32 = 16.0;
/// Frames to retry installing the macOS window glass before giving up.
const GLASS_ATTEMPTS: u32 = 120;

fn viewport() -> egui::ViewportBuilder {
    let viewport = egui::ViewportBuilder::default()
        .with_app_id("ytd")
        .with_title(crate::banner())
        .with_inner_size(WINDOW_INNER)
        .with_min_inner_size(WINDOW_MIN)
        .with_icon(Arc::new(crate::icon::app_icon()));
    // The window glass shows through a transparent surface (macOS only).
    #[cfg(target_os = "macos")]
    let viewport = viewport.with_transparent(true);
    viewport
}

pub fn run() -> eframe::Result<()> {
    update::cleanup_replaced_binary();
    let options = eframe::NativeOptions {
        viewport: viewport(),
        // Linux compositors can miss frames with vsync on (as in torrenter).
        vsync: !cfg!(target_os = "linux"),
        ..Default::default()
    };
    eframe::run_native("ytd", options, Box::new(|cc| Ok(Box::new(GuiApp::new(cc)))))
}

// ---------------------------------------------------------------- themes

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThemeId {
    Scarlet,
    Phosphor,
    Amber,
    Ice,
    Violet,
}

impl ThemeId {
    const ALL: [ThemeId; 5] = [
        ThemeId::Scarlet,
        ThemeId::Phosphor,
        ThemeId::Amber,
        ThemeId::Ice,
        ThemeId::Violet,
    ];

    fn from_config(name: Option<&str>) -> Self {
        match name.unwrap_or("") {
            "phosphor" => Self::Phosphor,
            "amber" => Self::Amber,
            "ice" => Self::Ice,
            "violet" => Self::Violet,
            _ => Self::Scarlet,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Scarlet => "scarlet",
            Self::Phosphor => "phosphor",
            Self::Amber => "amber",
            Self::Ice => "ice",
            Self::Violet => "violet",
        }
    }
}

#[derive(Clone, Copy)]
struct Theme {
    bg: Color32,
    row: Color32,
    row_alt: Color32,
    fg: Color32,
    dim: Color32,
    fail: Color32,
}

fn theme(id: ThemeId) -> Theme {
    let rgb = Color32::from_rgb;
    match id {
        ThemeId::Scarlet => Theme {
            bg: rgb(10, 6, 8),
            row: rgb(20, 10, 14),
            row_alt: rgb(28, 14, 18),
            fg: rgb(255, 88, 102),
            dim: rgb(176, 52, 64),
            fail: rgb(255, 168, 72),
        },
        ThemeId::Phosphor => Theme {
            bg: rgb(10, 10, 10),
            row: rgb(16, 16, 16),
            row_alt: rgb(22, 22, 22),
            fg: rgb(110, 255, 110),
            dim: rgb(62, 140, 62),
            fail: rgb(255, 90, 90),
        },
        ThemeId::Amber => Theme {
            bg: rgb(12, 9, 4),
            row: rgb(22, 16, 6),
            row_alt: rgb(30, 22, 8),
            fg: rgb(255, 184, 72),
            dim: rgb(168, 112, 32),
            fail: rgb(255, 96, 64),
        },
        ThemeId::Ice => Theme {
            bg: rgb(6, 12, 18),
            row: rgb(10, 20, 30),
            row_alt: rgb(14, 28, 40),
            fg: rgb(140, 220, 255),
            dim: rgb(70, 120, 150),
            fail: rgb(255, 120, 140),
        },
        ThemeId::Violet => Theme {
            bg: rgb(10, 8, 14),
            row: rgb(18, 14, 26),
            row_alt: rgb(26, 20, 36),
            fg: rgb(210, 186, 255),
            dim: rgb(148, 124, 196),
            fail: rgb(255, 140, 96),
        },
    }
}

fn mix_rgb(a: Color32, b: Color32, toward_b: f32) -> Color32 {
    let t = toward_b.clamp(0.0, 1.0);
    let blend = |x: u8, y: u8| ((x as f32) * (1.0 - t) + (y as f32) * t).round() as u8;
    Color32::from_rgb(
        blend(a.r(), b.r()),
        blend(a.g(), b.g()),
        blend(a.b(), b.b()),
    )
}

/// Startup tint: clamped, and 100% on Windows (no frost there; torrenter).
/// Only in memory: the tint is not painted off macOS, so config.json is
/// left alone.
fn initial_glass_tint(saved: u8) -> u8 {
    let tint = config::normalize_glass_tint(saved);
    if cfg!(windows) {
        config::windows_glass_tint(tint)
    } else {
        tint
    }
}

/// `color` at the glass alpha (macOS), or as is.
fn surface(color: Color32, tint: Option<u8>) -> Color32 {
    match tint {
        #[cfg(target_os = "macos")]
        Some(alpha) => crate::glass::tint(color, alpha),
        _ => color,
    }
}

/// Cards, toasts and rows: nearly opaque, so text stays readable over the
/// glass without a wash (torrenter's `card_fill`).
fn card_fill(color: Color32) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 236)
}

fn apply_theme(ctx: &egui::Context, palette: Theme, tint: Option<u8>) {
    let mut visuals = egui::Visuals::dark();
    let glass = |color| surface(color, tint);
    visuals.override_text_color = Some(palette.fg);
    visuals.panel_fill = glass(palette.bg);
    // Tooltips and popups draw with window_fill: keep it solid so they stay
    // readable over the glass. The panels carry the glass.
    visuals.window_fill = palette.bg;
    visuals.extreme_bg_color = glass(palette.row);
    visuals.faint_bg_color = glass(palette.row_alt);
    visuals.widgets.inactive.bg_fill = glass(palette.row);
    visuals.widgets.inactive.weak_bg_fill = glass(palette.row);
    visuals.widgets.hovered.bg_fill = glass(palette.row_alt);
    visuals.widgets.hovered.weak_bg_fill = glass(palette.row_alt);
    visuals.widgets.active.bg_fill = glass(palette.row_alt);
    visuals.widgets.active.weak_bg_fill = glass(palette.row_alt);
    visuals.widgets.open.bg_fill = glass(palette.row_alt);
    visuals.selection.bg_fill =
        Color32::from_rgba_unmultiplied(palette.fg.r(), palette.fg.g(), palette.fg.b(), 40);
    visuals.selection.stroke = Stroke::new(1.0_f32, palette.fg);
    let fg = Stroke::new(1.0_f32, palette.fg);
    let dim = Stroke::new(1.0_f32, palette.dim);
    visuals.widgets.noninteractive.fg_stroke = dim;
    visuals.widgets.noninteractive.bg_stroke = dim;
    visuals.widgets.inactive.fg_stroke = dim;
    visuals.widgets.hovered.fg_stroke = fg;
    visuals.widgets.hovered.bg_stroke = dim;
    visuals.widgets.active.fg_stroke = fg;
    visuals.widgets.open.fg_stroke = fg;
    visuals.window_stroke = dim;
    visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);
    ctx.set_visuals(visuals);
    ctx.style_mut(|style| {
        for font in style.text_styles.values_mut() {
            font.family = FontFamily::Monospace;
        }
        style.spacing.item_spacing = Vec2::new(10.0, 8.0);
        style.spacing.button_padding = Vec2::new(8.0, 3.0);
    });
}

// ------------------------------------------------------- progress loaders

/// `None` draws the sliding indeterminate segment.
fn bar_fraction(status: &str, downloaded: u64, total: u64) -> Option<f32> {
    // No size yet (a trim, or the moment before the first progress line):
    // slide instead of sitting at 0%.
    if progress::is_indeterminate(status) || total == 0 {
        return None;
    }
    Some((downloaded as f32 / total as f32).clamp(0.0, 1.0))
}

fn snapshot_fraction(snapshot: &JobSnapshot) -> Option<f32> {
    bar_fraction(
        &snapshot.status(),
        snapshot.downloaded.load(Ordering::Relaxed),
        snapshot.total.load(Ordering::Relaxed),
    )
    .map(|fraction| snapshot.steady_fraction(fraction))
}

/// `12 MB / 1.1 GB`, with `~` when the total is yt-dlp's estimate.
fn size_text(snapshot: &JobSnapshot) -> Option<String> {
    let downloaded = snapshot.downloaded.load(Ordering::Relaxed);
    let total = snapshot.total.load(Ordering::Relaxed);
    if total > 0 {
        let approx = if snapshot.estimated.load(Ordering::Relaxed) {
            "~"
        } else {
            ""
        };
        Some(format!(
            "{} / {approx}{}",
            format_size(downloaded),
            format_size(total)
        ))
    } else if downloaded > 0 {
        Some(format_size(downloaded))
    } else {
        None
    }
}

/// A lean single-stroke tick (no box).
fn tick(ui: &mut egui::Ui, color: Color32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(20.0, 26.0), Sense::hover());
    let c = rect.center();
    ui.painter().add(egui::Shape::line(
        vec![
            c + egui::vec2(-5.0, 0.5),
            c + egui::vec2(-1.5, 4.0),
            c + egui::vec2(5.5, -4.0),
        ],
        Stroke::new(1.4_f32, color),
    ));
    response
}

/// torrenter's settings cog: a ring with six teeth, lit when open or hovered.
fn settings_cog(ui: &mut egui::Ui, palette: Theme, active: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), Sense::click());
    let color = if active || response.hovered() {
        palette.fg
    } else {
        palette.dim
    };
    let painter = ui.painter();
    paint_brush_stroke(painter, rect.shrink(1.5));
    let stroke = Stroke::new(1.3_f32, color);
    let c = rect.center();
    painter.circle_stroke(c, 4.8, stroke);
    painter.circle_filled(c, 1.6, color);
    for step in 0..6 {
        let rad = (step as f32 * 60.0).to_radians();
        let dir = egui::vec2(rad.cos(), rad.sin());
        painter.line_segment([c + dir * 6.2, c + dir * 8.4], stroke);
    }
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("settings")
}

/// torrenter's labelled switch: dim label, 32x16 pill, knob right when on.
fn labeled_switch(ui: &mut egui::Ui, label: &str, on: bool, palette: Theme) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        label.into(),
        egui::FontId::new(12.0, FontFamily::Monospace),
        palette.dim,
    );
    let (track_w, track_h, gap) = (32.0_f32, 16.0_f32, 12.0_f32);
    let size = egui::vec2(galley.size().x + gap + track_w, 26.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let painter = ui.painter();
    let text_pos = egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0);
    painter.galley(text_pos, galley, palette.dim);
    let track = egui::Rect::from_center_size(
        egui::pos2(rect.right() - track_w / 2.0, rect.center().y),
        egui::vec2(track_w, track_h),
    );
    let rounding = track_h / 2.0;
    if on {
        painter.rect_filled(track, rounding, palette.fg);
    }
    painter.rect_stroke(
        track,
        rounding,
        Stroke::new(1.0_f32, if on { palette.fg } else { palette.dim }),
        egui::StrokeKind::Inside,
    );
    let knob_r = 5.0_f32;
    let knob_x = if on {
        track.right() - 2.5 - knob_r
    } else {
        track.left() + 2.5 + knob_r
    };
    painter.circle_filled(
        egui::pos2(knob_x, track.center().y),
        knob_r,
        if on { palette.bg } else { palette.dim },
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// torrenter's theme picker: a 16 px swatch of the theme colour, outlined
/// when selected, named on hover.
fn theme_swatch(ui: &mut egui::Ui, fill: Color32, selected: bool, name: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), Sense::click());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, fill);
    if selected {
        painter.rect_stroke(
            rect.expand(2.0),
            2.0,
            Stroke::new(1.2_f32, fill),
            egui::StrokeKind::Outside,
        );
    }
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(format!("theme: {name}"))
}

fn indeterminate_offset(time: f32, span: f32) -> f32 {
    let travel = (1.0 - span).max(0.0);
    let cycle = (time * INDETERMINATE_HZ).rem_euclid(1.0);
    let ping = 0.5 - 0.5 * (cycle * std::f32::consts::TAU).cos();
    ping * travel
}

/// torrenter's 8 px bar: a filled share, or a segment that slides back and
/// forth while the total is unknown.
fn paint_progress(
    ui: &mut egui::Ui,
    accent: Color32,
    track: Color32,
    fraction: Option<f32>,
    width: Option<f32>,
) {
    let track_w = width.unwrap_or_else(|| ui.available_width()).max(1.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(track_w, 8.0), Sense::hover());
    ui.painter().rect_filled(rect, 2.0, track);
    let fill = if let Some(fraction) = fraction {
        let mut fill = rect;
        fill.set_width(rect.width() * fraction);
        fill
    } else {
        ui.ctx().request_repaint_after(SLIDER_REPAINT);
        let t = ui.input(|input| input.time) as f32;
        let span = (32.0 / rect.width()).clamp(0.2, INDETERMINATE_SPAN);
        let mut fill = rect;
        fill.min.x += rect.width() * indeterminate_offset(t, span);
        fill.set_width(rect.width() * span);
        fill
    };
    if fill.width() > 0.5 {
        ui.painter().rect_filled(fill, 2.0, accent);
    }
}

/// Running rows are dimmer; finished rows light up.
fn row_accent(palette: Theme, settled: bool) -> Color32 {
    if settled {
        palette.fg
    } else {
        mix_rgb(palette.fg, palette.dim, 0.65)
    }
}

// ---------------------------------------------------------------- worker

/// One change to `config.json`, run on the worker.
type ConfigEdit = Box<dyn FnOnce(&mut config::AppConfig) + Send>;

enum WorkerCmd {
    /// Install what is missing, or (`update: true`) re-install exactly the
    /// tools listed in `progress`.
    Tools {
        update: bool,
        progress: Vec<(Tool, Arc<JobSnapshot>)>,
    },
    Fetch {
        seq: u64,
        url: String,
        tools: ToolPaths,
    },
    Thumb {
        seq: u64,
        url: String,
    },
    /// Auto-update: look for a newer ytd release and outdated tools.
    CheckUpdates,
    /// "update tools" was pressed: which tools need installing?
    CheckTools,
    InstallApp {
        update: AvailableUpdate,
        snapshot: Arc<JobSnapshot>,
    },
    /// Try a YouTube sign-in source on `url` before saving it.
    TestCookies {
        source: CookieSource,
        url: String,
        tools: ToolPaths,
    },
    /// Change `config.json` off the GUI thread; `pending` drops by one when
    /// the write is done.
    EditConfig {
        change: ConfigEdit,
        pending: Arc<AtomicUsize>,
    },
}

enum WorkerEvent {
    ToolsDone(Result<ToolPaths, String>),
    /// The tools "update tools" should install; empty when all are current.
    ToolsChecked(Result<Vec<Tool>, String>),
    UpdatesChecked {
        app: Result<Option<AvailableUpdate>, String>,
        tools: Vec<Tool>,
        /// This install can replace its own binary (else only announce).
        can_install: bool,
    },
    AppInstalled(Result<String, String>),
    CookiesTested {
        source: CookieSource,
        result: Result<(), String>,
    },
    /// A `config.json` write from the worker finished.
    ConfigSaved(Result<(), String>),
    Info {
        seq: u64,
        /// The URL this info is for: the text box may have changed since.
        url: String,
        result: Result<VideoInfo, String>,
    },
    Thumb {
        seq: u64,
        result: Result<egui::ColorImage, String>,
    },
}

fn worker_loop(commands: Receiver<WorkerCmd>, events: Sender<WorkerEvent>, ctx: egui::Context) {
    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    else {
        return;
    };
    while let Ok(command) = commands.recv() {
        // Config writes run here, one at a time in the order they were sent:
        // two quick changes to one setting must not land out of order.
        let command = match command {
            WorkerCmd::EditConfig { change, pending } => {
                let result = config::edit_config(change)
                    .map(|_| ())
                    .map_err(|e| format!("{e:#}"));
                pending.fetch_sub(1, Ordering::SeqCst);
                let _ = events.send(WorkerEvent::ConfigSaved(result));
                ctx.request_repaint();
                continue;
            }
            other => other,
        };
        let events = events.clone();
        let ctx = ctx.clone();
        runtime.spawn(async move {
            let event = match command {
                WorkerCmd::Tools { update, progress } => {
                    let handle_for = |tool: Tool| {
                        progress
                            .iter()
                            .find(|(t, _)| *t == tool)
                            .map(|(_, snap)| ProgressHandle::gui(Arc::clone(snap)))
                            .unwrap_or(ProgressHandle::Hidden)
                    };
                    let result = if update {
                        let wanted: Vec<Tool> = progress.iter().map(|(t, _)| *t).collect();
                        tools::update_tools(&wanted, handle_for).await
                    } else {
                        tools::install_missing(handle_for).await
                    };
                    WorkerEvent::ToolsDone(result.map_err(|e| format!("{e:#}")))
                }
                WorkerCmd::Fetch { seq, url, tools } => {
                    let result = ytdlp::fetch_info(&tools, &url)
                        .await
                        .map_err(|e| format!("{e:#}"));
                    WorkerEvent::Info { seq, url, result }
                }
                WorkerCmd::Thumb { seq, url } => WorkerEvent::Thumb {
                    seq,
                    result: fetch_thumbnail(&url).await.map_err(|e| format!("{e:#}")),
                },
                WorkerCmd::CheckUpdates => WorkerEvent::UpdatesChecked {
                    app: update::check(AppKind::Gui)
                        .await
                        .map_err(|e| format!("{e:#}")),
                    // A failed tool check is not worth a toast: retried next launch.
                    tools: tools::outdated().await.unwrap_or_default(),
                    can_install: update::can_self_replace(),
                },
                WorkerCmd::CheckTools => WorkerEvent::ToolsChecked(
                    tools::needing_update().await.map_err(|e| format!("{e:#}")),
                ),
                WorkerCmd::TestCookies { source, url, tools } => {
                    let result = ytdlp::test_cookies(&tools, &source, &url).await;
                    WorkerEvent::CookiesTested { source, result }
                }
                // Handled in order before spawning, above.
                WorkerCmd::EditConfig { .. } => return,
                WorkerCmd::InstallApp {
                    update: available,
                    snapshot,
                } => {
                    let progress = ProgressHandle::gui(Arc::clone(&snapshot));
                    let result = update::install(&available, AppKind::Gui, &progress).await;
                    snapshot.mark_finished();
                    WorkerEvent::AppInstalled(
                        result
                            .map(|()| available.tag.clone())
                            .map_err(|e| format!("{e:#}")),
                    )
                }
            };
            let _ = events.send(event);
            ctx.request_repaint();
        });
    }
}

async fn fetch_thumbnail(url: &str) -> anyhow::Result<egui::ColorImage> {
    let client = tools::http_client()?;
    let response = client.get(url).send().await?.error_for_status()?;
    let bytes = response.bytes().await?;
    if bytes.len() > THUMB_MAX_BYTES {
        anyhow::bail!("thumbnail too large");
    }
    let image = tokio::task::spawn_blocking(move || image::load_from_memory(&bytes))
        .await??
        .to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        size,
        image.as_raw(),
    ))
}

// ----------------------------------------------------------------- state

enum ToolsState {
    Checking,
    Installing,
    Ready,
    Failed(String),
}

struct Draft {
    url: String,
    info: VideoInfo,
    thumb: Option<egui::TextureHandle>,
    thumb_state: ThumbState,
    kind: DownloadKind,
    video: Option<String>,
    audio: Option<String>,
    name: String,
    trim_start: String,
    trim_end: String,
}

#[derive(PartialEq)]
enum ThumbState {
    Loading,
    Loaded,
    Missing,
}

impl Draft {
    fn new(url: String, info: VideoInfo) -> Self {
        let name = default_name(&info.title);
        let mut draft = Self {
            url,
            thumb: None,
            thumb_state: if info.thumbnail_url().is_some() {
                ThumbState::Loading
            } else {
                ThumbState::Missing
            },
            info,
            kind: DownloadKind::Merged,
            video: None,
            audio: None,
            name,
            trim_start: String::new(),
            trim_end: String::new(),
        };
        draft.reset_formats();
        draft
    }

    fn reset_formats(&mut self) {
        self.video = self
            .info
            .video_choices(self.kind)
            .first()
            .map(|f| f.format_id.clone());
        self.audio = self
            .info
            .audio_choices(self.kind)
            .first()
            .map(|f| f.format_id.clone());
    }

    fn ext(&self) -> Option<String> {
        match self.kind {
            DownloadKind::Merged => Some("mp4".into()),
            DownloadKind::Audio => Some("mp3".into()),
            _ => self
                .video
                .as_deref()
                .and_then(|id| self.info.format(id))
                .map(|f| f.ext.clone()),
        }
    }

    /// The request, or why it cannot be queued yet.
    fn request(&self, dir: &Path) -> Result<DownloadRequest, String> {
        let trim = TrimRange::parse(&self.trim_start, &self.trim_end).map_err(|e| e.to_string())?;
        let video = if self.kind == DownloadKind::Audio {
            None
        } else {
            Some(
                self.video
                    .clone()
                    .ok_or("no video format for this choice")?,
            )
        };
        let audio = if self.kind.needs_audio() {
            Some(
                self.audio
                    .clone()
                    .ok_or("no audio format for this choice")?,
            )
        } else {
            None
        };
        let expected_size = [video.as_deref(), audio.as_deref()]
            .into_iter()
            .flatten()
            .filter_map(|id| self.info.format(id).and_then(|f| f.size()))
            .sum();
        Ok(DownloadRequest {
            url: self.url.clone(),
            title: self.info.title.clone(),
            kind: self.kind,
            video_format: video,
            audio_format: audio,
            name: clean_name(&self.name, self.ext().as_deref()),
            dir: dir.to_path_buf(),
            trim_start: trim.start,
            trim_end: trim.end,
            expected_size,
        })
    }
}

enum Fetch {
    Idle,
    Loading,
    Failed(String),
    /// YouTube answered "confirm you're not a bot" for `url`.
    SignIn {
        url: String,
    },
    Ready(Box<Draft>),
}

struct Toast {
    id: u64,
    text: String,
    fail: bool,
    expires_at: Option<Instant>,
    progress: Option<Arc<JobSnapshot>>,
}

enum DraftAction {
    Queue(DownloadRequest),
    Dismiss,
}

enum RowAction {
    TogglePause(u64),
    Stop(u64),
    Restart(u64),
    Remove(u64),
    Open(PathBuf),
}

pub struct GuiApp {
    theme: ThemeId,
    session: Session,
    tx: Sender<WorkerCmd>,
    events: Receiver<WorkerEvent>,
    tools_state: ToolsState,
    url: String,
    fetch_seq: u64,
    fetch: Fetch,
    toasts: Vec<Toast>,
    toast_seq: u64,
    show_settings: bool,
    download_dir: PathBuf,
    max_downloads: u8,
    auto_update: bool,
    /// The auto-update check already ran this session.
    checked_updates: bool,
    /// Outdated tools wait here until no download is running.
    tool_update_pending: Vec<Tool>,
    /// Browsers installed here (for the sign-in choices).
    browsers: Vec<Browser>,
    /// A sign-in source being tested: what to say while it runs.
    cookie_test: Option<String>,
    cookie_error: Option<String>,
    /// URL to fetch again once a sign-in source works.
    cookie_retry: Option<String>,
    /// Filters the queue; not saved.
    job_query: String,
    settings_tab: SettingsTab,
    /// Tool versions and paths for the updates tab: read when settings
    /// opens or the tools change, never on every frame.
    tool_rows: Option<Vec<ToolRow>>,
    /// "update tools" is asking GitHub which tools are out of date.
    tools_checking: bool,
    /// A newer ytd was installed in place: the restart banner shows it.
    installed_tag: Option<String>,
    /// **later** hides the banner; the update stays installed.
    banner_hidden: bool,
    /// An app update is downloading (only one at a time).
    app_installing: bool,
    /// **restart now** is pausing downloads and saving before relaunch.
    restart: Option<Restart>,
    /// Bumped when a tool install finishes or "update tools" gets its
    /// answer; an auto-update result asked for before that is stale.
    tools_generation: u64,
    /// `tools_generation` when the last auto-update check was sent.
    updates_asked_at: u64,
    /// macOS window glass (torrenter): saved tint, and what is painted now.
    glass_tint: u8,
    glass_applied: bool,
    glass_attempts: u32,
    viewport_fullscreen: bool,
    applied_style: Option<(ThemeId, Option<u8>)>,
    /// `config.json` writes sent to the worker and not done yet: quitting
    /// waits for them.
    pending_saves: Arc<AtomicUsize>,
}

/// One line of the updates tab's tool list.
struct ToolRow {
    key: &'static str,
    version: String,
    location: String,
}

/// Settings categories, listed in the sidebar (torrenter).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SettingsTab {
    #[default]
    Appearance,
    Downloads,
    SignIn,
    Updates,
}

impl SettingsTab {
    const ALL: [SettingsTab; 4] = [
        SettingsTab::Appearance,
        SettingsTab::Downloads,
        SettingsTab::SignIn,
        SettingsTab::Updates,
    ];

    fn label(self) -> &'static str {
        match self {
            SettingsTab::Appearance => "appearance",
            SettingsTab::Downloads => "downloads",
            SettingsTab::SignIn => "YouTube sign-in",
            SettingsTab::Updates => "updates",
        }
    }
}

impl GuiApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, commands) = mpsc::channel();
        let (events_tx, events) = mpsc::channel();
        let ctx = cc.egui_ctx.clone();
        std::thread::Builder::new()
            .name("ytd-worker".into())
            .spawn(move || worker_loop(commands, events_tx, ctx))
            .expect("failed to start worker thread");

        let cfg = config::load_config();
        let theme_id = ThemeId::from_config(cfg.theme.as_deref());
        let max_downloads = cfg.max_downloads.unwrap_or(DEFAULT_MAX_DOWNLOADS);
        let session = Session::open(config::jobs_path(), max_downloads);
        let mut app = Self {
            theme: theme_id,
            session,
            tx,
            events,
            tools_state: ToolsState::Checking,
            url: String::new(),
            fetch_seq: 0,
            fetch: Fetch::Idle,
            toasts: Vec::new(),
            toast_seq: 0,
            show_settings: false,
            download_dir: cfg.download_dir(),
            max_downloads: crate::slots::clamp_max_downloads(max_downloads),
            auto_update: cfg.auto_update,
            checked_updates: false,
            tool_update_pending: Vec::new(),
            browsers: cookies::detect(),
            cookie_test: None,
            cookie_error: None,
            cookie_retry: None,
            job_query: String::new(),
            settings_tab: SettingsTab::default(),
            tool_rows: None,
            tools_checking: false,
            installed_tag: None,
            banner_hidden: false,
            app_installing: false,
            restart: None,
            tools_generation: 0,
            updates_asked_at: 0,
            glass_tint: initial_glass_tint(cfg.glass_tint),
            glass_applied: false,
            glass_attempts: 0,
            viewport_fullscreen: false,
            applied_style: None,
            pending_saves: Arc::new(AtomicUsize::new(0)),
        };
        if let Hydrate::Corrupt(error) = app.session.hydrate() {
            app.toast_fail(format!("saved list was unreadable and was reset: {error}"));
        }
        app.setup_tools(None);
        app
    }

    fn palette(&self) -> Theme {
        theme(self.theme)
    }

    /// The alpha the window glass paints surfaces with: macOS only, 100% in
    /// fullscreen. `None` elsewhere (opaque).
    fn paint_tint(&self) -> Option<u8> {
        if cfg!(target_os = "macos") && self.glass_applied {
            Some(config::effective_glass_tint(
                self.glass_tint,
                self.viewport_fullscreen,
            ))
        } else {
            None
        }
    }

    fn surface(&self, color: Color32) -> Color32 {
        surface(color, self.paint_tint())
    }

    fn open_settings(&mut self) {
        self.show_settings = true;
        self.tool_rows = None;
    }

    // ------------------------------------------------------------ toasts

    fn push_toast(
        &mut self,
        text: String,
        fail: bool,
        hold: Option<Duration>,
        progress: Option<Arc<JobSnapshot>>,
    ) {
        self.toast_seq += 1;
        self.toasts.push(Toast {
            id: self.toast_seq,
            text,
            fail,
            expires_at: hold.map(|d| Instant::now() + d),
            progress,
        });
    }

    fn toast(&mut self, text: impl Into<String>) {
        self.push_toast(text.into(), false, Some(Duration::from_secs(4)), None);
    }

    fn job_url(&self, id: u64) -> Option<String> {
        self.session
            .jobs()
            .into_iter()
            .find(|job| job.id == id)
            .map(|job| job.request.url)
    }

    fn toast_fail(&mut self, text: impl Into<String>) {
        self.push_toast(text.into(), true, Some(Duration::from_secs(8)), None);
    }

    // ------------------------------------------------------------- tools

    /// Install missing tools, or re-install `update`'s tools, with one
    /// loader toast per tool.
    fn setup_tools(&mut self, update: Option<Vec<Tool>>) {
        let is_update = update.is_some();
        let wanted: Vec<Tool> = match update {
            Some(list) => list,
            None => tools::missing()
                .into_iter()
                .filter(|tool| tools::can_install(*tool))
                .collect(),
        };
        if wanted.is_empty() {
            match tools::resolve() {
                Ok(paths) => self.tools_ready(paths, false),
                Err(error) => self.tools_state = ToolsState::Failed(format!("{error:#}")),
            }
            return;
        }
        let progress: Vec<(Tool, Arc<JobSnapshot>)> = wanted
            .iter()
            .map(|tool| (*tool, Arc::new(JobSnapshot::default())))
            .collect();
        let verb = if is_update { "updating" } else { "installing" };
        for (tool, snap) in &progress {
            snap.set_status(progress::STARTING);
            self.push_toast(
                format!("{verb} {}…", tool.key()),
                false,
                None,
                Some(Arc::clone(snap)),
            );
        }
        self.tools_state = ToolsState::Installing;
        let _ = self.tx.send(WorkerCmd::Tools {
            update: is_update,
            progress,
        });
    }

    fn tools_ready(&mut self, paths: ToolPaths, announce: bool) {
        if paths.deno.is_none() {
            self.toast_fail("deno not found: some YouTube formats may be missing");
        }
        self.session.set_tools(paths);
        self.tools_state = ToolsState::Ready;
        if announce {
            self.toast("tools ready");
        }
        if self.auto_update && !self.checked_updates {
            self.check_updates();
        }
    }

    // ------------------------------------------------------- auto-update

    fn check_updates(&mut self) {
        self.checked_updates = true;
        self.updates_asked_at = self.tools_generation;
        let _ = self.tx.send(WorkerCmd::CheckUpdates);
    }

    fn set_auto_update(&mut self, on: bool) {
        self.auto_update = on;
        self.save_config(move |cfg| cfg.auto_update = on);
        if on && matches!(self.tools_state, ToolsState::Ready) {
            self.toast("checking for updates…");
            self.check_updates();
        }
    }

    fn on_updates_checked(
        &mut self,
        app: Result<Option<AvailableUpdate>, String>,
        stale: Vec<Tool>,
        can_install: bool,
    ) {
        match app {
            // Already on disk (VERSION stays old until restart): show the
            // banner again instead of installing it twice.
            Ok(Some(available))
                if self.installed_tag.as_deref() == Some(available.tag.as_str()) =>
            {
                self.banner_hidden = false;
            }
            Ok(Some(_)) if self.app_installing => {}
            // A translocated or read-only install can't replace itself.
            Ok(Some(available)) if !can_install => self.push_toast(
                format!(
                    "ytd {} is available: github.com/{}/releases",
                    available.tag,
                    update::REPO
                ),
                false,
                Some(Duration::from_secs(15)),
                None,
            ),
            // Installed in place, `.app` included (torrenter's flow): a
            // loader toast while it downloads, then the restart banner.
            Ok(Some(available)) => {
                self.app_installing = true;
                let snapshot = Arc::new(JobSnapshot::default());
                snapshot.set_status(progress::STARTING);
                self.push_toast(
                    format!("{INSTALLING_UPDATE}{}…", available.tag),
                    false,
                    None,
                    Some(Arc::clone(&snapshot)),
                );
                let _ = self.tx.send(WorkerCmd::InstallApp {
                    update: available,
                    snapshot,
                });
            }
            Ok(None) => {}
            Err(error) => self.toast_fail(format!("update check failed: {error}")),
        }
        // Drop a tools list that an install or "update tools" made stale
        // while this check ran, or the same tools would download twice.
        let current = self.updates_asked_at == self.tools_generation
            && !self.tools_checking
            && !matches!(self.tools_state, ToolsState::Installing);
        if !stale.is_empty() && current {
            self.tool_update_pending = stale;
        }
    }

    // ------------------------------------------------------- sign-in

    /// Test `source` on `url` (the video that was refused, or a short
    /// public one); it is saved only if YouTube accepts it.
    fn start_cookie_test(&mut self, source: CookieSource, url: String, retry: bool) {
        let Some(tools) = self.session.tools() else {
            self.toast_fail("tools are not ready yet");
            return;
        };
        self.cookie_error = None;
        self.cookie_test = Some(format!("checking {}…", source.usage()));
        self.cookie_retry = retry.then(|| url.clone());
        let _ = self.tx.send(WorkerCmd::TestCookies { source, url, tools });
    }

    fn apply_cookies(&mut self, source: Option<CookieSource>) {
        let saved = source.clone();
        self.save_config(move |cfg| cfg.cookies = saved);
        if let Some(mut tools) = self.session.tools() {
            tools.cookies = source;
            self.session.set_tools(tools);
        }
    }

    /// Browser buttons, "import cookies.txt…" and (in settings) "stop using".
    /// Returns the chosen source, or `Some(None)` for "stop using".
    fn sign_in_choices(
        &mut self,
        ui: &mut egui::Ui,
        settings: bool,
    ) -> Option<Option<CookieSource>> {
        let palette = self.palette();
        let current = self.session.tools().and_then(|t| t.cookies);
        let mut chosen = None;
        if let Some(testing) = &self.cookie_test {
            ui.label(RichText::new(testing).color(palette.dim));
            paint_progress(ui, palette.fg, palette.row_alt, None, None);
            return None;
        }
        ui.horizontal_wrapped(|ui| {
            if self.browsers.is_empty() {
                ui.label(RichText::new("no supported browser found").color(palette.dim));
            }
            for browser in &self.browsers {
                let selected = matches!(&current, Some(CookieSource::Browser { browser: b }) if b == browser.key);
                if ui
                    .selectable_label(selected, browser.label)
                    .on_hover_text(format!("use {}'s YouTube sign-in", browser.label))
                    .clicked()
                {
                    chosen = Some(Some(CookieSource::Browser {
                        browser: browser.key.to_string(),
                    }));
                }
            }
            let file_selected = matches!(current, Some(CookieSource::File { .. }));
            if ui
                .selectable_label(file_selected, "import cookies.txt…")
                .on_hover_text(
                    "For any browser: export youtube.com cookies in Netscape (cookies.txt) format with a cookies.txt browser extension, then pick the file.",
                )
                .clicked()
            {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("choose a cookies.txt export")
                    .add_filter("cookies.txt", &["txt"])
                    .pick_file()
                {
                    chosen = Some(Some(CookieSource::File { path }));
                }
            }
            if settings && current.is_some() && ui.button("stop using").clicked() {
                chosen = Some(None);
            }
        });
        if let Some(error) = &self.cookie_error {
            ui.add(egui::Label::new(RichText::new(error).color(palette.fail)).wrap());
        }
        chosen
    }

    // ------------------------------------------------------------- fetch

    fn start_fetch(&mut self) {
        let url = self.url.trim().to_string();
        if url.is_empty() {
            return;
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            self.fetch = Fetch::Failed("paste a full link starting with https://".into());
            return;
        }
        let Some(tools) = self.session.tools() else {
            self.toast_fail("tools are not ready yet");
            return;
        };
        self.fetch_seq += 1;
        self.fetch = Fetch::Loading;
        let _ = self.tx.send(WorkerCmd::Fetch {
            seq: self.fetch_seq,
            url,
            tools,
        });
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                WorkerEvent::ToolsDone(result) => {
                    self.tools_generation += 1;
                    self.tool_rows = None;
                    self.toasts.retain(|toast| toast.progress.is_none());
                    match result {
                        Ok(paths) => self.tools_ready(paths, true),
                        Err(error) => {
                            self.toast_fail(error.clone());
                            // An update that failed leaves the old tools in place.
                            match tools::resolve() {
                                Ok(paths) => self.tools_ready(paths, false),
                                Err(_) => self.tools_state = ToolsState::Failed(error),
                            }
                        }
                    }
                }
                WorkerEvent::ToolsChecked(result) => {
                    self.tools_checking = false;
                    self.toasts.retain(|toast| toast.text != CHECKING_TOOLS);
                    match result {
                        Ok(wanted) => {
                            // This check is newer than anything auto-update
                            // queued (or will report) from before it.
                            self.tools_generation += 1;
                            self.tool_update_pending.clear();
                            if wanted.is_empty() {
                                self.toast("tools are up to date");
                            } else if matches!(self.tools_state, ToolsState::Installing) {
                                // Two installs would write the same files.
                                self.toast("tools are already updating");
                            } else if self.session.any_running() {
                                // A download started while GitHub answered:
                                // a running yt-dlp must not be replaced.
                                self.tool_update_pending = wanted;
                                self.toast("tools will update when downloads finish");
                            } else {
                                self.setup_tools(Some(wanted));
                            }
                        }
                        Err(error) => {
                            self.toast_fail(format!("could not check for tool updates: {error}"))
                        }
                    }
                }
                WorkerEvent::Info { seq, url, result } if seq == self.fetch_seq => match result {
                    Ok(info) => {
                        let thumb = info.thumbnail_url();
                        self.fetch = Fetch::Ready(Box::new(Draft::new(url, info)));
                        if let Some(thumb) = thumb {
                            let _ = self.tx.send(WorkerCmd::Thumb { seq, url: thumb });
                        }
                    }
                    Err(error) if cookies::youtube_wants_sign_in(&url, &error) => {
                        self.cookie_error = None;
                        self.fetch = Fetch::SignIn { url };
                    }
                    Err(error) => self.fetch = Fetch::Failed(error),
                },
                WorkerEvent::Thumb { seq, result } if seq == self.fetch_seq => {
                    if let Fetch::Ready(draft) = &mut self.fetch {
                        match result {
                            Ok(image) => {
                                draft.thumb = Some(ctx.load_texture(
                                    "thumbnail",
                                    image,
                                    egui::TextureOptions::LINEAR,
                                ));
                                draft.thumb_state = ThumbState::Loaded;
                            }
                            Err(_) => draft.thumb_state = ThumbState::Missing,
                        }
                    }
                }
                WorkerEvent::UpdatesChecked {
                    app,
                    tools,
                    can_install,
                } => {
                    let asked = self
                        .toasts
                        .iter()
                        .any(|t| t.text == "checking for updates…");
                    self.toasts
                        .retain(|toast| toast.text != "checking for updates…");
                    if asked && matches!(app, Ok(None)) && tools.is_empty() {
                        self.toast("ytd and its tools are up to date");
                    }
                    self.on_updates_checked(app, tools, can_install);
                }
                WorkerEvent::AppInstalled(Ok(tag)) => {
                    self.app_installing = false;
                    self.toasts
                        .retain(|toast| !toast.text.starts_with(INSTALLING_UPDATE));
                    self.installed_tag = Some(tag);
                    self.banner_hidden = false;
                }
                WorkerEvent::CookiesTested { source, result } => {
                    self.cookie_test = None;
                    match result {
                        Ok(()) => {
                            self.cookie_error = None;
                            self.toast(format!("using {}", source.usage()));
                            self.apply_cookies(Some(source));
                            if let Some(url) = self.cookie_retry.take() {
                                self.url = url;
                                self.start_fetch();
                            }
                        }
                        Err(reason) => self.cookie_error = Some(reason),
                    }
                }
                WorkerEvent::ConfigSaved(Err(error)) => {
                    self.toast_fail(format!("could not save settings: {error}"))
                }
                WorkerEvent::ConfigSaved(Ok(())) => {}
                WorkerEvent::AppInstalled(Err(error)) => {
                    self.app_installing = false;
                    self.toasts
                        .retain(|toast| !toast.text.starts_with(INSTALLING_UPDATE));
                    self.toast_fail(format!(
                        "update failed: {error} (get it from github.com/{}/releases)",
                        update::REPO
                    ));
                }
                // A newer fetch replaced this one.
                WorkerEvent::Info { .. } | WorkerEvent::Thumb { .. } => {}
            }
        }
        for event in self.session.drain_events() {
            match event {
                SessionEvent::Finished { title, .. } => self.toast(format!("saved {title}")),
                SessionEvent::Failed { id, title, error }
                    if self.job_url(id).is_some_and(|url| cookies::youtube_wants_sign_in(&url, &error)) =>
                {
                    self.toast_fail(format!(
                        "{title}: YouTube wants a signed-in browser. Choose one in settings → YouTube sign-in, then restart the download."
                    ))
                }
                SessionEvent::Failed { title, error, .. } => {
                    self.toast_fail(format!("{title}: {error}"))
                }
            }
        }
        let now = Instant::now();
        self.toasts.retain(|toast| {
            let expired = toast.expires_at.is_some_and(|at| at <= now);
            let installed = toast
                .progress
                .as_ref()
                .is_some_and(|snap| snap.is_finished());
            !expired && !installed
        });
    }

    // ---------------------------------------------------------------- UI

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let palette = self.palette();
        ui.horizontal(|ui| {
            // The title goes back to the main page; the cog only opens
            // settings (torrenter: the header never changes).
            let title = glass_pair(
                ui,
                crate::update::REPO,
                20.0,
                palette.fg,
                format!("v{}", crate::VERSION),
                13.0,
                palette.dim,
            );
            if click_cursor(title).on_hover_text("downloads").clicked() {
                self.show_settings = false;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if settings_cog(ui, palette, self.show_settings).clicked() {
                    self.open_settings();
                }
                ui.add_space(4.0);
                if labeled_switch(ui, "auto-update", self.auto_update, palette)
                    .on_hover_text(
                        "update ytd and its tools (yt-dlp, ffmpeg, deno) from GitHub Releases",
                    )
                    .clicked()
                {
                    self.set_auto_update(!self.auto_update);
                }
                ui.add_space(8.0);
                match &self.tools_state {
                    ToolsState::Ready => {
                        tick(ui, palette.dim).on_hover_text("tools ready");
                    }
                    ToolsState::Checking => {
                        glass_label(ui, "checking tools…", 12.0, palette.dim);
                    }
                    ToolsState::Installing => {
                        glass_label(ui, "setting up tools…", 12.0, palette.dim);
                    }
                    ToolsState::Failed(_) => {
                        glass_label(ui, "tools missing", 12.0, palette.fail);
                    }
                }
            });
        });
    }

    /// torrenter's banner after an in-place update: the new binary is on
    /// disk and runs from the next start. **restart now** pauses downloads
    /// (they come back paused), saves, and starts the new version.
    fn restart_banner(&mut self, ui: &mut egui::Ui) {
        let Some(tag) = self.installed_tag.clone() else {
            return;
        };
        if self.banner_hidden {
            return;
        }
        let restarting = self.restart.is_some();
        let palette = self.palette();
        let fill =
            Color32::from_rgba_unmultiplied(palette.fg.r(), palette.fg.g(), palette.fg.b(), 18);
        egui::Frame::NONE
            .fill(fill)
            .stroke(Stroke::new(1.0_f32, palette.dim))
            .inner_margin(egui::Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let text = if restarting {
                        format!("restarting into {tag}…")
                    } else {
                        format!("{tag} is on disk  ·  restart to run it")
                    };
                    glass_label(ui, text, 13.0, palette.fg);
                    if restarting {
                        return;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(RichText::new("later").size(12.0).color(palette.dim))
                            .clicked()
                        {
                            self.banner_hidden = true;
                        }
                        if ui
                            .button(RichText::new("restart now").size(12.0).color(palette.fg))
                            .clicked()
                        {
                            self.begin_restart();
                        }
                    });
                });
            });
        ui.add_space(8.0);
    }

    /// Pause running downloads; [`Self::step_restart`] relaunches once their
    /// processes ended and the queue is saved. Nothing waits on this thread.
    fn begin_restart(&mut self) {
        self.restart = Some(Restart {
            paused: Vec::new(),
            jobs_until: Instant::now() + RESTART_JOBS_WAIT,
        });
        self.pause_for_restart();
    }

    /// Pause every active job not paused yet, remembering it so a failed
    /// relaunch can resume it. Runs every restart frame: a download queued
    /// or resumed meanwhile must not be left running when the process exits.
    fn pause_for_restart(&mut self) {
        let Some(restart) = self.restart.as_mut() else {
            return;
        };
        for job in self.session.jobs() {
            if job.snapshot.is_active() && !job.snapshot.is_paused() {
                let _ = self.session.toggle_pause(job.id);
                if !restart.paused.contains(&job.id) {
                    restart.paused.push(job.id);
                }
            }
        }
    }

    /// One frame of a restart: wait (without blocking) for paused downloads
    /// to stop and settings to finish saving, then save the queue and
    /// relaunch. If the relaunch fails, the downloads it paused resume.
    fn step_restart(&mut self) {
        self.pause_for_restart();
        let Some(restart) = &self.restart else {
            return;
        };
        let now = Instant::now();
        if self.session.any_running() && now < restart.jobs_until {
            return;
        }
        if self.pending_saves.load(Ordering::SeqCst) > 0
            && now < restart.jobs_until + RESTART_SAVES_WAIT
        {
            return;
        }
        self.session.persist(true);
        let Some(restart) = self.restart.take() else {
            return;
        };
        if let Err(error) = update::relaunch() {
            for id in restart.paused {
                let _ = self.session.toggle_pause(id);
            }
            self.toast_fail(format!("could not restart: {error:#} (downloads resumed)"));
        }
    }

    fn tools_banner(&mut self, ui: &mut egui::Ui) {
        let palette = self.palette();
        let ToolsState::Failed(error) = &self.tools_state else {
            return;
        };
        let error = error.clone();
        egui::Frame::NONE
            .fill(card_fill(palette.row))
            .stroke(Stroke::new(1.0_f32, palette.fail))
            .corner_radius(8)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.label(RichText::new(error).color(palette.fail));
                if ui.button("retry").clicked() {
                    self.setup_tools(None);
                }
            });
        ui.add_space(6.0);
    }

    /// The URL box: bordered (dim, lit on hover or focus), with a × that
    /// clears it at its right end. Enter fetches; × or Esc start over.
    fn url_row(&mut self, ui: &mut egui::Ui) {
        let ready = matches!(self.tools_state, ToolsState::Ready);
        let palette = self.palette();
        let mut clear = false;
        let mut fetch = false;
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), URL_H),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                let gap = 8.0;
                ui.spacing_mut().item_spacing.x = gap;
                let button_w = 90.0;
                let field_w = (ui.available_width() - gap - button_w).max(200.0);
                let edit_w = (field_w - 16.0 - CLEAR_GLYPH_ROOM).max(160.0);
                let input_id = ui.make_persistent_id("download-url");
                let focused = ui.memory(|memory| memory.has_focus(input_id));
                let mut edit = TextEdit::singleline(&mut self.url)
                    .id(input_id)
                    .font(FontId::new(14.0, FontFamily::Monospace))
                    .desired_width(edit_w)
                    .margin(egui::Margin {
                        left: 8,
                        right: 8 + CLEAR_GLYPH_ROOM as i8,
                        top: 5,
                        bottom: 5,
                    });
                if !focused {
                    edit = edit.hint_text(
                        RichText::new("paste a video link and press Enter").color(palette.dim),
                    );
                }
                let boxed = ui.allocate_ui_with_layout(
                    egui::vec2(field_w, URL_H),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        bordered_field(ui, palette);
                        ui.add(edit)
                    },
                );
                let field = boxed.inner;
                // The box's rect, as torrenter's file glyph uses: the
                // TextEdit's own response rect leaves out its margins, so
                // a glyph placed from it sits 24 px short of the border.
                if !self.url.is_empty() && clear_glyph(ui, palette, boxed.response.rect).clicked() {
                    clear = true;
                }
                // egui drops focus on Esc, so look at both.
                if (field.has_focus() || field.lost_focus())
                    && ui.input(|input| input.key_pressed(egui::Key::Escape))
                {
                    clear = true;
                }
                let entered =
                    field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
                let clicked = ui
                    .add_enabled(
                        ready && !self.url.trim().is_empty(),
                        egui::Button::new("fetch").min_size(egui::vec2(button_w, URL_H)),
                    )
                    .clicked();
                fetch = ready && (entered || clicked);
            },
        );
        if clear {
            self.clear_input();
        } else if fetch {
            self.start_fetch();
        }
    }

    /// Start over: empty the box and drop whatever the area below shows. A
    /// fetch still running is abandoned (its result no longer matches
    /// `fetch_seq`), and a sign-in test no longer refetches when it ends.
    fn clear_input(&mut self) {
        self.url.clear();
        self.fetch_seq += 1;
        self.fetch = Fetch::Idle;
        self.cookie_retry = None;
    }

    /// The download folder under the URL box: click the glyph or the path to
    /// choose another. New downloads go there; queued ones keep theirs.
    fn folder_row(&mut self, ui: &mut egui::Ui) {
        let palette = self.palette();
        let (row, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), DEST_H), Sense::hover());
        let mut pick = false;
        ui.allocate_new_ui(
            egui::UiBuilder::new()
                .max_rect(row)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let glyph = dest_glyph(ui, palette);
                let full = self.download_dir.display().to_string();
                let path = click_text(ui, truncate_middle(&full, 72), 12.0, palette.dim)
                    .on_hover_text(full);
                pick = glyph.clicked() || path.clicked();
            },
        );
        if pick {
            if let Some(dir) = pick_folder(&self.download_dir) {
                self.set_download_dir(dir);
            }
        }
    }

    fn set_download_dir(&mut self, dir: PathBuf) {
        self.download_dir = dir.clone();
        self.save_config(move |cfg| cfg.download_dir = Some(dir));
    }

    /// Write a change to `config.json` on the worker: the GUI thread does no
    /// file I/O. A failure comes back as a toast.
    fn save_config(&self, change: impl FnOnce(&mut config::AppConfig) + Send + 'static) {
        self.pending_saves.fetch_add(1, Ordering::SeqCst);
        let sent = self.tx.send(WorkerCmd::EditConfig {
            change: Box::new(change),
            pending: Arc::clone(&self.pending_saves),
        });
        if sent.is_err() {
            self.pending_saves.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn fetch_area(&mut self, ui: &mut egui::Ui) {
        let palette = self.palette();
        let mut action = None;
        // The sign-in card needs `&mut self`: render it from a snapshot.
        if let Fetch::SignIn { url } = &self.fetch {
            let url = url.clone();
            let chosen = card(ui, palette, |ui| {
                ui.label(
                    RichText::new("YouTube wants a signed-in browser")
                        .size(15.0)
                        .strong(),
                );
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "YouTube is asking this computer to confirm it is not a bot. ytd can use the YouTube sign-in from one of your browsers: it reads that browser's YouTube cookies from disk and sends them only to YouTube.",
                        )
                        .color(palette.dim),
                    )
                    .wrap(),
                );
                ui.label(RichText::new("use the sign-in from:").color(palette.dim));
                self.sign_in_choices(ui, false)
            });
            if let Some(Some(source)) = chosen {
                self.start_cookie_test(source, url, true);
            }
            return;
        }
        match &mut self.fetch {
            Fetch::Idle => {}
            Fetch::Loading => {
                card(ui, palette, |ui| {
                    ui.label(RichText::new("fetching video info…").color(palette.dim));
                    paint_progress(ui, palette.fg, palette.row_alt, None, None);
                });
            }
            Fetch::Failed(error) => {
                let error = error.clone();
                card(ui, palette, |ui| {
                    ui.label(RichText::new(error).color(palette.fail));
                });
            }
            // Drawn above, before this match borrows `self.fetch`.
            Fetch::SignIn { .. } => {}
            Fetch::Ready(draft) => {
                action = draft_ui(ui, draft, palette, &self.download_dir);
            }
        }
        match action {
            Some(DraftAction::Queue(request)) => {
                let queued_url = request.url.clone();
                match self.session.queue(request) {
                    Ok(_) => {
                        self.fetch = Fetch::Idle;
                        // A leftover search must not hide the new job.
                        self.job_query.clear();
                        // Keep a link typed while this one was loading.
                        if self.url.trim() == queued_url {
                            self.url.clear();
                        }
                        self.session.persist(true);
                    }
                    Err(error) => self.toast_fail(error),
                }
            }
            Some(DraftAction::Dismiss) => {
                // Clear the link too, unless another one was typed meanwhile.
                if let Fetch::Ready(draft) = &self.fetch {
                    if self.url.trim() == draft.url {
                        self.url.clear();
                    }
                }
                self.fetch = Fetch::Idle;
            }
            None => {}
        }
    }

    fn queue_ui(&mut self, ui: &mut egui::Ui) {
        let palette = self.palette();
        let jobs = self.session.jobs();
        let any_finished = jobs.iter().any(|job| job.snapshot.is_finished());
        ui.horizontal(|ui| {
            glass_label(ui, format!("queue ({})", jobs.len()), 14.0, palette.dim);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if any_finished && ui.small_button("clear finished").clicked() {
                    self.session.clear_finished();
                }
            });
        });
        if jobs.is_empty() {
            self.job_query.clear();
            glass_label(ui, "nothing yet: paste a link above", 14.0, palette.dim);
            return;
        }
        search_row(
            ui,
            palette,
            &mut self.job_query,
            "job-search",
            "type something…",
        );
        ui.add_space(6.0);
        let visible: Vec<&Job> = jobs
            .iter()
            .rev()
            .filter(|job| job_matches(job, &self.job_query))
            .collect();
        if visible.is_empty() {
            glass_label(ui, "no jobs match that search", 14.0, palette.dim);
            return;
        }
        let mut actions = Vec::new();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (index, job) in visible.into_iter().enumerate() {
                    if let Some(action) = job_row(ui, job, palette, index % 2 == 1) {
                        actions.push(action);
                    }
                }
            });
        for action in actions {
            match action {
                RowAction::TogglePause(id) => {
                    if let Err(error) = self.session.toggle_pause(id) {
                        self.toast_fail(error);
                    }
                }
                RowAction::Stop(id) => self.session.stop(id),
                RowAction::Restart(id) => {
                    if let Err(error) = self.session.restart(id) {
                        self.toast_fail(error);
                    }
                }
                RowAction::Remove(id) => self.session.remove(id),
                RowAction::Open(path) => {
                    if let Err(error) = open::that(&path) {
                        self.toast_fail(format!("could not open {}: {error}", path.display()));
                    }
                }
            }
            self.session.persist(true);
        }
    }

    // ------------------------------------------------------ settings page

    /// Settings as a page (torrenter): "← settings", a sidebar of tabs and
    /// one card per setting.
    fn settings_page(&mut self, ui: &mut egui::Ui) {
        let palette = self.palette();
        if page_bar(ui, palette, "settings") {
            self.show_settings = false;
            return;
        }
        let height = ui.available_height();
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(SIDEBAR_W, height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(SIDEBAR_W);
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.add_space(4.0);
                    for tab in SettingsTab::ALL {
                        if menu_row(ui, tab.label(), self.settings_tab == tab, palette).clicked() {
                            self.settings_tab = tab;
                        }
                    }
                },
            );
            let (line, _) = ui.allocate_exact_size(egui::vec2(1.0, height), Sense::hover());
            let edge = Color32::from_rgba_unmultiplied(
                palette.dim.r(),
                palette.dim.g(),
                palette.dim.b(),
                90,
            );
            ui.painter().line_segment(
                [line.center_top(), line.center_bottom()],
                Stroke::new(1.0_f32, edge),
            );
            ui.add_space(12.0);
            ui.vertical(|ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = CARD_GAP;
                        match self.settings_tab {
                            SettingsTab::Appearance => self.appearance_tab(ui, palette),
                            SettingsTab::Downloads => self.downloads_tab(ui, palette),
                            SettingsTab::SignIn => self.sign_in_tab(ui, palette),
                            SettingsTab::Updates => self.updates_tab(ui, palette),
                        }
                    });
            });
        });
    }

    fn appearance_tab(&mut self, ui: &mut egui::Ui, palette: Theme) {
        let fill = card_fill(palette.row);
        page_title(ui, "appearance", palette);
        let mut picked = None;
        // Swatches centred in the same box as the tint slider below.
        setting_card(
            ui,
            fill,
            palette,
            ("theme", "colour theme for every screen"),
            APPEARANCE_CONTROL_W,
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                let (swatch, gap) = (16.0, 10.0);
                ui.spacing_mut().item_spacing.x = gap;
                ui.add_space((APPEARANCE_CONTROL_W - (5.0 * swatch + 4.0 * gap)) / 2.0);
                ui.style_mut().interaction.interact_radius = 0.0;
                for id in ThemeId::ALL {
                    if theme_swatch(ui, theme(id).fg, self.theme == id, id.as_str()).clicked()
                        && self.theme != id
                    {
                        picked = Some(id);
                    }
                }
            },
        );
        if let Some(id) = picked {
            self.theme = id;
            let name = id.as_str().to_string();
            self.save_config(move |cfg| cfg.theme = Some(name));
        }
        #[cfg(target_os = "macos")]
        {
            let locked = self.viewport_fullscreen;
            let shown = config::effective_glass_tint(self.glass_tint, locked);
            let description = if locked {
                "locked at 100% in fullscreen"
            } else {
                "how strongly the theme colour sits on the window blur"
            };
            let mut persist = false;
            setting_card(
                ui,
                fill,
                palette,
                ("window tint", description),
                APPEARANCE_CONTROL_W,
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    glass_label(
                        ui,
                        format!("{}%", config::glass_tint_percent(shown)),
                        12.0,
                        palette.fg,
                    );
                    ui.allocate_ui(egui::vec2(ui.available_width(), 22.0), |ui| {
                        if locked {
                            let mut locked_tint = shown;
                            tint_slider(ui, &mut locked_tint, palette, true);
                        } else {
                            let response = tint_slider(ui, &mut self.glass_tint, palette, false);
                            // Save once the drag ends, not on every frame of it.
                            if response.drag_stopped()
                                || (response.changed() && !response.dragged())
                            {
                                persist = true;
                            }
                        }
                    });
                },
            );
            if persist {
                let tint = self.glass_tint;
                self.save_config(move |cfg| cfg.glass_tint = config::normalize_glass_tint(tint));
            }
        }
    }

    fn downloads_tab(&mut self, ui: &mut egui::Ui, palette: Theme) {
        page_title(ui, "downloads", palette);
        let mut picked = None;
        setting_card(
            ui,
            card_fill(palette.row),
            palette,
            (
                "downloads at once",
                "how many downloads run together; the rest wait in list order. Paused downloads do not count",
            ),
            240.0,
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                // right-to-left: listed in reverse so they read 1 → max
                for max in (1..=MAX_DOWNLOADS_CAP).rev() {
                    let on = self.max_downloads == max;
                    if route_chip(ui, &max.to_string(), on, palette).clicked() && !on {
                        picked = Some(max);
                    }
                }
            },
        );
        if let Some(max) = picked {
            self.max_downloads = max;
            self.session.set_max_downloads(max);
            self.save_config(move |cfg| cfg.max_downloads = Some(max));
        }
    }

    fn sign_in_tab(&mut self, ui: &mut egui::Ui, palette: Theme) {
        page_title(ui, "YouTube sign-in", palette);
        let current = self
            .session
            .tools()
            .and_then(|t| t.cookies)
            .map(|c| c.label())
            .unwrap_or_else(|| "not used".into());
        let mut choice = None;
        settings_card(ui, card_fill(palette.row), palette, |ui| {
            setting_row(
                ui,
                palette,
                (
                    "in use",
                    "only needed when YouTube asks to confirm you're not a bot. ytd reads the browser's YouTube cookies from disk and sends them only to YouTube",
                ),
                200.0,
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    ui.label(RichText::new(&current).size(13.0).color(palette.fg));
                },
            );
            tile_divider(ui, palette);
            choice = self.sign_in_choices(ui, true);
        });
        match choice {
            Some(Some(source)) => {
                self.start_cookie_test(source, ytdlp::COOKIE_TEST_URL.to_string(), false)
            }
            Some(None) => {
                self.apply_cookies(None);
                self.toast("YouTube sign-in no longer used");
            }
            None => {}
        }
    }

    fn updates_tab(&mut self, ui: &mut egui::Ui, palette: Theme) {
        let fill = card_fill(palette.row);
        page_title(ui, "updates", palette);
        let mut toggle = false;
        setting_card(
            ui,
            fill,
            palette,
            (
                "auto-update",
                "update ytd and its tools (yt-dlp, ffmpeg, deno) from GitHub Releases (same switch as the header)",
            ),
            SWITCH_CONTROL_W,
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                toggle = labeled_switch(ui, "", self.auto_update, palette).clicked();
            },
        );
        if toggle {
            self.set_auto_update(!self.auto_update);
        }
        if self.tool_rows.is_none() {
            let cfg = config::load_config();
            self.tool_rows = Some(
                Tool::ALL
                    .into_iter()
                    .map(|tool| ToolRow {
                        key: tool.key(),
                        version: cfg
                            .tools
                            .get(tool.key())
                            .map(|r| r.version.clone())
                            .unwrap_or_else(|| "system".into()),
                        location: tools::locate(tool)
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "missing".into()),
                    })
                    .collect(),
            );
        }
        let busy = self.session.any_running() || matches!(self.tools_state, ToolsState::Installing);
        let checking = self.tools_checking;
        let mut update_tools = false;
        settings_card(ui, fill, palette, |ui| {
            for (index, row) in self.tool_rows.iter().flatten().enumerate() {
                if index > 0 {
                    tile_divider(ui, palette);
                }
                setting_row(
                    ui,
                    palette,
                    (row.key, &row.location),
                    200.0,
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        ui.label(RichText::new(&row.version).size(12.0).color(palette.fg));
                    },
                );
            }
            tile_divider(ui, palette);
            ui.horizontal(|ui| {
                let label = if checking {
                    "checking…"
                } else {
                    "update tools"
                };
                let response = ui.add_enabled(!busy && !checking, egui::Button::new(label));
                if response.clicked() {
                    update_tools = true;
                }
                if busy {
                    response.on_disabled_hover_text("wait for downloads to finish");
                }
                if ui.button("open tools folder").clicked() {
                    let dir = config::tools_dir();
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = open::that(&dir);
                }
            });
        });
        if update_tools {
            // Only what is missing or behind its latest release gets
            // downloaded; GitHub is asked now, off the GUI thread.
            if self.tx.send(WorkerCmd::CheckTools).is_ok() {
                self.tools_checking = true;
                self.push_toast(CHECKING_TOOLS.into(), false, None, None);
            } else {
                self.toast_fail("could not check for tool updates: the worker stopped");
            }
        }
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        if self.toasts.is_empty() {
            return;
        }
        let palette = self.palette();
        let mut clicked = None;
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -16.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_max_width(380.0);
                ui.with_layout(egui::Layout::top_down(egui::Align::RIGHT), |ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    for toast in self.toasts.iter().rev().take(6) {
                        let fill = Color32::from_rgba_unmultiplied(
                            palette.row.r(),
                            palette.row.g(),
                            palette.row.b(),
                            235,
                        );
                        let color = if toast.fail {
                            palette.fail
                        } else {
                            palette.dim
                        };
                        let response = egui::Frame::NONE
                            .fill(fill)
                            .stroke(Stroke::new(1.0_f32, color))
                            .corner_radius(6)
                            .inner_margin(egui::Margin::symmetric(10, 8))
                            .show(ui, |ui| {
                                ui.set_max_width(360.0);
                                let label = ui.add(
                                    egui::Label::new(
                                        RichText::new(&toast.text).size(12.0).color(color),
                                    )
                                    .wrap(),
                                );
                                if let Some(snapshot) = &toast.progress {
                                    ui.add_space(4.0);
                                    paint_progress(
                                        ui,
                                        palette.fg,
                                        palette.row_alt,
                                        snapshot_fraction(snapshot),
                                        Some(label.rect.width().max(220.0)),
                                    );
                                    let downloaded = snapshot.downloaded.load(Ordering::Relaxed);
                                    let total = snapshot.total.load(Ordering::Relaxed);
                                    let detail = if total > 0 {
                                        format!(
                                            "{} {} / {}",
                                            snapshot.status(),
                                            format_size(downloaded),
                                            format_size(total)
                                        )
                                    } else {
                                        snapshot.status()
                                    };
                                    ui.label(RichText::new(detail).size(11.0).color(palette.dim));
                                }
                            })
                            .response;
                        if toast.progress.is_none() && response.interact(Sense::click()).clicked() {
                            clicked = Some(toast.id);
                        }
                    }
                });
            });
        if let Some(id) = clicked {
            self.toasts.retain(|toast| toast.id != id);
        }
    }
}

fn card<R>(ui: &mut egui::Ui, palette: Theme, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::NONE
        .fill(card_fill(palette.row))
        .stroke(Stroke::new(1.0_f32, palette.dim))
        .corner_radius(8)
        .inner_margin(12)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn draft_ui(
    ui: &mut egui::Ui,
    draft: &mut Draft,
    palette: Theme,
    dir: &Path,
) -> Option<DraftAction> {
    let mut action = None;
    card(ui, palette, |ui| {
        ui.horizontal_top(|ui| {
            match &draft.thumb {
                Some(texture) => {
                    ui.add(
                        egui::Image::from_texture(egui::load::SizedTexture::new(
                            texture.id(),
                            THUMB,
                        ))
                        .corner_radius(6),
                    );
                }
                None => {
                    let (rect, _) = ui.allocate_exact_size(THUMB, Sense::hover());
                    ui.painter().rect_filled(rect, 6.0, palette.row_alt);
                    let text = if draft.thumb_state == ThumbState::Loading {
                        "loading…"
                    } else {
                        "no preview"
                    };
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        text,
                        egui::FontId::monospace(12.0),
                        palette.dim,
                    );
                }
            }
            ui.vertical(|ui| {
                ui.add(
                    egui::Label::new(RichText::new(&draft.info.title).size(15.0).strong()).wrap(),
                );
                let mut meta = Vec::new();
                if let Some(uploader) = &draft.info.uploader {
                    meta.push(uploader.clone());
                }
                let duration = draft.info.duration_text();
                if !duration.is_empty() {
                    meta.push(duration);
                }
                if !meta.is_empty() {
                    ui.label(RichText::new(meta.join(" · ")).color(palette.dim));
                }
                ui.horizontal_wrapped(|ui| {
                    for kind in DownloadKind::ALL {
                        let response = ui
                            .selectable_label(draft.kind == kind, kind.short())
                            .on_hover_text(kind.label());
                        if response.clicked() && draft.kind != kind {
                            draft.kind = kind;
                            draft.reset_formats();
                        }
                    }
                });
            });
        });
        ui.add_space(4.0);

        egui::Grid::new("draft-grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                if draft.kind != DownloadKind::Audio {
                    let label = if draft.kind == DownloadKind::Single {
                        "format"
                    } else {
                        "video"
                    };
                    ui.label(RichText::new(label).color(palette.dim));
                    format_combo(
                        ui,
                        palette,
                        "video-combo",
                        &draft.info.video_choices(draft.kind),
                        &mut draft.video,
                    );
                    ui.end_row();
                }
                if draft.kind.needs_audio() {
                    ui.label(RichText::new("audio").color(palette.dim));
                    format_combo(
                        ui,
                        palette,
                        "audio-combo",
                        &draft.info.audio_choices(draft.kind),
                        &mut draft.audio,
                    );
                    ui.end_row();
                }
                ui.label(RichText::new("name").color(palette.dim));
                ui.horizontal(|ui| {
                    let ext = draft.ext().map(|e| format!(".{e}")).unwrap_or_default();
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.name)
                            .desired_width((ui.available_width() - 70.0).max(160.0)),
                    );
                    ui.label(RichText::new(ext).color(palette.dim));
                });
                ui.end_row();
                ui.label(RichText::new("trim").color(palette.dim));
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.trim_start)
                            .hint_text(RichText::new("start").color(palette.dim))
                            .desired_width(90.0),
                    );
                    ui.label(RichText::new("to").color(palette.dim));
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.trim_end)
                            .hint_text(RichText::new("end").color(palette.dim))
                            .desired_width(90.0),
                    );
                    ui.label(
                        RichText::new("3 · 3:00 · 1:02:03")
                            .size(11.0)
                            .color(palette.dim),
                    );
                });
                ui.end_row();
            });

        let request = draft.request(dir);
        if let Err(error) = &request {
            ui.label(RichText::new(error).color(palette.fail));
        }
        ui.horizontal(|ui| {
            let add = ui.add_enabled(request.is_ok(), egui::Button::new("add to queue"));
            if add.clicked() {
                if let Ok(request) = request.clone() {
                    action = Some(DraftAction::Queue(request));
                }
            }
            if ui.button("dismiss").clicked() {
                action = Some(DraftAction::Dismiss);
            }
        });
    });
    action
}

fn format_combo(
    ui: &mut egui::Ui,
    palette: Theme,
    id: &str,
    choices: &[&ytdlp::Format],
    selected: &mut Option<String>,
) {
    if choices.is_empty() {
        ui.label("none available for this choice");
        return;
    }
    let current = selected
        .as_deref()
        .and_then(|id| choices.iter().find(|f| f.format_id == id))
        .map(|f| f.label())
        .unwrap_or_else(|| "choose…".into());
    // The list opens over the window glass: give it a solid fill so the
    // rows behind it do not show through (torrenter's solid_popup_below).
    ui.scope(|ui| {
        let solid = Color32::from_rgb(palette.row.r(), palette.row.g(), palette.row.b());
        ui.visuals_mut().window_fill = solid;
        ui.visuals_mut().panel_fill = solid;
        egui::ComboBox::from_id_salt(id)
            .selected_text(current)
            .width(ui.available_width().min(420.0))
            .show_ui(ui, |ui| {
                for format in choices {
                    let chosen = selected.as_deref() == Some(format.format_id.as_str());
                    if ui.selectable_label(chosen, format.label()).clicked() {
                        *selected = Some(format.format_id.clone());
                    }
                }
            });
    });
}

// ---------------------------------------------- text on glass (torrenter)

fn glass_text_wash() -> Color32 {
    Color32::from_rgba_unmultiplied(16, 14, 16, 110)
}

/// A dark highlighter smear behind text drawn straight on the window glass,
/// so it stays readable over a light wallpaper. macOS only.
fn paint_brush_stroke(painter: &egui::Painter, rect: egui::Rect) {
    if !cfg!(target_os = "macos") {
        return;
    }
    if rect.width() < 0.5 || rect.height() < 0.5 {
        return;
    }
    let smear = rect.expand2(egui::vec2(5.0, 2.5));
    let radius = (smear.height() * 0.5).max(4.0);
    painter.rect_filled(smear, radius, glass_text_wash());
}

fn paint_washed_galley(
    painter: &egui::Painter,
    pos: egui::Pos2,
    galley: Arc<egui::Galley>,
    color: Color32,
) {
    paint_brush_stroke(
        painter,
        egui::Rect::from_min_size(pos, galley.size()).expand2(egui::vec2(4.0, 2.0)),
    );
    painter.galley(pos, galley, color);
}

/// One line, cut with "…" to the room left.
fn glass_text(
    ui: &mut egui::Ui,
    text: impl Into<String>,
    size: f32,
    color: Color32,
    sense: Sense,
) -> egui::Response {
    layout_glass_text(ui, text, size, color, sense, false)
}

/// Wrapping text with the glass wash.
fn glass_label(
    ui: &mut egui::Ui,
    text: impl Into<String>,
    size: f32,
    color: Color32,
) -> egui::Response {
    layout_glass_text(ui, text, size, color, Sense::hover(), true)
}

fn layout_glass_text(
    ui: &mut egui::Ui,
    text: impl Into<String>,
    size: f32,
    color: Color32,
    sense: Sense,
    wrap: bool,
) -> egui::Response {
    let pad = if cfg!(target_os = "macos") {
        egui::vec2(6.0, 4.0)
    } else {
        egui::vec2(0.0, 0.0)
    };
    let width = (ui.available_width() - pad.x).max(1.0);
    let format = egui::TextFormat::simple(FontId::new(size, FontFamily::Monospace), color);
    let mut job = egui::text::LayoutJob::single_section(text.into(), format);
    job.wrap = if wrap {
        egui::text::TextWrapping::wrap_at_width(width)
    } else {
        egui::text::TextWrapping::truncate_at_width(width)
    };
    let galley = ui.painter().layout_job(job);
    let (rect, response) = ui.allocate_exact_size(galley.size() + pad, sense);
    let pos = egui::pos2(
        rect.left() + pad.x * 0.5,
        rect.center().y - galley.size().y * 0.5,
    );
    paint_washed_galley(ui.painter(), pos, galley, color);
    response
}

/// The header's title and version under one wash; clickable.
fn glass_pair(
    ui: &mut egui::Ui,
    left: impl Into<String>,
    left_size: f32,
    left_color: Color32,
    right: impl Into<String>,
    right_size: f32,
    right_color: Color32,
) -> egui::Response {
    let painter = ui.painter();
    let left_galley = painter.layout_no_wrap(
        left.into(),
        FontId::new(left_size, FontFamily::Monospace),
        left_color,
    );
    let right_galley = painter.layout_no_wrap(
        right.into(),
        FontId::new(right_size, FontFamily::Monospace),
        right_color,
    );
    let gap = 7.0_f32;
    let pad = if cfg!(target_os = "macos") {
        egui::vec2(8.0, 4.0)
    } else {
        egui::vec2(0.0, 0.0)
    };
    let inner_h = left_galley.size().y.max(right_galley.size().y);
    let inner_w = left_galley.size().x + gap + right_galley.size().x;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(inner_w, inner_h) + pad, Sense::click());
    let bottom = rect.bottom() - pad.y * 0.5;
    let left_pos = egui::pos2(rect.left() + pad.x * 0.5, bottom - left_galley.size().y);
    let title_mid = left_pos.y + left_galley.size().y * 0.5;
    let right_pos = egui::pos2(
        left_pos.x + left_galley.size().x + gap,
        title_mid - right_galley.size().y * 0.5,
    );
    let smear = egui::Rect::from_min_max(
        egui::pos2(left_pos.x, bottom - inner_h),
        egui::pos2(right_pos.x + right_galley.size().x, bottom),
    );
    paint_brush_stroke(ui.painter(), smear);
    ui.painter().galley(left_pos, left_galley, left_color);
    ui.painter().galley(right_pos, right_galley, right_color);
    response
}

// --------------------------------------------- settings layout (torrenter)

/// Sidebar width, the gap between cards, and the control boxes on the right
/// of a setting tile (theme and tint share one so they line up).
const SIDEBAR_W: f32 = 190.0;
const CARD_GAP: f32 = 10.0;
const APPEARANCE_CONTROL_W: f32 = 260.0;
const SWITCH_CONTROL_W: f32 = 40.0;
const MENU_INSET: f32 = 20.0;

/// "← name" then a hairline: the first line of an inner page. True when the
/// arrow or the name is clicked (back to the downloads).
fn page_bar(ui: &mut egui::Ui, palette: Theme, name: &str) -> bool {
    let mut back = false;
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), 28.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            let (rect, arrow) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), Sense::click());
            let arrow = click_cursor(arrow);
            let lit = arrow.hovered();
            let painter = ui.painter();
            paint_brush_stroke(painter, rect.shrink(1.5));
            let stroke = Stroke::new(1.3_f32, if lit { palette.fg } else { palette.dim });
            let c = rect.center();
            painter.line_segment(
                [c + egui::vec2(6.0, 0.0), c + egui::vec2(-6.0, 0.0)],
                stroke,
            );
            painter.line_segment(
                [c + egui::vec2(-6.0, 0.0), c + egui::vec2(-1.5, -4.5)],
                stroke,
            );
            painter.line_segment(
                [c + egui::vec2(-6.0, 0.0), c + egui::vec2(-1.5, 4.5)],
                stroke,
            );
            back |= arrow.on_hover_text("back").clicked();
            ui.add_space(6.0);
            back |= click_text(ui, name, 13.0, palette.dim)
                .on_hover_text("back")
                .clicked();
        },
    );
    ui.add_space(8.0);
    hairline(ui, palette);
    ui.add_space(10.0);
    back
}

fn hairline(ui: &mut egui::Ui, palette: Theme) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        Stroke::new(1.0_f32, mix_rgb(palette.row_alt, palette.dim, 0.35)),
    );
}

/// The content pane's heading (the chosen tab).
fn page_title(ui: &mut egui::Ui, text: &str, palette: Theme) {
    ui.add_space(2.0);
    glass_label(ui, text, 15.0, palette.fg);
}

/// One setting as a full-width tile: title and description on the left,
/// the control in a box of `control_w` on the right.
fn setting_card(
    ui: &mut egui::Ui,
    fill: Color32,
    palette: Theme,
    text: (&str, &str),
    control_w: f32,
    layout: egui::Layout,
    control: impl FnOnce(&mut egui::Ui),
) {
    settings_card(ui, fill, palette, |ui| {
        setting_row(ui, palette, text, control_w, layout, control);
    });
}

/// A tile's content without the frame (rows inside one card share it).
fn setting_row(
    ui: &mut egui::Ui,
    palette: Theme,
    text: (&str, &str),
    control_w: f32,
    layout: egui::Layout,
    control: impl FnOnce(&mut egui::Ui),
) {
    let (title, description) = text;
    let width = ui.available_width();
    let text_w = (width - control_w - 24.0).max(80.0);
    let painter = ui.painter();
    let title = painter.layout_no_wrap(
        title.to_string(),
        FontId::new(13.0, FontFamily::Monospace),
        palette.fg,
    );
    let description = painter.layout(
        description.to_string(),
        FontId::new(11.0, FontFamily::Monospace),
        palette.dim,
        text_w,
    );
    let gap = 3.0;
    let text_h = title.size().y + gap + description.size().y;
    let row_h = text_h.max(30.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, row_h), Sense::hover());
    let top = rect.center().y - text_h / 2.0;
    let title_h = title.size().y;
    let painter = ui.painter();
    painter.galley(egui::pos2(rect.left(), top), title, palette.fg);
    painter.galley(
        egui::pos2(rect.left(), top + title_h + gap),
        description,
        palette.dim,
    );
    let control_rect =
        egui::Rect::from_min_max(egui::pos2(rect.right() - control_w, rect.top()), rect.max);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(control_rect).layout(layout));
    control(&mut child);
}

/// A thin line between rows inside one card.
fn tile_divider(ui: &mut egui::Ui, palette: Theme) {
    let color =
        Color32::from_rgba_unmultiplied(palette.dim.r(), palette.dim.g(), palette.dim.b(), 50);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        Stroke::new(1.0_f32, color),
    );
}

/// A settings card: the same frame as a download row.
fn settings_card(
    ui: &mut egui::Ui,
    fill: Color32,
    palette: Theme,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::NONE
        .fill(fill)
        .corner_radius(8)
        .stroke(Stroke::new(1.0_f32, palette.dim))
        .inner_margin(10.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            add_contents(ui);
        });
}

/// A sidebar row: lit and dotted when selected.
fn menu_row(ui: &mut egui::Ui, title: &str, selected: bool, palette: Theme) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 26.0), Sense::click());
    let painter = ui.painter();
    if response.hovered() {
        painter.rect_filled(rect, 3.0, palette.row_alt);
    }
    let color = if selected || response.hovered() {
        palette.fg
    } else {
        palette.dim
    };
    if selected {
        painter.circle_filled(
            egui::pos2(rect.left() + 9.0, rect.center().y),
            3.0,
            palette.fg,
        );
    }
    let title = painter.layout_no_wrap(
        title.to_string(),
        FontId::new(13.0, FontFamily::Monospace),
        color,
    );
    let pos = egui::pos2(
        rect.left() + MENU_INSET,
        rect.center().y - title.size().y / 2.0,
    );
    painter.galley(pos, title, color);
    click_cursor(response)
}

/// A small choice chip, outlined when selected.
fn route_chip(ui: &mut egui::Ui, text: &str, selected: bool, palette: Theme) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        text.into(),
        FontId::new(12.0, FontFamily::Monospace),
        if selected { palette.fg } else { palette.dim },
    );
    let size = galley.size() + egui::vec2(14.0, 8.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let color = if selected || response.hovered() {
        palette.fg
    } else {
        palette.dim
    };
    let painter = ui.painter();
    painter.galley(rect.min + egui::vec2(7.0, 4.0), galley, color);
    if selected {
        painter.rect_stroke(
            rect,
            4.0,
            Stroke::new(1.0_f32, color),
            egui::StrokeKind::Inside,
        );
    }
    click_cursor(response)
}

#[cfg(target_os = "macos")]
fn tint_slider(ui: &mut egui::Ui, value: &mut u8, palette: Theme, locked: bool) -> egui::Response {
    let min = config::GLASS_TINT_MIN;
    let max = config::GLASS_TINT_MAX;
    let knob_r = 5.0_f32;
    let width = ui.available_width().max(1.0);
    let sense = if locked {
        Sense::hover()
    } else {
        Sense::click_and_drag()
    };
    let (rect, mut response) = ui.allocate_exact_size(egui::vec2(width, 22.0), sense);
    let track = rect.shrink2(egui::vec2(knob_r, 0.0));
    let span = track.width().max(1.0);
    if !locked && (response.clicked() || response.dragged()) {
        if let Some(pointer) = response.interact_pointer_pos() {
            let t = ((pointer.x - track.left()) / span).clamp(0.0, 1.0);
            let next = min + (t * f32::from(max - min)).round() as u8;
            if next != *value {
                *value = next;
                response.mark_changed();
            }
        }
    }
    let t = f32::from(config::normalize_glass_tint(*value) - min) / f32::from(max - min);
    let y = track.center().y;
    let knob = egui::pos2(track.left() + t * span, y);
    let painter = ui.painter();
    painter.line_segment(
        [egui::pos2(track.left(), y), knob],
        Stroke::new(2.0_f32, palette.fg),
    );
    painter.line_segment(
        [knob, egui::pos2(track.right(), y)],
        Stroke::new(1.0_f32, palette.dim),
    );
    painter.circle_filled(knob, knob_r, if locked { palette.dim } else { palette.fg });
    if locked {
        response.on_hover_text("tint is locked at 100% in fullscreen")
    } else {
        click_cursor(response).on_hover_text("how heavy the theme wash sits on the blur")
    }
}

// ------------------------------------------------- header and search (torrenter)

/// A text field's border: dim, lit on hover or focus, never growing.
fn bordered_field(ui: &mut egui::Ui, palette: Theme) {
    let dim = Stroke::new(1.0_f32, palette.dim);
    let lit = Stroke::new(1.0_f32, palette.fg);
    let widgets = &mut ui.visuals_mut().widgets;
    widgets.inactive.bg_stroke = dim;
    widgets.hovered.bg_stroke = lit;
    widgets.active.bg_stroke = lit;
    widgets.open.bg_stroke = lit;
    widgets.hovered.expansion = 0.0;
    widgets.active.expansion = 0.0;
    widgets.open.expansion = 0.0;
}

fn click_cursor(response: egui::Response) -> egui::Response {
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn click_text(
    ui: &mut egui::Ui,
    text: impl Into<String>,
    size: f32,
    color: Color32,
) -> egui::Response {
    click_cursor(glass_text(ui, text, size, color, Sense::click()))
}

/// Inside the URL box, at its right end: clear it. `field` is the whole
/// box, border included; the × sits 8 px inside its right edge.
fn clear_glyph(ui: &mut egui::Ui, palette: Theme, field: egui::Rect) -> egui::Response {
    const ARM: f32 = 4.0;
    let center = egui::pos2(field.right() - 8.0 - ARM, field.center().y);
    let hit = egui::Rect::from_center_size(center, egui::vec2(18.0, 18.0));
    // click_and_drag, like the TextEdit under it: otherwise egui gives the
    // press's drag to the TextEdit, which takes focus and moves the caret.
    let response = ui.interact(
        hit,
        ui.id().with("url-clear-glyph"),
        Sense::click_and_drag(),
    );
    let color = if response.hovered() {
        palette.fg
    } else {
        palette.dim
    };
    let stroke = Stroke::new(1.4_f32, color);
    let painter = ui.painter();
    painter.line_segment(
        [
            center + egui::vec2(-ARM, -ARM),
            center + egui::vec2(ARM, ARM),
        ],
        stroke,
    );
    painter.line_segment(
        [
            center + egui::vec2(-ARM, ARM),
            center + egui::vec2(ARM, -ARM),
        ],
        stroke,
    );
    click_cursor(response).on_hover_text("clear (Esc)")
}

fn dest_glyph(ui: &mut egui::Ui, palette: Theme) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), Sense::click());
    paint_folder(ui.painter(), rect, palette.fg);
    click_cursor(response).on_hover_text("choose download folder")
}

fn paint_folder(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let stroke = Stroke::new(1.2_f32, color);
    let tab = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 1.5, rect.top() + 3.0),
        egui::pos2(rect.left() + 7.0, rect.top() + 6.5),
    );
    let body = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 1.5, rect.top() + 6.0),
        egui::pos2(rect.right() - 1.5, rect.bottom() - 2.0),
    );
    painter.rect_stroke(tab, 1.0, stroke, egui::StrokeKind::Inside);
    painter.rect_stroke(body, 1.2, stroke, egui::StrokeKind::Inside);
}

fn pick_folder(start: &Path) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title("download folder");
    if start.exists() {
        dialog = dialog.set_directory(start);
    } else if let Some(parent) = start.parent().filter(|path| path.exists()) {
        dialog = dialog.set_directory(parent);
    }
    dialog.pick_folder()
}

/// Keep both ends of a long path: `/Users/me/Dow…/ytd`.
fn truncate_middle(value: &str, max: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= max {
        return value.to_string();
    }
    if max <= 1 {
        return "…".into();
    }
    let keep = max - 1;
    let left = keep / 2;
    let right = keep - left;
    format!(
        "{}…{}",
        chars[..left].iter().collect::<String>(),
        chars[chars.len() - right..].iter().collect::<String>()
    )
}

/// The search field above the queue: label, a bordered field that lights up
/// on hover or focus, Esc clears it.
fn search_row(ui: &mut egui::Ui, palette: Theme, query: &mut String, id_salt: &str, hint: &str) {
    const SEARCH_H: f32 = 28.0;
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), SEARCH_H),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            glass_label(ui, "search", 12.0, palette.dim);
            let search_id = ui.make_persistent_id(id_salt);
            let focused = ui.memory(|memory| memory.has_focus(search_id));
            let field_w = ui.available_width().max(160.0);
            let mut edit = TextEdit::singleline(query)
                .id(search_id)
                .font(FontId::new(14.0, FontFamily::Monospace))
                .desired_width((field_w - 16.0).max(120.0))
                .margin(egui::Margin::symmetric(8, 5));
            if !focused {
                edit = edit.hint_text(RichText::new(hint).color(palette.dim));
            }
            let response = ui.allocate_ui_with_layout(
                egui::vec2(field_w, SEARCH_H),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    bordered_field(ui, palette);
                    ui.add(edit)
                },
            );
            let field = response.inner;
            if (field.has_focus() || field.lost_focus())
                && ui.input(|input| input.key_pressed(egui::Key::Escape))
            {
                query.clear();
            }
        },
    );
}

/// Kind, link, title, folder, date added or status contains the query.
fn job_matches(job: &Job, query: &str) -> bool {
    let needle = query.trim();
    if needle.is_empty() {
        return true;
    }
    let needle = needle.to_lowercase();
    [
        job.request.kind.label().to_string(),
        job.request.kind.short().to_string(),
        job.request.url.clone(),
        job.request.title.clone(),
        job.request.dir.display().to_string(),
        job.added.clone(),
        job.snapshot.status(),
    ]
    .iter()
    .any(|field| field.to_lowercase().contains(&needle))
}

fn job_row(ui: &mut egui::Ui, job: &Job, palette: Theme, alt: bool) -> Option<RowAction> {
    let snap = &job.snapshot;
    let status = snap.status();
    let finished = snap.is_finished();
    let paused = snap.is_paused();
    let done = finished && status == progress::DONE;
    let failed = finished && status == progress::FAILED;
    let mut action = None;
    egui::Frame::NONE
        .fill(card_fill(if alt { palette.row_alt } else { palette.row }))
        .stroke(Stroke::new(1.0_f32, palette.dim))
        .corner_radius(8)
        .inner_margin(10)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Buttons first, from the right; the kind and title get what is
            // left, so a long title is cut short instead of running under them.
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(&job.added).size(11.0).color(palette.dim));
                    let button = |ui: &mut egui::Ui, text: &str, color: Color32| {
                        ui.add(
                            egui::Button::new(RichText::new(text).size(12.0).color(color)).small(),
                        )
                        .clicked()
                    };
                    if finished {
                        if button(ui, "remove", palette.dim) {
                            action = Some(RowAction::Remove(job.id));
                        }
                        if !done && button(ui, "restart", palette.fg) {
                            action = Some(RowAction::Restart(job.id));
                        }
                        if let (true, Some(path)) = (done, snap.output()) {
                            if button(ui, "open", palette.fg) {
                                action = Some(RowAction::Open(path));
                            }
                        }
                    } else {
                        if button(ui, "stop", palette.fail) {
                            action = Some(RowAction::Stop(job.id));
                        }
                        let label = if paused { "resume" } else { "pause" };
                        if button(ui, label, palette.fg) {
                            action = Some(RowAction::TogglePause(job.id));
                        }
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(job.request.kind.short())
                                .size(11.0)
                                .color(palette.dim),
                        );
                        let folder = snap
                            .output()
                            .and_then(|p| p.parent().map(Path::to_path_buf))
                            .unwrap_or_else(|| job.request.dir.clone());
                        let title = ui
                            .add(
                                egui::Label::new(
                                    RichText::new(&job.request.title)
                                        .color(row_accent(palette, done)),
                                )
                                .truncate()
                                .sense(Sense::click()),
                            )
                            .on_hover_text(format!(
                                "{}\nclick to open the folder",
                                job.request.url
                            ));
                        if title.clicked() {
                            action = Some(RowAction::Open(folder));
                        }
                    });
                });
            });
            let accent = row_accent(palette, finished);
            let fraction = if paused || finished {
                Some(snapshot_fraction(snap).unwrap_or(if done { 1.0 } else { 0.0 }))
            } else {
                snapshot_fraction(snap)
            };
            paint_progress(ui, accent, palette.bg, fraction, None);
            let speed = snap.speed.load(Ordering::Relaxed);
            let eta = snap.eta.load(Ordering::Relaxed);
            let mut stats = vec![status.clone()];
            if let Some(size) = size_text(snap) {
                stats.push(size);
            }
            if !finished && !paused && speed > 1 {
                stats.push(format_speed(speed as f64));
                let eta = (eta != u64::MAX).then(|| Duration::from_secs(eta));
                stats.push(format!("eta {}", format_eta(eta)));
            }
            ui.label(
                RichText::new(stats.join("  "))
                    .size(12.0)
                    .color(palette.dim),
            );
            if failed {
                if let Some(error) = snap.error() {
                    ui.add(
                        egui::Label::new(RichText::new(error).size(12.0).color(palette.fail))
                            .wrap(),
                    );
                }
            }
        });
    ui.add_space(4.0);
    action
}

impl eframe::App for GuiApp {
    /// The window glass shows through a transparent clear (macOS only).
    #[cfg(target_os = "macos")]
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        crate::glass::clear_color()
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // The window may not be ready on the first frame: try again for a
        // while, and paint opaque until the blur is in place.
        if !self.glass_applied && self.glass_attempts < GLASS_ATTEMPTS {
            self.glass_attempts += 1;
            self.glass_applied = crate::glass::apply(frame);
        }
        self.viewport_fullscreen = ctx.input(|input| input.viewport().fullscreen == Some(true));
        let style = (self.theme, self.paint_tint());
        if self.applied_style != Some(style) {
            apply_theme(ctx, self.palette(), style.1);
            self.applied_style = Some(style);
        }
        self.drain_events(ctx);
        self.step_restart();
        self.session.persist(false);
        // Outdated tools found by auto-update (or by "update tools" during a
        // download): replace them once idle, and not while a check runs.
        if !self.tool_update_pending.is_empty()
            && matches!(self.tools_state, ToolsState::Ready)
            && !self.tools_checking
            && !self.session.any_running()
        {
            let stale = std::mem::take(&mut self.tool_update_pending);
            self.setup_tools(Some(stale));
        }

        let bg = self.surface(self.palette().bg);
        egui::TopBottomPanel::top("top")
            .frame(
                egui::Frame::NONE
                    .fill(bg)
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ctx, |ui| {
                self.top_bar(ui);
                // Inner pages keep only the title bar (torrenter).
                if !self.show_settings {
                    ui.add_space(GUTTER);
                    self.url_row(ui);
                    ui.add_space(GUTTER / 2.0);
                    self.folder_row(ui);
                }
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(bg)
                    .inner_margin(egui::Margin::symmetric(14, 6)),
            )
            .show(ctx, |ui| {
                ui.add_space(10.0);
                self.restart_banner(ui);
                if self.show_settings {
                    self.settings_page(ui);
                    return;
                }
                self.tools_banner(ui);
                self.fetch_area(ui);
                ui.add_space(8.0);
                self.queue_ui(ui);
            });
        self.toasts_ui(ctx);

        let busy = self.session.any_active()
            || self.restart.is_some()
            || !self.toasts.is_empty()
            || matches!(self.fetch, Fetch::Loading)
            || matches!(
                self.tools_state,
                ToolsState::Installing | ToolsState::Checking
            );
        ctx.request_repaint_after(if busy {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(1)
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_for_exit();
    }
}

impl GuiApp {
    /// Before quitting or relaunching: pause running jobs and wait for
    /// their yt-dlp/ffmpeg trees to end, so nothing keeps writing after the
    /// window is gone. They come back paused next time.
    fn save_for_exit(&mut self) {
        self.session.shutdown(Duration::from_secs(5));
        self.session.persist(true);
        // A setting picked just before quitting is still being written.
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.pending_saves.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractions() {
        assert_eq!(bar_fraction(progress::DOWNLOADING, 50, 100), Some(0.5));
        assert_eq!(
            bar_fraction(progress::DOWNLOADING, 0, 0),
            None,
            "no size yet slides instead of sitting at 0%"
        );
        assert_eq!(bar_fraction(progress::MERGING, 50, 100), None);
        assert_eq!(
            bar_fraction(progress::DOWNLOADING, 10, 0),
            None,
            "unknown total slides"
        );
        assert_eq!(bar_fraction(progress::DOWNLOADING, 200, 100), Some(1.0));
    }

    #[test]
    fn indeterminate_segment_stays_on_the_track() {
        for step in 0..100 {
            let offset = indeterminate_offset(step as f32 * 0.1, 0.28);
            assert!((0.0..=0.72 + f32::EPSILON).contains(&offset));
        }
    }

    #[test]
    fn middle_truncation_keeps_ends() {
        assert_eq!(truncate_middle("short", 16), "short");
        let value = truncate_middle("/Users/me/Downloads/ytd/very-long-name", 16);
        assert_eq!(value.chars().count(), 16);
        assert!(value.starts_with("/Users/"));
        assert!(value.ends_with("ong-name"), "{value}");
        assert!(value.contains('…'));
        assert_eq!(truncate_middle("abc", 1), "…");
    }

    fn sample_job(title: &str, url: &str, dir: &str) -> Job {
        let request = DownloadRequest {
            url: url.into(),
            title: title.into(),
            kind: DownloadKind::Audio,
            video_format: None,
            audio_format: Some("140".into()),
            name: "clip".into(),
            dir: PathBuf::from(dir),
            trim_start: None,
            trim_end: None,
            expected_size: 0,
        };
        Job {
            id: 1,
            request,
            snapshot: Arc::new(JobSnapshot::default()),
            added: "2026-11-26 10:00".into(),
            extra: Default::default(),
        }
    }

    #[test]
    fn empty_search_matches_every_job() {
        let job = sample_job("Sintel", "https://example.com/v", "/tmp/ytd");
        assert!(job_matches(&job, ""));
        assert!(job_matches(&job, "   "));
    }

    #[test]
    fn search_matches_title_url_and_folder() {
        let job = sample_job(
            "Sintel Trailer",
            "https://example.com/watch?v=abc",
            "/tmp/clips",
        );
        assert!(job_matches(&job, "sintel"));
        assert!(job_matches(&job, "watch?v=ABC"));
        assert!(job_matches(&job, "clips"));
        assert!(job_matches(&job, "queued"), "status");
        assert!(job_matches(&job, "2026-11-26"), "date added");
        assert!(!job_matches(&job, "big buck bunny"));
    }

    #[test]
    fn theme_names_round_trip() {
        for id in ThemeId::ALL {
            assert_eq!(ThemeId::from_config(Some(id.as_str())), id);
        }
        assert_eq!(ThemeId::from_config(None), ThemeId::Scarlet);
    }
}
