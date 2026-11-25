//! ytd: desktop video downloader built on yt-dlp and ffmpeg.
//!
//! The engine (`session`, `ytdlp`, `tools`) is shared by the CLI (`src/main.rs`)
//! and the egui GUI (`gui`, behind the `gui` feature). Fronts only read
//! [`progress::JobSnapshot`]s; they never touch a running download directly.

pub mod config;
pub mod fsutil;
pub mod icon_raster;
pub mod jobs;
pub mod progress;
pub mod request;
pub mod session;
pub mod slots;
pub mod tools;
pub mod trim;
pub mod update;
pub mod ytdlp;

#[cfg(feature = "gui")]
pub mod gui;
#[cfg(feature = "gui")]
pub mod icon;

/// Shown in the window title and `--version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
