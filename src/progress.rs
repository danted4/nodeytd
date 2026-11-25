//! Shared progress state and control flags.
//!
//! An engine (tool install, yt-dlp download) reports through a
//! [`ProgressHandle`]: an indicatif bar on the CLI, an `Arc<JobSnapshot>` for
//! the GUI. Pause and stop are atomic flags the engine polls; they surface as
//! the typed errors [`DownloadPaused`] / [`DownloadStopped`], never as failures.

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressState, ProgressStyle};

pub const QUEUED: &str = "queued";
pub const STARTING: &str = "starting";
pub const DOWNLOADING: &str = "downloading";
pub const MERGING: &str = "merging";
pub const CONVERTING: &str = "converting";
pub const EXTRACTING: &str = "extracting";
pub const VERIFYING: &str = "verifying";
pub const PAUSED: &str = "paused";
pub const DONE: &str = "done";
pub const STOPPED: &str = "stopped";
pub const FAILED: &str = "failed";

/// Statuses with no byte count to show: the GUI draws a sliding bar.
pub fn is_indeterminate(status: &str) -> bool {
    matches!(
        status,
        QUEUED | STARTING | MERGING | CONVERTING | EXTRACTING | VERIFYING
    )
}

#[derive(Debug)]
pub struct DownloadPaused;

impl fmt::Display for DownloadPaused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("download paused")
    }
}

impl std::error::Error for DownloadPaused {}

#[derive(Debug)]
pub struct DownloadStopped;

impl fmt::Display for DownloadStopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("download stopped")
    }
}

impl std::error::Error for DownloadStopped {}

pub fn is_pause_abort(error: &anyhow::Error) -> bool {
    error.downcast_ref::<DownloadPaused>().is_some()
}

pub fn is_stopped(error: &anyhow::Error) -> bool {
    error.downcast_ref::<DownloadStopped>().is_some()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What a front reads about one job. Counters are atomics so the engine can
/// update them without blocking the GUI thread.
#[derive(Debug)]
pub struct JobSnapshot {
    pub downloaded: AtomicU64,
    pub total: AtomicU64,
    /// Bytes per second as yt-dlp reports it.
    pub speed: AtomicU64,
    /// Seconds left; `u64::MAX` when unknown.
    pub eta: AtomicU64,
    /// `total` is yt-dlp's running estimate, shown with a `~`.
    pub estimated: AtomicBool,
    /// Highest bar fraction shown so far (x 10 000): an estimate that grows
    /// never makes the bar slide backwards.
    peak: AtomicU32,
    finished: AtomicBool,
    paused: AtomicBool,
    stop: AtomicBool,
    /// An engine task owns this job right now (see `Session`).
    running: AtomicBool,
    status: Mutex<String>,
    title: Mutex<String>,
    error: Mutex<Option<String>>,
    output: Mutex<Option<PathBuf>>,
}

impl Default for JobSnapshot {
    fn default() -> Self {
        Self {
            downloaded: AtomicU64::new(0),
            total: AtomicU64::new(0),
            speed: AtomicU64::new(0),
            eta: AtomicU64::new(u64::MAX),
            estimated: AtomicBool::new(false),
            peak: AtomicU32::new(0),
            finished: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            running: AtomicBool::new(false),
            status: Mutex::new(QUEUED.to_string()),
            title: Mutex::new(String::new()),
            error: Mutex::new(None),
            output: Mutex::new(None),
        }
    }
}

impl JobSnapshot {
    pub fn status(&self) -> String {
        lock(&self.status).clone()
    }

    pub fn set_status(&self, status: impl Into<String>) {
        *lock(&self.status) = status.into();
    }

    pub fn title(&self) -> String {
        lock(&self.title).clone()
    }

    pub fn set_title(&self, title: impl Into<String>) {
        *lock(&self.title) = title.into();
    }

    pub fn error(&self) -> Option<String> {
        lock(&self.error).clone()
    }

    pub fn output(&self) -> Option<PathBuf> {
        lock(&self.output).clone()
    }

    pub fn set_output(&self, path: Option<PathBuf>) {
        *lock(&self.output) = path;
    }

    pub fn set_error(&self, error: impl Into<String>) {
        *lock(&self.error) = Some(error.into());
        self.speed.store(0, Ordering::Relaxed);
        self.finished.store(true, Ordering::Relaxed);
        self.set_status(FAILED);
    }

    pub fn mark_finished(&self) {
        let total = self.total.load(Ordering::Relaxed);
        if total > 0 {
            self.downloaded.store(total, Ordering::Relaxed);
        }
        self.speed.store(0, Ordering::Relaxed);
        self.eta.store(0, Ordering::Relaxed);
        self.finished.store(true, Ordering::Relaxed);
        self.set_status(DONE);
    }

    pub fn mark_stopped(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);
        self.speed.store(0, Ordering::Relaxed);
        self.finished.store(true, Ordering::Relaxed);
        self.set_status(STOPPED);
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
        if paused {
            self.speed.store(0, Ordering::Relaxed);
            self.eta.store(u64::MAX, Ordering::Relaxed);
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    pub fn is_done(&self) -> bool {
        self.is_finished() && self.status() == DONE
    }

    /// Still in the queue or downloading (paused jobs count: they can resume).
    pub fn is_active(&self) -> bool {
        !self.is_finished()
    }

    /// `raw`, but never below what the bar already showed.
    pub fn steady_fraction(&self, raw: f32) -> f32 {
        let scaled = (raw.clamp(0.0, 1.0) * 10_000.0) as u32;
        let previous = self.peak.fetch_max(scaled, Ordering::Relaxed);
        previous.max(scaled) as f32 / 10_000.0
    }

    /// Claim the job for an engine task. Exactly one caller wins.
    pub fn try_claim(&self) -> bool {
        self.running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn release_claim(&self) {
        self.running.store(false, Ordering::Release);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Restore counters and state from a persisted record.
    pub fn restore(&self, downloaded: u64, total: u64, status: &str, error: Option<String>) {
        self.downloaded.store(downloaded, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
        match status {
            DONE => self.mark_finished(),
            STOPPED => self.mark_stopped(),
            FAILED => self.set_error(error.unwrap_or_else(|| "failed".to_string())),
            _ => {
                // Anything unfinished comes back paused: the user resumes it.
                self.set_paused(true);
                self.set_status(PAUSED);
            }
        }
    }

    /// Bails with [`DownloadStopped`] or [`DownloadPaused`] when asked to.
    pub fn check_control(&self) -> anyhow::Result<()> {
        if self.should_stop() {
            anyhow::bail!(DownloadStopped);
        }
        if self.is_paused() {
            anyhow::bail!(DownloadPaused);
        }
        Ok(())
    }
}

const BAR_TEMPLATE: &str =
    "{msg:>11} [{bar:24}] {percent:>3}% {have}/{full} @ {speed_txt} eta {eta_txt}";

fn cli_style() -> ProgressStyle {
    ProgressStyle::with_template(BAR_TEMPLATE)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("█░")
        .with_key("have", |state: &ProgressState, w: &mut dyn fmt::Write| {
            let _ = w.write_str(&format_size(state.pos()));
        })
        .with_key("full", |state: &ProgressState, w: &mut dyn fmt::Write| {
            let _ = w.write_str(&format_size(state.len().unwrap_or(0)));
        })
        .with_key(
            "speed_txt",
            |state: &ProgressState, w: &mut dyn fmt::Write| {
                let _ = w.write_str(&format_speed(state.per_sec()));
            },
        )
        .with_key(
            "eta_txt",
            |state: &ProgressState, w: &mut dyn fmt::Write| {
                let _ = w.write_str(&format_eta(Some(state.eta())));
            },
        )
}

/// One handle that drives a CLI bar, a GUI snapshot, or nothing.
#[derive(Clone, Debug)]
pub enum ProgressHandle {
    Cli(ProgressBar),
    Gui(std::sync::Arc<JobSnapshot>),
    Hidden,
}

impl ProgressHandle {
    pub fn cli(label: &str) -> Self {
        let bar = ProgressBar::new(1);
        bar.set_style(cli_style());
        bar.set_message(label.to_string());
        Self::Cli(bar)
    }

    pub fn gui(snapshot: std::sync::Arc<JobSnapshot>) -> Self {
        Self::Gui(snapshot)
    }

    pub fn snapshot(&self) -> Option<&JobSnapshot> {
        match self {
            Self::Gui(snapshot) => Some(snapshot),
            _ => None,
        }
    }

    pub fn set_length(&self, total: u64) {
        match self {
            Self::Cli(bar) => bar.set_length(total.max(1)),
            Self::Gui(snapshot) => snapshot.total.store(total, Ordering::Relaxed),
            Self::Hidden => {}
        }
    }

    pub fn set_position(&self, downloaded: u64) {
        match self {
            Self::Cli(bar) => bar.set_position(downloaded),
            Self::Gui(snapshot) => snapshot.downloaded.store(downloaded, Ordering::Relaxed),
            Self::Hidden => {}
        }
    }

    pub fn set_rate(&self, speed: Option<f64>, eta: Option<u64>) {
        if let Self::Gui(snapshot) = self {
            snapshot
                .speed
                .store(speed.unwrap_or(0.0).max(0.0) as u64, Ordering::Relaxed);
            snapshot
                .eta
                .store(eta.unwrap_or(u64::MAX), Ordering::Relaxed);
        }
    }

    pub fn set_estimated(&self, estimated: bool) {
        if let Self::Gui(snapshot) = self {
            snapshot.estimated.store(estimated, Ordering::Relaxed);
        }
    }

    pub fn set_status(&self, status: &str) {
        match self {
            Self::Cli(bar) => bar.set_message(status.to_string()),
            Self::Gui(snapshot) => snapshot.set_status(status),
            Self::Hidden => {}
        }
    }

    pub fn finish(&self) {
        match self {
            Self::Cli(bar) => bar.finish(),
            Self::Gui(snapshot) => snapshot.mark_finished(),
            Self::Hidden => {}
        }
    }

    pub fn abandon(&self) {
        if let Self::Cli(bar) = self {
            bar.abandon();
        }
    }

    /// Stop or pause requested on the GUI snapshot (the CLI never asks).
    pub fn check_control(&self) -> anyhow::Result<()> {
        match self {
            Self::Gui(snapshot) => snapshot.check_control(),
            _ => Ok(()),
        }
    }
}

/// 1024-based, two decimals: `12.34 MB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

pub fn format_speed(bytes_per_sec: f64) -> String {
    format!("{}/s", format_size(bytes_per_sec.max(0.0) as u64))
}

/// Largest unit only: `3 minutes`, `12 seconds`, `N/A`.
pub fn format_eta(eta: Option<Duration>) -> String {
    let Some(eta) = eta else {
        return "N/A".to_string();
    };
    let secs = eta.as_secs();
    let plural = |n: u64, unit: &str| {
        if n == 1 {
            format!("1 {unit}")
        } else {
            format!("{n} {unit}s")
        }
    };
    if secs >= 3600 {
        plural(secs / 3600, "hour")
    } else if secs >= 60 {
        plural(secs / 60, "minute")
    } else {
        plural(secs, "second")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_eta_format() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.50 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.00 MB");
        assert_eq!(format_eta(None), "N/A");
        assert_eq!(format_eta(Some(Duration::from_secs(1))), "1 second");
        assert_eq!(format_eta(Some(Duration::from_secs(190))), "3 minutes");
        assert_eq!(format_eta(Some(Duration::from_secs(7300))), "2 hours");
    }

    #[test]
    fn control_flags_become_typed_errors() {
        let snap = JobSnapshot::default();
        assert!(snap.check_control().is_ok());
        snap.set_paused(true);
        assert!(is_pause_abort(&snap.check_control().unwrap_err()));
        snap.request_stop();
        assert!(is_stopped(&snap.check_control().unwrap_err()));
    }

    #[test]
    fn steady_fraction_never_goes_backwards() {
        let snap = JobSnapshot::default();
        assert_eq!(snap.steady_fraction(0.5), 0.5);
        assert_eq!(
            snap.steady_fraction(0.4),
            0.5,
            "estimate grew: hold the bar"
        );
        assert!((snap.steady_fraction(0.6) - 0.6).abs() < 1e-4);
    }

    #[test]
    fn claim_is_exclusive() {
        let snap = JobSnapshot::default();
        assert!(snap.try_claim());
        assert!(!snap.try_claim());
        snap.release_claim();
        assert!(snap.try_claim());
    }

    #[test]
    fn restore_brings_unfinished_jobs_back_paused() {
        let snap = JobSnapshot::default();
        snap.restore(10, 100, DOWNLOADING, None);
        assert!(snap.is_paused());
        assert_eq!(snap.status(), PAUSED);
        assert!(snap.is_active());
        let done = JobSnapshot::default();
        done.restore(100, 100, DONE, None);
        assert!(done.is_done());
    }
}
