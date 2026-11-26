//! ytd: desktop video downloader built on yt-dlp and ffmpeg.
//!
//! The engine (`session`, `ytdlp`, `tools`) is shared by the CLI (`src/main.rs`)
//! and the egui GUI (`gui`, behind the `gui` feature). Fronts only read
//! [`progress::JobSnapshot`]s; they never touch a running download directly.

pub mod config;
pub mod cookies;
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

/// The signature shown in the GUI header, the window title and at the start
/// of the CLI: `danted4/rustYTD v4.0.1`, as in torrenter.
pub fn banner() -> String {
    format!("{} v{VERSION}", update::REPO)
}

#[cfg(test)]
mod tests {
    #[test]
    fn banner_is_repo_then_version() {
        let text = super::banner();
        assert!(text.starts_with("danted4/rustYTD v"), "{text}");
        assert!(text.ends_with(super::VERSION), "{text}");
    }
}
