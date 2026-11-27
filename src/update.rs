//! App self-update from this repo's GitHub Releases (the "auto-update"
//! toggle). Same rules as the tool installer: the asset URL must be under
//! `https://github.com/<repo>/releases/download/`, its SHA-256 must match the
//! release's `SHA256SUMS`, and the size is capped.
//!
//! The running executable is replaced in place; the new version runs from
//! the next launch (or [`relaunch`]). On Windows the running exe cannot be
//! overwritten, so it is renamed to `<exe>.old` first and deleted on the
//! next start. Inside a macOS `.app` the binary in `Contents/MacOS` is
//! replaced too, then the bundle's version is stamped and the bundle is
//! ad-hoc signed again, as torrenter does.

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
        return result;
    }
    refresh_installed_app(exe, &update.tag).with_context(|| {
        format!(
            "ytd {} is installed, but ytd.app could not be signed again; if it does not open, \
             download the .app.zip from github.com/{REPO}/releases",
            update.tag
        )
    })
}

/// Whether this install can replace its own binary: not a quarantined
/// `.app` that macOS runs from a read-only translocated copy, and its
/// folder is writable (a non-admin user, a read-only volume or a system
/// folder is not). When it can't, a new version is only announced.
pub fn can_self_replace() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    if exe.to_string_lossy().contains("/AppTranslocation/") {
        return false;
    }
    let Some(dir) = exe.parent() else {
        return false;
    };
    let probe = dir.join(format!(".ytd-write-test-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// `ytd.app`'s `Info.plist`: written by `macos-app` at release time and
/// re-stamped by the updater.
pub fn app_info_plist(version: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>ytd</string>
	<key>CFBundleExecutable</key>
	<string>ytd-gui</string>
	<key>CFBundleIconFile</key>
	<string>AppIcon</string>
	<key>CFBundleIdentifier</key>
	<string>com.danted4.ytd</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>ytd</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>{version}</string>
	<key>CFBundleVersion</key>
	<string>{version}</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.utilities</string>
	<key>LSMinimumSystemVersion</key>
	<string>12.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
"#
    )
}

/// `/Applications/ytd.app` for `/Applications/ytd.app/Contents/MacOS/ytd-gui`.
fn enclosing_app_bundle(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    if macos.file_name()?.to_str()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()?.to_str()? != "Contents" {
        return None;
    }
    let app = contents.parent()?;
    app.file_name()?
        .to_str()?
        .ends_with(".app")
        .then(|| app.to_path_buf())
}

fn version_from_tag(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

fn replace_plist_string(xml: &str, key: &str, value: &str) -> Result<String> {
    let header = format!("<key>{key}</key>");
    let Some(key_at) = xml.find(&header) else {
        bail!("Info.plist has no {key}");
    };
    let after_key = key_at + header.len();
    let rest = &xml[after_key..];
    let Some(string_at) = rest.find("<string>") else {
        bail!("Info.plist {key} has no string value");
    };
    if string_at > 80 {
        bail!("Info.plist {key} is not followed by a string");
    }
    let value_at = after_key + string_at + "<string>".len();
    let Some(end) = xml[value_at..].find("</string>") else {
        bail!("Info.plist {key} string is unclosed");
    };
    Ok(format!(
        "{}{value}{}",
        &xml[..value_at],
        &xml[value_at + end..]
    ))
}

/// Finder's "version" for the bundle follows the binary that was put in.
fn stamp_app_bundle(app: &Path, tag: &str) -> Result<()> {
    let version = version_from_tag(tag);
    let plist_path = app.join("Contents/Info.plist");
    let xml = std::fs::read_to_string(&plist_path)
        .with_context(|| format!("read {}", plist_path.display()))?;
    let xml = replace_plist_string(&xml, "CFBundleShortVersionString", version)?;
    let xml = replace_plist_string(&xml, "CFBundleVersion", version)?;
    std::fs::write(&plist_path, xml).with_context(|| format!("write {}", plist_path.display()))?;
    Ok(())
}

/// After the binary inside a `.app` is replaced, stamp the new version and
/// sign the bundle ad hoc again (as `macos-app` builds it): until then its
/// seal no longer matches its contents, so a failure is reported.
fn refresh_installed_app(exe: &Path, tag: &str) -> Result<()> {
    let Some(app) = enclosing_app_bundle(exe) else {
        return Ok(());
    };
    stamp_app_bundle(&app, tag)?;
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("codesign")
            .args(["--force", "--deep", "-s", "-"])
            .arg(&app)
            .status()
            .context("could not run codesign")?;
        if !status.success() {
            bail!("codesign failed ({status})");
        }
    }
    Ok(())
}

/// On Linux, `current_exe` of a binary that was renamed over reads
/// `/path/ytd-gui (deleted)`.
fn without_linux_deleted_marker(path: &Path) -> PathBuf {
    const MARKER: &str = " (deleted)";
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return path.to_path_buf();
    };
    let Some(stripped) = name.strip_suffix(MARKER) else {
        return path.to_path_buf();
    };
    let mut out = path.to_path_buf();
    out.set_file_name(stripped);
    out
}

/// Start the installed (new) binary as a new process, then exit this one.
/// The caller stops downloads and saves state first. Never exec(): the new
/// image would inherit this window's GL, display and AppKit state (and on
/// Linux often a `(deleted)` path). A `.app` is opened through Launch
/// Services (`open -n`) so it gets its own Dock icon and activation.
/// Returns only on failure.
pub fn relaunch() -> Result<()> {
    let exe = std::env::current_exe().context("cannot find the running executable")?;
    let exe = without_linux_deleted_marker(&exe);
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    match enclosing_app_bundle(&exe).filter(|_| cfg!(target_os = "macos")) {
        Some(app) => {
            let mut open = std::process::Command::new("open");
            open.arg("-n").arg(&app);
            if !args.is_empty() {
                open.arg("--args").args(&args);
            }
            let status = open.status().context("could not run open")?;
            if !status.success() {
                bail!("could not open {} ({status})", app.display());
            }
        }
        None => {
            std::process::Command::new(&exe)
                .args(args)
                .spawn()
                .with_context(|| format!("failed to relaunch {}", exe.display()))?;
        }
    }
    std::process::exit(0);
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

/// Delete the `<exe>.old` a Windows update left behind. After "restart now"
/// the old process may still be exiting and its image locked, so a failed
/// delete is retried in the background for a few seconds.
pub fn cleanup_replaced_binary() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = std::fs::remove_file(sibling(&exe, ".update"));
    let old = sibling(&exe, ".old");
    if !old.exists() || std::fs::remove_file(&old).is_ok() {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if !old.exists() || std::fs::remove_file(&old).is_ok() {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn app_bundle_is_the_shell_around_macos_binary() {
        let exe = PathBuf::from("/Applications/ytd.app/Contents/MacOS/ytd-gui");
        assert_eq!(
            enclosing_app_bundle(&exe).as_deref(),
            Some(Path::new("/Applications/ytd.app"))
        );
        assert!(enclosing_app_bundle(Path::new("/tmp/ytd-gui")).is_none());
        assert!(enclosing_app_bundle(Path::new("/opt/ytd/Contents/MacOS/ytd-gui")).is_none());
    }

    #[test]
    fn plist_version_strings_are_replaced() {
        let xml = r#"<key>CFBundleShortVersionString</key>
	<string>4.1.1</string>
	<key>CFBundleVersion</key>
	<string>4.1.1</string>"#;
        let xml = replace_plist_string(xml, "CFBundleShortVersionString", "4.1.2").unwrap();
        let xml = replace_plist_string(&xml, "CFBundleVersion", "4.1.2").unwrap();
        assert!(xml.contains("<string>4.1.2</string>"));
        assert!(!xml.contains("4.1.1"));
        assert_eq!(version_from_tag("v4.1.2"), "4.1.2");
        assert!(replace_plist_string(&xml, "CFBundleName", "x").is_err());
    }

    #[test]
    fn stamping_writes_the_bundle_version() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("ytd.app");
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        std::fs::write(app.join("Contents/Info.plist"), app_info_plist("4.1.1")).unwrap();
        stamp_app_bundle(&app, "v4.1.2").unwrap();
        let xml = std::fs::read_to_string(app.join("Contents/Info.plist")).unwrap();
        assert_eq!(xml.matches("<string>4.1.2</string>").count(), 2);
        assert!(xml.contains("<string>ytd</string>"));
    }

    /// A bundle that can't be stamped is reported, not skipped: its seal
    /// would no longer match the new binary. Outside a bundle nothing runs.
    #[test]
    fn a_failed_bundle_refresh_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("ytd.app/Contents/MacOS/ytd-gui");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        // No Info.plist.
        assert!(refresh_installed_app(&exe, "v4.1.2").is_err());
        assert!(refresh_installed_app(&dir.path().join("ytd-gui"), "v4.1.2").is_ok());
    }

    #[test]
    fn linux_deleted_marker_is_dropped() {
        assert_eq!(
            without_linux_deleted_marker(Path::new("/opt/ytd/ytd-gui (deleted)")),
            PathBuf::from("/opt/ytd/ytd-gui")
        );
        assert_eq!(
            without_linux_deleted_marker(Path::new("/opt/ytd/ytd-gui")),
            PathBuf::from("/opt/ytd/ytd-gui")
        );
    }

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
