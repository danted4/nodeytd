//! Settings file and per-OS paths.
//!
//! `config.json`, `jobs.json` and the `tools/` folder live in one directory:
//! `%APPDATA%\ytd` on Windows, `~/Library/Application Support/ytd` on macOS,
//! `$XDG_CONFIG_HOME/ytd` or `~/.config/ytd` elsewhere. `YTD_CONFIG_DIR`
//! overrides it (tests use this).

use std::collections::BTreeMap;
use std::path::PathBuf;
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

/// `<Downloads>/rustYTD`: `%USERPROFILE%\Downloads` on Windows, `~/Downloads`
/// on macOS, the XDG download dir on Linux (home as the fallback).
pub fn default_download_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rustYTD")
}

/// One installed tool: where it is and which release it came from.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolRecord {
    pub version: String,
    pub installed_at: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
    /// Keys a newer version wrote: kept on rewrite.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
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
