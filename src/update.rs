//! App self-update from this repo's GitHub Releases (the "auto-update"
//! toggle). Same rules as the tool installer: the asset URL must be under
//! `https://github.com/<repo>/releases/download/`, its SHA-256 must match the
//! release's `SHA256SUMS`, and the size is capped.
//!
//! The running executable is replaced in place; the new version runs from
//! the next launch. On Windows the running exe cannot be overwritten, so it
//! is renamed to `<exe>.old` first and deleted on the next start.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::progress::{ProgressHandle, VERIFYING};
use crate::tools::{self, Github};

/// Renamed from `danted4/rustYTD` in 4.1.0 (and `nodeytd` before that).
/// GitHub redirects the old names' API, but an older build's download pin
/// still names its own repo, so 4.0.x has to be updated by hand once.
pub const REPO: &str = "danted4/ytd";
const MAX_BYTES: u64 = 120 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppKind {
    Cli,
    Gui,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub tag: String,
    asset_url: String,
    sums_url: String,
}

fn target_triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

/// The release asset this build updates from, e.g.
/// `ytd-gui-x86_64-pc-windows-msvc.exe`.
pub fn asset_name(kind: AppKind) -> Option<String> {
    let stem = match kind {
        AppKind::Cli => "ytd",
        AppKind::Gui => "ytd-gui",
    };
    let ext = if cfg!(windows) { ".exe" } else { "" };
    target_triple().map(|target| format!("{stem}-{target}{ext}"))
}

/// Inside a macOS `.app` the bundle is signed as a whole: replacing the
/// binary would break it, so there the update is only announced.
pub fn can_self_replace() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    !exe.to_string_lossy().contains(".app/Contents/MacOS/")
}

fn parse_version(raw: &str) -> Option<(u64, u64, u64)> {
    let raw = raw.trim().trim_start_matches('v');
    let mut parts = raw.split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

fn is_newer(tag: &str, current: &str) -> bool {
    match (parse_version(tag), parse_version(current)) {
        (Some(tag), Some(current)) => tag > current,
        _ => false,
    }
}

/// A newer release with an asset for this build, if there is one.
pub async fn check(kind: AppKind) -> Result<Option<AvailableUpdate>> {
    check_with(
        &tools::http_client()?,
        &Github::default(),
        kind,
        crate::VERSION,
    )
    .await
}

async fn check_with(
    client: &reqwest::Client,
    github: &Github,
    kind: AppKind,
    current: &str,
) -> Result<Option<AvailableUpdate>> {
    let Some(name) = asset_name(kind) else {
        return Ok(None);
    };
    let release = tools::latest_release(client, github, REPO).await?;
    if !is_newer(&release.tag_name, current) {
        return Ok(None);
    }
    let (Some(asset_url), Some(sums_url)) =
        (release.asset_url(&name), release.asset_url("SHA256SUMS"))
    else {
        return Ok(None);
    };
    Ok(Some(AvailableUpdate {
        tag: release.tag_name.clone(),
        asset_url: asset_url.to_string(),
        sums_url: sums_url.to_string(),
    }))
}

/// Download, verify and swap in the update. Takes effect on next launch.
pub async fn install(
    update: &AvailableUpdate,
    kind: AppKind,
    progress: &ProgressHandle,
) -> Result<()> {
    let exe = std::env::current_exe().context("cannot find the running executable")?;
    install_to(
        &tools::http_client()?,
        &Github::default(),
        update,
        kind,
        &exe,
        progress,
    )
    .await
}

async fn install_to(
    client: &reqwest::Client,
    github: &Github,
    update: &AvailableUpdate,
    kind: AppKind,
    exe: &Path,
    progress: &ProgressHandle,
) -> Result<()> {
    let name = asset_name(kind).context("no release build for this platform")?;
    tools::pinned(github, REPO, &update.asset_url)?;
    tools::pinned(github, REPO, &update.sums_url)?;
    let sums = tools::fetch_text(client, &update.sums_url, 64 * 1024).await?;
    let expected = tools::parse_sums(&sums, &name)
        .with_context(|| format!("no checksum for {name} in SHA256SUMS"))?;
    let staged = sibling(exe, ".update");
    let result = async {
        tools::download_to(client, &update.asset_url, &staged, MAX_BYTES, progress).await?;
        progress.set_status(VERIFYING);
        let actual = tools::sha256_file(&staged).await?;
        if !actual.eq_ignore_ascii_case(&expected) {
            bail!("checksum mismatch for {name}");
        }
        replace_executable(&staged, exe)
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    exe.with_file_name(name)
}

fn replace_executable(staged: &Path, exe: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(staged, std::fs::Permissions::from_mode(0o755))?;
    }
    if cfg!(windows) {
        let old = sibling(exe, ".old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old).context("failed to move the running exe aside")?;
        if let Err(error) = std::fs::rename(staged, exe) {
            let _ = std::fs::rename(&old, exe);
            return Err(error).context("failed to install the update");
        }
    } else {
        std::fs::rename(staged, exe).context("failed to install the update")?;
    }
    Ok(())
}

/// Delete the `<exe>.old` a Windows update left behind.
pub fn cleanup_replaced_binary() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(sibling(&exe, ".old"));
        let _ = std::fs::remove_file(sibling(&exe, ".update"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_semver() {
        assert!(is_newer("v4.0.1", "4.0.0"));
        assert!(is_newer("v4.10.0", "4.9.9"));
        assert!(!is_newer("v4.0.0", "4.0.0"));
        assert!(!is_newer("v3.9.0", "4.0.0"));
        assert!(!is_newer("nightly", "4.0.0"));
        assert!(
            !is_newer("v4.0.0-rc.1", "3.0.0"),
            "pre-releases are never offered"
        );
    }

    #[test]
    fn asset_names_match_the_release_workflow() {
        if let Some(name) = asset_name(AppKind::Gui) {
            assert!(name.starts_with("ytd-gui-"));
            assert_eq!(name.ends_with(".exe"), cfg!(windows));
        }
    }

    #[test]
    fn replace_swaps_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("ytd-gui.exe");
        let staged = dir.path().join("ytd-gui.exe.update");
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(&staged, b"new").unwrap();
        replace_executable(&staged, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert!(!staged.exists());
    }
}
