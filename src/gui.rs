//! The egui desktop app: paste a link, preview it, pick what to download,
//! and watch the queue. Styled after torrenter: monospace text, dark
//! palettes, custom-painted progress bars with a sliding "busy" segment.
//!
//! The UI thread never blocks on the network: tool installs, video info and
//! thumbnails run on a worker thread with its own Tokio runtime, and
//! downloads run inside [`Session`].

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, FontFamily, RichText, Sense, Stroke, Vec2};

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

pub fn run() -> eframe::Result<()> {
    update::cleanup_replaced_binary();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("ytd")
            .with_title(format!("ytd {}", crate::VERSION))
            .with_inner_size(WINDOW_INNER)
            .with_min_inner_size(WINDOW_MIN)
            .with_icon(Arc::new(crate::icon::app_icon())),
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

fn apply_theme(ctx: &egui::Context, palette: Theme) {
    let mut visuals = egui::Visuals::dark();
    visuals.override_text_color = Some(palette.fg);
    visuals.panel_fill = palette.bg;
    visuals.window_fill = palette.bg;
    visuals.extreme_bg_color = palette.row;
    visuals.faint_bg_color = palette.row_alt;
    visuals.widgets.inactive.bg_fill = palette.row;
    visuals.widgets.inactive.weak_bg_fill = palette.row;
    visuals.widgets.hovered.bg_fill = palette.row_alt;
    visuals.widgets.hovered.weak_bg_fill = palette.row_alt;
    visuals.widgets.active.bg_fill = palette.row_alt;
    visuals.widgets.active.weak_bg_fill = palette.row_alt;
    visuals.widgets.open.bg_fill = palette.row_alt;
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
    if progress::is_indeterminate(status) || (total == 0 && downloaded > 0) {
        return None;
    }
    if total == 0 {
        return Some(0.0);
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
}

enum WorkerEvent {
    ToolsDone(Result<ToolPaths, String>),
    UpdatesChecked {
        app: Result<Option<AvailableUpdate>, String>,
        tools: Vec<Tool>,
    },
    AppInstalled(Result<String, String>),
    CookiesTested {
        source: CookieSource,
        result: Result<(), String>,
    },
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
                },
                WorkerCmd::TestCookies { source, url, tools } => {
                    let result = ytdlp::test_cookies(&tools, &source, &url).await;
                    WorkerEvent::CookiesTested { source, result }
                }
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
        };
        if let Hydrate::Corrupt(error) = app.session.hydrate() {
            app.toast_fail(format!("saved list was unreadable and was reset: {error}"));
        }
        apply_theme(&cc.egui_ctx, theme(app.theme));
        app.setup_tools(None);
        app
    }

    fn palette(&self) -> Theme {
        theme(self.theme)
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
                Err(error) => {
                    let message =
                        if cfg!(target_os = "macos") && tools::locate(Tool::Ffmpeg).is_none() {
                            tools::MACOS_FFMPEG_HINT.to_string()
                        } else {
                            format!("{error:#}")
                        };
                    self.tools_state = ToolsState::Failed(message);
                }
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
        let _ = self.tx.send(WorkerCmd::CheckUpdates);
    }

    fn set_auto_update(&mut self, on: bool) {
        self.auto_update = on;
        let _ = config::edit_config(move |cfg| cfg.auto_update = on);
        if on && matches!(self.tools_state, ToolsState::Ready) {
            self.toast("checking for updates…");
            self.check_updates();
        }
    }

    fn on_updates_checked(
        &mut self,
        app: Result<Option<AvailableUpdate>, String>,
        stale: Vec<Tool>,
    ) {
        match app {
            Ok(Some(available)) if update::can_self_replace() => {
                let snapshot = Arc::new(JobSnapshot::default());
                snapshot.set_status(progress::STARTING);
                self.push_toast(
                    format!("installing ytd {}…", available.tag),
                    false,
                    None,
                    Some(Arc::clone(&snapshot)),
                );
                let _ = self.tx.send(WorkerCmd::InstallApp {
                    update: available,
                    snapshot,
                });
            }
            Ok(Some(available)) => self.push_toast(
                format!(
                    "ytd {} is available: github.com/{}/releases",
                    available.tag,
                    update::REPO
                ),
                false,
                Some(Duration::from_secs(15)),
                None,
            ),
            Ok(None) => {}
            Err(error) => self.toast_fail(format!("update check failed: {error}")),
        }
        if !stale.is_empty() {
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
        let _ = config::edit_config(move |cfg| cfg.cookies = saved);
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
                WorkerEvent::Info { seq, url, result } if seq == self.fetch_seq => match result {
                    Ok(info) => {
                        let thumb = info.thumbnail_url();
                        self.fetch = Fetch::Ready(Box::new(Draft::new(url, info)));
                        if let Some(thumb) = thumb {
                            let _ = self.tx.send(WorkerCmd::Thumb { seq, url: thumb });
                        }
                    }
                    Err(error) if cookies::is_sign_in_error(&error) => {
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
                WorkerEvent::UpdatesChecked { app, tools } => {
                    let asked = self
                        .toasts
                        .iter()
                        .any(|t| t.text == "checking for updates…");
                    self.toasts
                        .retain(|toast| toast.text != "checking for updates…");
                    if asked && matches!(app, Ok(None)) && tools.is_empty() {
                        self.toast("ytd and its tools are up to date");
                    }
                    self.on_updates_checked(app, tools);
                }
                WorkerEvent::AppInstalled(Ok(tag)) => self.push_toast(
                    format!("ytd {tag} installed: restart ytd to use it"),
                    false,
                    Some(Duration::from_secs(30)),
                    None,
                ),
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
                WorkerEvent::AppInstalled(Err(error)) => {
                    self.toast_fail(format!("update failed: {error}"))
                }
                // A newer fetch replaced this one.
                WorkerEvent::Info { .. } | WorkerEvent::Thumb { .. } => {}
            }
        }
        for event in self.session.drain_events() {
            match event {
                SessionEvent::Finished { title, .. } => self.toast(format!("saved {title}")),
                SessionEvent::Failed { title, error, .. } if cookies::is_sign_in_error(&error) => {
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

    fn save_settings(&self) {
        let dir = self.download_dir.clone();
        let max = self.max_downloads;
        let theme = self.theme.as_str().to_string();
        let _ = config::edit_config(move |cfg| {
            cfg.download_dir = Some(dir);
            cfg.max_downloads = Some(max);
            cfg.theme = Some(theme);
        });
    }

    // ---------------------------------------------------------------- UI

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let palette = self.palette();
        ui.horizontal(|ui| {
            ui.label(RichText::new("ytd").size(20.0).strong().color(palette.fg));
            ui.label(RichText::new(format!("v{}", crate::VERSION)).color(palette.dim));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if settings_cog(ui, palette, self.show_settings).clicked() {
                    self.show_settings = !self.show_settings;
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
                        ui.label(
                            RichText::new("checking tools…")
                                .color(palette.dim)
                                .size(12.0),
                        );
                    }
                    ToolsState::Installing => {
                        ui.label(
                            RichText::new("setting up tools…")
                                .color(palette.dim)
                                .size(12.0),
                        );
                    }
                    ToolsState::Failed(_) => {
                        ui.label(
                            RichText::new("tools missing")
                                .color(palette.fail)
                                .size(12.0),
                        );
                    }
                }
            });
        });
    }

    fn tools_banner(&mut self, ui: &mut egui::Ui) {
        let palette = self.palette();
        let ToolsState::Failed(error) = &self.tools_state else {
            return;
        };
        let error = error.clone();
        egui::Frame::NONE
            .fill(palette.row)
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

    fn url_row(&mut self, ui: &mut egui::Ui) {
        let ready = matches!(self.tools_state, ToolsState::Ready);
        let palette = self.palette();
        ui.horizontal(|ui| {
            let button_w = 90.0;
            let edit = egui::TextEdit::singleline(&mut self.url)
                .hint_text(RichText::new("paste a video link and press Enter").color(palette.dim))
                .desired_width((ui.available_width() - button_w).max(120.0));
            let response = ui.add(edit);
            let entered = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let clicked = ui
                .add_enabled(
                    ready && !self.url.trim().is_empty(),
                    egui::Button::new("fetch"),
                )
                .clicked();
            if ready && (entered || clicked) {
                self.start_fetch();
            }
        });
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
                        // Keep a link typed while this one was loading.
                        if self.url.trim() == queued_url {
                            self.url.clear();
                        }
                        self.session.persist(true);
                    }
                    Err(error) => self.toast_fail(error),
                }
            }
            Some(DraftAction::Dismiss) => self.fetch = Fetch::Idle,
            None => {}
        }
    }

    fn queue_ui(&mut self, ui: &mut egui::Ui) {
        let palette = self.palette();
        let jobs = self.session.jobs();
        let any_finished = jobs.iter().any(|job| job.snapshot.is_finished());
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("queue ({})", jobs.len())).color(palette.dim));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if any_finished && ui.small_button("clear finished").clicked() {
                    self.session.clear_finished();
                }
            });
        });
        if jobs.is_empty() {
            ui.label(RichText::new("nothing yet: paste a link above").color(palette.dim));
            return;
        }
        let mut actions = Vec::new();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (index, job) in jobs.iter().rev().enumerate() {
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

    fn settings_ui(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let palette = self.palette();
        let mut open = true;
        let mut changed = false;
        let mut update_tools = false;
        let mut cookie_choice: Option<Option<CookieSource>> = None;
        egui::Window::new("settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::RIGHT_TOP, [-12.0, 48.0])
            .show(ctx, |ui| {
                ui.set_width(420.0);
                ui.label(RichText::new("download folder").color(palette.dim));
                ui.horizontal(|ui| {
                    ui.label(self.download_dir.display().to_string());
                });
                ui.horizontal(|ui| {
                    if ui.button("change…").clicked() {
                        if let Some(dir) = rfd::FileDialog::new()
                            .set_directory(&self.download_dir)
                            .pick_folder()
                        {
                            self.download_dir = dir;
                            changed = true;
                        }
                    }
                    if ui.button("open").clicked() {
                        let _ = std::fs::create_dir_all(&self.download_dir);
                        let _ = open::that(&self.download_dir);
                    }
                });
                ui.separator();
                ui.label(RichText::new("downloads at once").color(palette.dim));
                let mut max = self.max_downloads;
                if ui
                    .add(egui::Slider::new(&mut max, 1..=MAX_DOWNLOADS_CAP))
                    .changed()
                {
                    self.max_downloads = max;
                    self.session.set_max_downloads(max);
                    changed = true;
                }
                ui.separator();
                ui.label(RichText::new("theme").color(palette.dim));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    ui.add_space(2.0);
                    for id in ThemeId::ALL {
                        let swatch = theme_swatch(ui, theme(id).fg, self.theme == id, id.as_str());
                        if swatch.clicked() && self.theme != id {
                            self.theme = id;
                            apply_theme(ctx, theme(id));
                            changed = true;
                        }
                    }
                });
                ui.separator();
                ui.label(RichText::new("YouTube sign-in").color(palette.dim));
                let current = self
                    .session
                    .tools()
                    .and_then(|t| t.cookies)
                    .map(|c| c.label())
                    .unwrap_or_else(|| "not used".into());
                ui.label(current).on_hover_text(
                    "Only needed when YouTube asks to confirm you're not a bot. ytd reads the browser's YouTube cookies from disk and sends them only to YouTube.",
                );
                if let Some(choice) = self.sign_in_choices(ui, true) {
                    cookie_choice = Some(choice);
                }
                ui.separator();
                ui.label(RichText::new("tools").color(palette.dim));
                let cfg = config::load_config();
                for tool in Tool::ALL {
                    let location = tools::locate(tool)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "missing".into());
                    let version = cfg
                        .tools
                        .get(tool.key())
                        .map(|r| r.version.clone())
                        .unwrap_or_else(|| "system".into());
                    ui.label(format!("{:<7} {version}", tool.key()));
                    ui.label(RichText::new(location).size(11.0).color(palette.dim));
                }
                ui.horizontal(|ui| {
                    let busy = self.session.any_running()
                        || matches!(self.tools_state, ToolsState::Installing);
                    let response = ui.add_enabled(!busy, egui::Button::new("update tools"));
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
        if !open {
            self.show_settings = false;
        }
        if changed {
            self.save_settings();
        }
        match cookie_choice {
            Some(Some(source)) => {
                self.start_cookie_test(source, ytdlp::COOKIE_TEST_URL.to_string(), false)
            }
            Some(None) => {
                self.apply_cookies(None);
                self.toast("YouTube sign-in no longer used");
            }
            None => {}
        }
        if update_tools {
            self.setup_tools(Some(tools::managed()));
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
        .fill(palette.row)
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
            ui.label(
                RichText::new(format!("→ {}", dir.display()))
                    .size(11.0)
                    .color(palette.dim),
            );
        });
    });
    action
}

fn format_combo(
    ui: &mut egui::Ui,
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
        .fill(if alt { palette.row_alt } else { palette.row })
        .stroke(Stroke::new(1.0_f32, palette.dim))
        .corner_radius(8)
        .inner_margin(10)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
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
                            RichText::new(&job.request.title).color(row_accent(palette, done)),
                        )
                        .truncate()
                        .sense(Sense::click()),
                    )
                    .on_hover_text(format!("{}\nclick to open the folder", job.request.url));
                if title.clicked() {
                    action = Some(RowAction::Open(folder));
                }
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
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events(ctx);
        self.session.persist(false);
        // Outdated tools found by auto-update: replace them once idle.
        if !self.tool_update_pending.is_empty()
            && matches!(self.tools_state, ToolsState::Ready)
            && !self.session.any_running()
        {
            let stale = std::mem::take(&mut self.tool_update_pending);
            self.setup_tools(Some(stale));
        }

        egui::TopBottomPanel::top("top")
            .frame(
                egui::Frame::NONE
                    .fill(self.palette().bg)
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ctx, |ui| self.top_bar(ui));
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(self.palette().bg)
                    .inner_margin(egui::Margin::symmetric(14, 6)),
            )
            .show(ctx, |ui| {
                ui.add_space(10.0);
                self.tools_banner(ui);
                self.url_row(ui);
                ui.add_space(4.0);
                self.fetch_area(ui);
                ui.add_space(8.0);
                self.queue_ui(ui);
            });
        self.settings_ui(ctx);
        self.toasts_ui(ctx);

        let busy = self.session.any_active()
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
        // Pause running jobs and wait for their yt-dlp/ffmpeg trees to end,
        // so nothing keeps writing after the window is gone. They come back
        // paused next time.
        self.session.shutdown(Duration::from_secs(5));
        self.session.persist(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractions() {
        assert_eq!(bar_fraction(progress::DOWNLOADING, 50, 100), Some(0.5));
        assert_eq!(bar_fraction(progress::DOWNLOADING, 0, 0), Some(0.0));
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
    fn theme_names_round_trip() {
        for id in ThemeId::ALL {
            assert_eq!(ThemeId::from_config(Some(id.as_str())), id);
        }
        assert_eq!(ThemeId::from_config(None), ThemeId::Scarlet);
    }
}
