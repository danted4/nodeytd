//! Settings file and per-OS paths.
//!
//! `config.json`, `jobs.json` and the `tools/` folder live in one directory:
//! `%APPDATA%\ytd` on Windows, `~/Library/Application Support/ytd` on macOS,
//! `$XDG_CONFIG_HOME/ytd` or `~/.config/ytd` elsewhere. `YTD_CONFIG_DIR`
//! overrides it (tests use this).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::fsutil::atomic_write;

static EDIT: Mutex<()> = Mutex::new(());

pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("YTD_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir).join("ytd");
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ytd")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn jobs_path() -> PathBuf {
    config_dir().join("jobs.json")
}

pub fn tools_dir() -> PathBuf {
    config_dir().join("tools")
}

/// `<Downloads>/ytd`: `%USERPROFILE%\Downloads` on Windows, `~/Downloads`
/// on macOS, the XDG download dir on Linux (home as the fallback).
pub fn default_download_dir() -> PathBuf {
    default_dir_in(
        &dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from(".")),
    )
}

/// `<base>/ytd`, or `<base>/ytd-downloads` when something that is not a
/// folder already has that name: the CLI binary saved into Downloads as
/// `ytd` would otherwise make every download fail.
fn default_dir_in(base: &Path) -> PathBuf {
    let dir = base.join("ytd");
    if dir.exists() && !dir.is_dir() {
        base.join("ytd-downloads")
    } else {
        dir
    }
}

/// One installed tool: where it is and which release it came from.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolRecord {
    pub version: String,
    pub installed_at: String,
}

pub const GLASS_TINT_MIN: u8 = 40;
pub const GLASS_TINT_MAX: u8 = 210;
/// 60% of the min..=max range (still 40–210).
pub const GLASS_TINT_DEFAULT: u8 =
    GLASS_TINT_MIN + ((GLASS_TINT_MAX - GLASS_TINT_MIN) as u16 * 60 / 100) as u8;

fn default_glass_tint() -> u8 {
    GLASS_TINT_DEFAULT
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_downloads: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Off by default: when on, the GUI updates itself and its tools.
    #[serde(default)]
    pub auto_update: bool,
    /// Where yt-dlp gets YouTube sign-in cookies from. Unset: none are sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookies: Option<crate::cookies::CookieSource>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, ToolRecord>,
    /// How heavily the theme colour sits on the macOS window blur
    /// (`GLASS_TINT_MIN..=GLASS_TINT_MAX`). Windows and Linux are opaque.
    #[serde(
        default = "default_glass_tint",
        deserialize_with = "lenient_glass_tint"
    )]
    pub glass_tint: u8,
    /// Keys a newer version wrote: kept on rewrite.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            download_dir: None,
            max_downloads: None,
            theme: None,
            auto_update: false,
            cookies: None,
            tools: BTreeMap::new(),
            glass_tint: GLASS_TINT_DEFAULT,
            extra: BTreeMap::new(),
        }
    }
}

// ------------------------------------------------- window tint (torrenter)

pub fn normalize_glass_tint(value: u8) -> u8 {
    value.clamp(GLASS_TINT_MIN, GLASS_TINT_MAX)
}

pub fn glass_tint_percent(value: u8) -> u8 {
    let value = u16::from(normalize_glass_tint(value));
    let span = u16::from(GLASS_TINT_MAX - GLASS_TINT_MIN);
    ((value - u16::from(GLASS_TINT_MIN)) * 100 / span) as u8
}

pub fn glass_tint_from_percent(percent: u8) -> u8 {
    let percent = u16::from(percent.min(100));
    let span = u16::from(GLASS_TINT_MAX - GLASS_TINT_MIN);
    GLASS_TINT_MIN + (percent * span / 100) as u8
}

/// Paint tint: native fullscreen uses 100%; otherwise the saved windowed value.
pub fn effective_glass_tint(saved: u8, fullscreen: bool) -> u8 {
    if fullscreen {
        glass_tint_from_percent(100)
    } else {
        normalize_glass_tint(saved)
    }
}

/// Windows has no frost. Ignore a leftover slider value and use 100%.
pub fn windows_glass_tint(_saved: u8) -> u8 {
    glass_tint_from_percent(100)
}

/// A hand-edited `glassTint` (300, 60.5, -1, "x") must not make the whole
/// file unreadable: clamp a number, default anything else.
fn lenient_glass_tint<'de, D>(deserializer: D) -> std::result::Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value
        .as_f64()
        .map(|number| normalize_glass_tint(number.clamp(0.0, 255.0).round() as u8))
        .unwrap_or(GLASS_TINT_DEFAULT))
}

impl AppConfig {
    pub fn download_dir(&self) -> PathBuf {
        self.download_dir
            .clone()
            .unwrap_or_else(default_download_dir)
    }
}

/// Load settings. A file that does not parse is moved to `config.json.bak`
/// and defaults are used, so a bad edit never bricks the app.
pub fn load_config() -> AppConfig {
    let path = config_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return AppConfig::default();
    };
    match serde_json::from_str(&text) {
        Ok(config) => config,
        Err(_) => {
            let _ = std::fs::rename(&path, path.with_extension("json.bak"));
            AppConfig::default()
        }
    }
}

pub fn save_config(config: &AppConfig) -> Result<()> {
    let json = serde_json::to_vec_pretty(config).context("serialize config")?;
    atomic_write(&config_path(), &json)
}

/// Load, change, save under one lock so two edits never drop each other.
pub fn edit_config(change: impl FnOnce(&mut AppConfig)) -> Result<AppConfig> {
    let _guard = EDIT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut config = load_config();
    change(&mut config);
    save_config(&config)?;
    Ok(config)
}

#[cfg(test)]
pub(crate) mod test_env {
    use std::sync::{Mutex, MutexGuard};

    static ENV: Mutex<()> = Mutex::new(());

    /// Points `YTD_CONFIG_DIR` at a temp dir for one test at a time.
    pub struct TempConfig {
        pub dir: tempfile::TempDir,
        _guard: MutexGuard<'static, ()>,
    }

    pub fn temp_config() -> TempConfig {
        let guard = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("YTD_CONFIG_DIR", dir.path());
        TempConfig { dir, _guard: guard }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glass_tint_clamps_and_defaults() {
        assert_eq!(normalize_glass_tint(0), GLASS_TINT_MIN);
        assert_eq!(normalize_glass_tint(255), GLASS_TINT_MAX);
        assert_eq!(glass_tint_from_percent(0), GLASS_TINT_MIN);
        assert_eq!(glass_tint_from_percent(100), GLASS_TINT_MAX);
        assert_eq!(glass_tint_from_percent(60), GLASS_TINT_DEFAULT);
        assert_eq!(glass_tint_percent(GLASS_TINT_DEFAULT), 60);
        let parsed: AppConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.glass_tint, GLASS_TINT_DEFAULT);
        assert_eq!(AppConfig::default().glass_tint, GLASS_TINT_DEFAULT);
        let config = AppConfig {
            glass_tint: 90,
            ..AppConfig::default()
        };
        let parsed: AppConfig =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(parsed.glass_tint, 90);
    }

    #[test]
    fn fullscreen_uses_max_tint_without_changing_saved() {
        let saved = glass_tint_from_percent(60);
        assert_eq!(effective_glass_tint(saved, true), GLASS_TINT_MAX);
        assert_eq!(effective_glass_tint(saved, false), saved);
        assert_eq!(saved, GLASS_TINT_DEFAULT);
    }

    #[test]
    fn odd_glass_tint_values_do_not_reset_the_file() {
        for (raw, want) in [
            ("300", GLASS_TINT_MAX),
            ("-1", GLASS_TINT_MIN),
            ("60.5", 61),
            ("100", 100),
            ("\"x\"", GLASS_TINT_DEFAULT),
            ("null", GLASS_TINT_DEFAULT),
        ] {
            let parsed: AppConfig =
                serde_json::from_str(&format!(r#"{{"glassTint":{raw},"theme":"ice"}}"#))
                    .unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(parsed.glass_tint, want, "{raw}");
            assert_eq!(parsed.theme.as_deref(), Some("ice"), "other keys survive");
        }
    }

    #[test]
    fn windows_tint_snaps_to_max_regardless_of_saved() {
        assert_eq!(windows_glass_tint(0), GLASS_TINT_MAX);
        assert_eq!(windows_glass_tint(GLASS_TINT_DEFAULT), GLASS_TINT_MAX);
        assert_eq!(glass_tint_percent(windows_glass_tint(90)), 100);
    }

    #[test]
    fn default_download_folder_is_named_ytd() {
        let base = tempfile::tempdir().unwrap();
        assert_eq!(default_dir_in(base.path()), base.path().join("ytd"));
        std::fs::create_dir(base.path().join("ytd")).unwrap();
        assert_eq!(default_dir_in(base.path()), base.path().join("ytd"));
    }

    #[test]
    fn default_download_folder_avoids_a_file_named_ytd() {
        let base = tempfile::tempdir().unwrap();
        std::fs::write(base.path().join("ytd"), b"binary").unwrap();
        assert_eq!(
            default_dir_in(base.path()),
            base.path().join("ytd-downloads")
        );
    }

    #[test]
    fn round_trip_keeps_unknown_keys() {
        let env = test_env::temp_config();
        std::fs::write(
            env.dir.path().join("config.json"),
            r#"{"maxDownloads":3,"futureKey":{"a":1}}"#,
        )
        .unwrap();
        let config = edit_config(|config| config.theme = Some("ice".into())).unwrap();
        assert_eq!(config.max_downloads, Some(3));
        let text = std::fs::read_to_string(config_path()).unwrap();
        assert!(text.contains("futureKey"));
        assert!(text.contains("\"theme\": \"ice\""));
    }

    #[test]
    fn corrupt_config_is_moved_aside() {
        let env = test_env::temp_config();
        std::fs::write(env.dir.path().join("config.json"), "{not json").unwrap();
        let config = load_config();
        assert!(config.theme.is_none());
        assert!(env.dir.path().join("config.json.bak").exists());
    }
}
