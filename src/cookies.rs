//! Using a browser's YouTube sign-in, for when YouTube answers "Sign in to
//! confirm you're not a bot" (or an age check).
//!
//! yt-dlp reads the browser's cookie database from its profile folder on
//! disk (`--cookies-from-browser`); no browser is opened. A `cookies.txt`
//! export (`--cookies`) is the fallback that works with any browser, for
//! when a browser's cookies cannot be read (Chromium browsers on Windows
//! encrypt them so only the browser itself can). Off unless the user picks
//! a source; the source is tested before it is saved.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where yt-dlp gets YouTube cookies from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CookieSource {
    /// A browser yt-dlp knows, by its yt-dlp name (`firefox`, `chrome`…).
    Browser { browser: String },
    /// A Netscape-format `cookies.txt` export.
    File { path: PathBuf },
}

impl CookieSource {
    pub fn label(&self) -> String {
        match self {
            CookieSource::Browser { browser } => browser_label(browser).to_string(),
            CookieSource::File { path } => format!(
                "cookies file ({})",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
        }
    }

    /// For sentences: "Firefox's YouTube sign-in", "the cookies in c.txt".
    pub fn usage(&self) -> String {
        match self {
            CookieSource::Browser { browser } => {
                format!("{}'s YouTube sign-in", browser_label(browser))
            }
            CookieSource::File { path } => format!(
                "the cookies in {}",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
        }
    }

    /// The yt-dlp arguments for this source.
    pub fn args(&self) -> Vec<OsString> {
        match self {
            CookieSource::Browser { browser } => {
                vec!["--cookies-from-browser".into(), browser.into()]
            }
            CookieSource::File { path } => vec!["--cookies".into(), path.clone().into_os_string()],
        }
    }
}

/// A browser yt-dlp can read cookies from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Browser {
    /// yt-dlp's name for it.
    pub key: &'static str,
    pub label: &'static str,
}

/// Every browser yt-dlp supports, in the order the UI lists them.
pub const BROWSERS: [Browser; 9] = [
    Browser {
        key: "firefox",
        label: "Firefox",
    },
    Browser {
        key: "chrome",
        label: "Chrome",
    },
    Browser {
        key: "edge",
        label: "Edge",
    },
    Browser {
        key: "brave",
        label: "Brave",
    },
    Browser {
        key: "opera",
        label: "Opera",
    },
    Browser {
        key: "vivaldi",
        label: "Vivaldi",
    },
    Browser {
        key: "chromium",
        label: "Chromium",
    },
    Browser {
        key: "safari",
        label: "Safari",
    },
    Browser {
        key: "whale",
        label: "Whale",
    },
];

pub fn browser_label(key: &str) -> &str {
    BROWSERS
        .iter()
        .find(|b| b.key == key)
        .map(|b| b.label)
        .unwrap_or(key)
}

pub fn is_browser(key: &str) -> bool {
    BROWSERS.iter().any(|b| b.key == key)
}

/// Profile folders where each browser keeps its cookies, for this OS.
/// Only used to list browsers that are installed; yt-dlp finds the actual
/// cookie database itself.
fn profile_dirs(key: &str) -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let local = dirs::data_local_dir().unwrap_or_default();
    let roaming = dirs::data_dir().unwrap_or_default();
    let config = dirs::config_dir().unwrap_or_default();
    let join = |base: &Path, rel: &str| base.join(rel);
    if cfg!(windows) {
        match key {
            "firefox" => vec![join(&roaming, "Mozilla/Firefox/Profiles")],
            "chrome" => vec![join(&local, "Google/Chrome/User Data")],
            "edge" => vec![join(&local, "Microsoft/Edge/User Data")],
            "brave" => vec![join(&local, "BraveSoftware/Brave-Browser/User Data")],
            "opera" => vec![join(&roaming, "Opera Software/Opera Stable")],
            "vivaldi" => vec![join(&local, "Vivaldi/User Data")],
            "chromium" => vec![join(&local, "Chromium/User Data")],
            "whale" => vec![join(&local, "Naver/Naver Whale/User Data")],
            _ => Vec::new(),
        }
    } else if cfg!(target_os = "macos") {
        let support = join(&home, "Library/Application Support");
        match key {
            "firefox" => vec![join(&support, "Firefox/Profiles")],
            "chrome" => vec![join(&support, "Google/Chrome")],
            "edge" => vec![join(&support, "Microsoft Edge")],
            "brave" => vec![join(&support, "BraveSoftware/Brave-Browser")],
            "opera" => vec![join(&support, "com.operasoftware.Opera")],
            "vivaldi" => vec![join(&support, "Vivaldi")],
            "chromium" => vec![join(&support, "Chromium")],
            "whale" => vec![join(&support, "Naver/Whale")],
            "safari" => vec![
                join(&home, "Library/Containers/com.apple.Safari"),
                join(&home, "Library/Cookies"),
            ],
            _ => Vec::new(),
        }
    } else {
        match key {
            "firefox" => vec![
                join(&home, ".mozilla/firefox"),
                join(&home, "snap/firefox/common/.mozilla/firefox"),
                join(&home, ".var/app/org.mozilla.firefox/.mozilla/firefox"),
            ],
            "chrome" => vec![join(&config, "google-chrome")],
            "edge" => vec![join(&config, "microsoft-edge")],
            "brave" => vec![join(&config, "BraveSoftware/Brave-Browser")],
            "opera" => vec![join(&config, "opera")],
            "vivaldi" => vec![join(&config, "vivaldi")],
            "chromium" => vec![
                join(&config, "chromium"),
                join(&home, "snap/chromium/common/chromium"),
            ],
            "whale" => vec![join(&config, "naver-whale")],
            _ => Vec::new(),
        }
    }
}

/// Browsers that look installed on this machine.
pub fn detect() -> Vec<Browser> {
    BROWSERS
        .into_iter()
        .filter(|browser| profile_dirs(browser.key).iter().any(|dir| dir.is_dir()))
        .collect()
}

/// A failure on `url` that a YouTube sign-in can fix. Other sites' login
/// errors don't count: the sign-in is only ever sent to YouTube.
pub fn youtube_wants_sign_in(url: &str, message: &str) -> bool {
    crate::request::is_youtube_url(url) && is_sign_in_error(message)
}

/// yt-dlp's answer when YouTube wants a signed-in session.
pub fn is_sign_in_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("sign in to confirm")
        || message.contains("not a bot")
        || message.contains("--cookies-from-browser or --cookies")
}

/// A plain-words reason why `source` did not work, from yt-dlp's error.
pub fn explain(source: &CookieSource, error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    let name = source.label();
    if let CookieSource::File { .. } = source {
        if lower.contains("netscape")
            || lower.contains("cookie file")
            || lower.contains("cookiefile")
        {
            return "That file is not a cookies.txt export. Export YouTube's cookies in Netscape (cookies.txt) format and pick that file.".to_string();
        }
        if lower.contains("no such file") || lower.contains("not found") {
            return "That cookies file could not be opened.".to_string();
        }
    }
    if lower.contains("dpapi") || lower.contains("app-bound") || lower.contains("failed to decrypt")
    {
        return format!(
            "{name} encrypts its cookies so other apps cannot read them. Use another browser you are signed in to YouTube with, or import a cookies.txt file."
        );
    }
    if (lower.contains("could not copy") && lower.contains("cookie"))
        || lower.contains("database is locked")
        || lower.contains("permission denied")
    {
        return format!("{name} is keeping its cookies locked while it runs. Close {name} completely, then try again.");
    }
    if (lower.contains("could not find") || lower.contains("no such file"))
        && (lower.contains("cookie") || lower.contains("profile"))
    {
        return format!("No {name} profile with cookies was found. Open YouTube in {name}, sign in, then try again.");
    }
    if is_sign_in_error(error) {
        return format!("YouTube still asks to sign in. Open youtube.com in {name}, sign in to your account, then try again.");
    }
    format!("Could not use {name}'s sign-in: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browser(key: &str) -> CookieSource {
        CookieSource::Browser {
            browser: key.into(),
        }
    }

    #[test]
    fn args_per_source() {
        let args: Vec<String> = browser("firefox")
            .args()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["--cookies-from-browser", "firefox"]);
        let file = CookieSource::File {
            path: PathBuf::from("c.txt"),
        };
        assert_eq!(file.args()[0], "--cookies");
        assert_eq!(file.label(), "cookies file (c.txt)");
    }

    #[test]
    fn serde_shape_is_stable() {
        let json = serde_json::to_string(&browser("edge")).unwrap();
        assert_eq!(json, r#"{"type":"browser","browser":"edge"}"#);
        let back: CookieSource = serde_json::from_str(&json).unwrap();
        assert_eq!(back, browser("edge"));
    }

    #[test]
    fn spots_the_bot_check() {
        assert!(is_sign_in_error(
            "[youtube] x: Sign in to confirm you’re not a bot. This helps protect our community. Use --cookies-from-browser or --cookies for the authentication."
        ));
        assert!(!is_sign_in_error("Video unavailable"));
        let login = "This video is only available for registered users. Use --cookies-from-browser or --cookies";
        assert!(youtube_wants_sign_in("https://youtu.be/x", login));
        assert!(!youtube_wants_sign_in("https://vimeo.com/1", login));
    }

    #[test]
    fn explains_known_failures_in_plain_words() {
        let edge = browser("edge");
        assert!(explain(&edge, "ERROR: Failed to decrypt with DPAPI. See https://github.com/yt-dlp/yt-dlp/issues/10927")
            .starts_with("Edge encrypts its cookies"));
        assert!(explain(
            &browser("chrome"),
            "ERROR: Could not copy Chrome cookie database. See ..."
        )
        .contains("Close Chrome completely"));
        assert!(explain(
            &browser("firefox"),
            "could not find firefox cookies database in C:\\x"
        )
        .starts_with("No Firefox profile"));
        assert!(
            explain(&browser("firefox"), "Sign in to confirm you're not a bot")
                .starts_with("YouTube still asks")
        );
        let file = CookieSource::File {
            path: PathBuf::from("x.txt"),
        };
        assert!(
            explain(&file, "does not look like a Netscape format cookies file")
                .contains("cookies.txt")
        );
    }

    #[test]
    fn labels_cover_every_browser() {
        for b in BROWSERS {
            assert_eq!(browser_label(b.key), b.label);
            assert!(is_browser(b.key));
        }
        assert!(!is_browser("netscape"));
    }
}
