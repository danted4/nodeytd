//! The external programs ytd drives: yt-dlp (extraction and download),
//! ffmpeg/ffprobe (merge, mp3, trim) and deno (the JS runtime yt-dlp needs to
//! solve YouTube's player).
//!
//! Lookup order: our `tools/` folder, then PATH. Missing tools are installed
//! from their publishers' GitHub releases only: the download URL must sit
//! under `https://github.com/<repo>/releases/download/`, the size is capped,
//! and the SHA-256 from the release's checksum file must match before the
//! file is moved into place.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::config::{self, ToolRecord};
use crate::progress::{ProgressHandle, DOWNLOADING, EXTRACTING, VERIFYING};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tool {
    YtDlp,
    Ffmpeg,
    Deno,
}

impl Tool {
    pub const ALL: [Tool; 3] = [Tool::YtDlp, Tool::Ffmpeg, Tool::Deno];

    pub fn key(self) -> &'static str {
        match self {
            Tool::YtDlp => "yt-dlp",
            Tool::Ffmpeg => "ffmpeg",
            Tool::Deno => "deno",
        }
    }

    pub fn exe_name(self) -> String {
        exe(self.key())
    }
}

fn exe(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// Where each tool is. `deno` is optional: without it yt-dlp still works for
/// some formats, so ytd warns instead of refusing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolPaths {
    pub ytdlp: PathBuf,
    pub ffmpeg: PathBuf,
    pub deno: Option<PathBuf>,
}

impl ToolPaths {
    /// yt-dlp's `--ffmpeg-location` takes the folder holding ffmpeg and ffprobe.
    pub fn ffmpeg_dir(&self) -> PathBuf {
        self.ffmpeg
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.ffmpeg.clone())
    }
}

/// Find an executable on PATH.
pub fn which(exe_name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe_name))
        .find(|candidate| candidate.is_file())
}

pub fn locate(tool: Tool) -> Option<PathBuf> {
    let local = config::tools_dir().join(tool.exe_name());
    if local.is_file() {
        return Some(local);
    }
    which(&tool.exe_name())
}

/// Tools that are not installed anywhere we look.
pub fn missing() -> Vec<Tool> {
    Tool::ALL
        .into_iter()
        .filter(|tool| locate(*tool).is_none())
        .collect()
}

/// All tool paths, or an error naming what is missing.
pub fn resolve() -> Result<ToolPaths> {
    let ytdlp = locate(Tool::YtDlp);
    let ffmpeg = locate(Tool::Ffmpeg);
    match (ytdlp, ffmpeg) {
        (Some(ytdlp), Some(ffmpeg)) => Ok(ToolPaths {
            ytdlp,
            ffmpeg,
            deno: locate(Tool::Deno),
        }),
        (ytdlp, ffmpeg) => {
            let mut names = Vec::new();
            if ytdlp.is_none() {
                names.push("yt-dlp");
            }
            if ffmpeg.is_none() {
                names.push("ffmpeg");
            }
            bail!(
                "missing tools: {} (run `ytd tools install`)",
                names.join(", ")
            )
        }
    }
}

/// Shown when ffmpeg cannot be installed for this platform.
pub const MACOS_FFMPEG_HINT: &str =
    "ffmpeg is not auto-installed on macOS: run `brew install ffmpeg`, then retry";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Archive {
    Raw,
    Zip,
    TarXz,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Sums {
    /// One checksum file for the whole release.
    Combined(&'static str),
    /// `<asset>.sha256sum` beside the asset.
    Sidecar,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Plan {
    repo: &'static str,
    asset: String,
    sums: Sums,
    archive: Archive,
    /// Files to keep, by file name. `Raw` installs the asset as `members[0]`.
    members: Vec<String>,
    max_bytes: u64,
}

const MB: u64 = 1024 * 1024;

fn plan_for(tool: Tool, os: &str, arch: &str) -> Option<Plan> {
    let arm = arch == "aarch64";
    match tool {
        Tool::YtDlp => {
            let asset = match (os, arm) {
                ("windows", false) => "yt-dlp.exe",
                ("windows", true) => "yt-dlp_arm64.exe",
                ("macos", _) => "yt-dlp_macos",
                ("linux", false) => "yt-dlp_linux",
                ("linux", true) => "yt-dlp_linux_aarch64",
                _ => return None,
            };
            Some(Plan {
                repo: "yt-dlp/yt-dlp",
                asset: asset.to_string(),
                sums: Sums::Combined("SHA2-256SUMS"),
                archive: Archive::Raw,
                members: vec![exe_for(os, "yt-dlp")],
                max_bytes: 200 * MB,
            })
        }
        Tool::Ffmpeg => {
            let (asset, archive) = match (os, arm) {
                ("windows", false) => ("ffmpeg-master-latest-win64-gpl.zip", Archive::Zip),
                ("windows", true) => ("ffmpeg-master-latest-winarm64-gpl.zip", Archive::Zip),
                ("linux", false) => ("ffmpeg-master-latest-linux64-gpl.tar.xz", Archive::TarXz),
                ("linux", true) => ("ffmpeg-master-latest-linuxarm64-gpl.tar.xz", Archive::TarXz),
                _ => return None,
            };
            Some(Plan {
                repo: "FFmpeg-Builds/FFmpeg-Builds",
                asset: asset.to_string(),
                sums: Sums::Combined("checksums.sha256"),
                archive,
                members: vec![exe_for(os, "ffmpeg"), exe_for(os, "ffprobe")],
                max_bytes: 600 * MB,
            })
        }
        Tool::Deno => {
            let target = match (os, arm) {
                ("windows", false) => "x86_64-pc-windows-msvc",
                ("windows", true) => "aarch64-pc-windows-msvc",
                ("macos", false) => "x86_64-apple-darwin",
                ("macos", true) => "aarch64-apple-darwin",
                ("linux", false) => "x86_64-unknown-linux-gnu",
                ("linux", true) => "aarch64-unknown-linux-gnu",
                _ => return None,
            };
            Some(Plan {
                repo: "denoland/deno",
                asset: format!("deno-{target}.zip"),
                sums: Sums::Sidecar,
                archive: Archive::Zip,
                members: vec![exe_for(os, "deno")],
                max_bytes: 300 * MB,
            })
        }
    }
}

fn exe_for(os: &str, stem: &str) -> String {
    if os == "windows" {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

fn plan(tool: Tool) -> Option<Plan> {
    plan_for(tool, std::env::consts::OS, std::env::consts::ARCH)
}

/// Whether ytd can install this tool on this machine.
pub fn can_install(tool: Tool) -> bool {
    plan(tool).is_some()
}

/// Where release metadata and downloads come from. Tests point both at a
/// local server; production is always GitHub.
#[derive(Clone, Debug)]
pub struct Github {
    pub api: String,
    pub web: String,
}

impl Default for Github {
    fn default() -> Self {
        Self {
            api: "https://api.github.com".to_string(),
            web: "https://github.com".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct Release {
    pub(crate) tag_name: String,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// `GET /repos/<repo>/releases/latest`.
pub(crate) async fn latest_release(
    client: &reqwest::Client,
    github: &Github,
    repo: &str,
) -> Result<Release> {
    client
        .get(format!("{}/repos/{}/releases/latest", github.api, repo))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("failed to read the latest {repo} release"))?
        .json()
        .await
        .context("unexpected release metadata")
}

/// Tools ytd installed whose publisher has a newer release than the one
/// recorded in `config.json`.
pub async fn outdated() -> Result<Vec<Tool>> {
    let client = http_client()?;
    let github = Github::default();
    let config = config::load_config();
    let mut stale = Vec::new();
    for tool in managed() {
        let (Some(plan), Some(record)) = (plan(tool), config.tools.get(tool.key())) else {
            continue;
        };
        let release = latest_release(&client, &github, plan.repo).await?;
        if release.version() != record.version {
            stale.push(tool);
        }
    }
    Ok(stale)
}

impl Release {
    pub(crate) fn asset_url(&self, name: &str) -> Option<&str> {
        self.assets
            .iter()
            .find(|asset| asset.name == name)
            .map(|asset| asset.browser_download_url.as_str())
    }

    /// The rolling `latest` tag of the ffmpeg builds: use the publish date.
    fn version(&self) -> String {
        if self.tag_name == "latest" {
            if let Some(date) = self.published_at.as_deref().and_then(|d| d.get(..10)) {
                return date.to_string();
            }
        }
        self.tag_name.clone()
    }
}

/// rustls only, bounded timeouts, and no redirect from https to http.
pub fn http_client() -> Result<reqwest::Client> {
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        let downgrade = attempt
            .previous()
            .last()
            .is_some_and(|prev| prev.scheme() == "https")
            && attempt.url().scheme() == "http";
        if downgrade {
            attempt.error("refusing https to http redirect")
        } else {
            attempt.follow()
        }
    });
    reqwest::Client::builder()
        .use_rustls_tls()
        .user_agent(format!("ytd/{}", crate::VERSION))
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(60))
        .redirect(policy)
        .build()
        .context("failed to build HTTP client")
}

/// Install (or update) one tool. Returns the release version installed.
pub async fn install(tool: Tool, progress: &ProgressHandle) -> Result<String> {
    let Some(plan) = plan(tool) else {
        if tool == Tool::Ffmpeg && cfg!(target_os = "macos") {
            bail!(MACOS_FFMPEG_HINT);
        }
        bail!("{} cannot be auto-installed on this platform", tool.key());
    };
    let client = http_client()?;
    let version = install_with(
        &client,
        &Github::default(),
        &plan,
        &config::tools_dir(),
        progress,
    )
    .await
    .with_context(|| format!("installing {}", tool.key()))?;
    let installed_at = chrono::Local::now().format("%Y-%m-%d").to_string();
    let recorded = version.clone();
    config::edit_config(move |config| {
        config.tools.insert(
            tool.key().to_string(),
            ToolRecord {
                version: recorded,
                installed_at,
            },
        );
    })?;
    Ok(version)
}

/// Install every missing tool, in order. Tools we cannot install here (ffmpeg
/// on macOS) fail with a hint; the others still install.
pub async fn install_missing(progress_for: impl Fn(Tool) -> ProgressHandle) -> Result<ToolPaths> {
    let mut errors = Vec::new();
    for tool in missing() {
        let progress = progress_for(tool);
        match install(tool, &progress).await {
            Ok(_) => progress.finish(),
            Err(error) => {
                progress.abandon();
                errors.push(format!("{error:#}"));
            }
        }
    }
    match resolve() {
        Ok(paths) => Ok(paths),
        Err(_) if !errors.is_empty() => bail!("{}", errors.join("; ")),
        Err(error) => Err(error),
    }
}

/// The tools [`update_all`] touches: ours or missing, and installable here.
pub fn managed() -> Vec<Tool> {
    Tool::ALL
        .into_iter()
        .filter(|tool| {
            let ours = config::tools_dir().join(tool.exe_name()).is_file();
            (ours || locate(*tool).is_none()) && can_install(*tool)
        })
        .collect()
}

/// Re-install every tool ytd manages (those in our `tools/` folder, plus any
/// that are missing). Tools found on PATH are the user's to update.
pub async fn update_all(progress_for: impl Fn(Tool) -> ProgressHandle) -> Result<ToolPaths> {
    let mut errors = Vec::new();
    for tool in managed() {
        let progress = progress_for(tool);
        match install(tool, &progress).await {
            Ok(_) => progress.finish(),
            Err(error) => {
                progress.abandon();
                errors.push(format!("{error:#}"));
            }
        }
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    resolve()
}

pub(crate) fn pinned(github: &Github, repo: &str, url: &str) -> Result<()> {
    let prefix = format!("{}/{}/releases/download/", github.web, repo);
    if !url.starts_with(&prefix) {
        bail!("refusing download from unexpected host: {url}");
    }
    Ok(())
}

async fn install_with(
    client: &reqwest::Client,
    github: &Github,
    plan: &Plan,
    dir: &Path,
    progress: &ProgressHandle,
) -> Result<String> {
    progress.set_status(DOWNLOADING);
    let release = latest_release(client, github, plan.repo).await?;
    let asset_url = release
        .asset_url(&plan.asset)
        .with_context(|| format!("release {} has no {}", release.tag_name, plan.asset))?
        .to_string();
    pinned(github, plan.repo, &asset_url)?;
    let sums_name = match &plan.sums {
        Sums::Combined(name) => name.to_string(),
        Sums::Sidecar => format!("{}.sha256sum", plan.asset),
    };
    let sums_url = release
        .asset_url(&sums_name)
        .with_context(|| format!("release {} has no {sums_name}", release.tag_name))?
        .to_string();
    pinned(github, plan.repo, &sums_url)?;
    let sums = fetch_text(client, &sums_url, MB).await?;
    let expected = parse_sums(&sums, &plan.asset)
        .with_context(|| format!("no checksum for {} in {sums_name}", plan.asset))?;

    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("failed to create {}", dir.display()))?;
    let download = dir.join(format!(".download-{}", plan.asset));
    let result = async {
        download_to(client, &asset_url, &download, plan.max_bytes, progress).await?;
        progress.set_status(VERIFYING);
        let actual = sha256_file(&download).await?;
        if !actual.eq_ignore_ascii_case(&expected) {
            bail!("checksum mismatch for {}", plan.asset);
        }
        progress.set_status(EXTRACTING);
        let plan = plan.clone();
        let dir = dir.to_path_buf();
        let download = download.clone();
        tokio::task::spawn_blocking(move || place(&plan, &download, &dir))
            .await
            .context("extract task failed")??;
        Ok(())
    }
    .await;
    let _ = tokio::fs::remove_file(&download).await;
    result?;
    Ok(release.version())
}

pub(crate) async fn fetch_text(client: &reqwest::Client, url: &str, max: u64) -> Result<String> {
    let response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("failed to download {url}"))?;
    let bytes = response.bytes().await.context("failed to read checksums")?;
    if bytes.len() as u64 > max {
        bail!("checksum file too large");
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Stream `url` into `dest` with a size cap, retrying transient failures.
pub(crate) async fn download_to(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    max: u64,
    progress: &ProgressHandle,
) -> Result<()> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match download_once(client, url, dest, max, progress).await {
            Ok(()) => return Ok(()),
            Err(error) if attempt < 4 && is_transient(&error) => {
                tokio::time::sleep(Duration::from_millis(400 * (1 << attempt))).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_transient(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<reqwest::Error>().is_some_and(|e| {
            e.is_timeout()
                || e.is_connect()
                || e.status()
                    .is_some_and(|s| matches!(s.as_u16(), 429 | 502 | 503 | 504))
        })
    })
}

async fn download_once(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    max: u64,
    progress: &ProgressHandle,
) -> Result<()> {
    let response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("failed to download {url}"))?;
    let total = response.content_length().unwrap_or(0);
    if total > max {
        bail!("download is larger than the {} MB cap", max / MB);
    }
    progress.set_length(total);
    progress.set_position(0);
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("failed to create {}", dest.display()))?;
    let mut stream = response.bytes_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        progress.check_control()?;
        let chunk = chunk.context("download interrupted")?;
        written += chunk.len() as u64;
        if written > max {
            bail!("download is larger than the {} MB cap", max / MB);
        }
        file.write_all(&chunk)
            .await
            .with_context(|| format!("failed to write {}", dest.display()))?;
        progress.set_position(written);
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}

pub(crate) async fn sha256_file(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<String> {
        use std::io::Read;
        let mut file = std::fs::File::open(&path)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    })
    .await
    .context("hash task failed")?
}

fn is_sha256(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The SHA-256 for `name` in a checksum file. Accepts GNU `hash  name` /
/// `hash *name` lines, and a file holding one bare hash (deno's Windows
/// sidecars are PowerShell `Get-FileHash` output).
pub(crate) fn parse_sums(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        let mut tokens = line.split_whitespace();
        let (Some(hash), Some(file)) = (tokens.next(), tokens.next()) else {
            continue;
        };
        let file = file.trim_start_matches('*');
        let file = file.rsplit(['/', '\\']).next().unwrap_or(file);
        if is_sha256(hash) && file == name {
            return Some(hash.to_ascii_lowercase());
        }
    }
    let hashes: Vec<&str> = text
        .split(|ch: char| !ch.is_ascii_hexdigit())
        .filter(|token| is_sha256(token))
        .collect();
    match hashes.as_slice() {
        [only] => Some(only.to_ascii_lowercase()),
        _ => None,
    }
}

/// Move the verified download into `dir`: as-is, or the wanted members of
/// its archive. Each file lands via `<name>.tmp` + rename.
fn place(plan: &Plan, download: &Path, dir: &Path) -> Result<()> {
    match plan.archive {
        Archive::Raw => {
            let target = dir.join(&plan.members[0]);
            replace(download, &target, true)?;
        }
        Archive::Zip => {
            let file = std::fs::File::open(download)?;
            let mut archive = zip::ZipArchive::new(file).context("not a zip archive")?;
            let mut found = Vec::new();
            for index in 0..archive.len() {
                let mut entry = archive.by_index(index)?;
                if !entry.is_file() {
                    continue;
                }
                let Some(name) = entry
                    .enclosed_name()
                    .and_then(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
                else {
                    continue;
                };
                if plan.members.contains(&name) && !found.contains(&name) {
                    let tmp = dir.join(format!("{name}.tmp"));
                    let mut out = std::fs::File::create(&tmp)?;
                    std::io::copy(&mut entry, &mut out)?;
                    out.sync_all()?;
                    drop(out);
                    replace(&tmp, &dir.join(&name), false)?;
                    found.push(name);
                }
            }
            ensure_all_found(plan, &found)?;
        }
        Archive::TarXz => {
            let tar_path = dir.join(".extract.tar");
            let result = (|| -> Result<()> {
                {
                    let mut input = std::io::BufReader::new(std::fs::File::open(download)?);
                    let mut output = std::io::BufWriter::new(std::fs::File::create(&tar_path)?);
                    lzma_rs::xz_decompress(&mut input, &mut output)
                        .map_err(|e| anyhow::anyhow!("bad xz archive: {e:?}"))?;
                }
                let mut archive = tar::Archive::new(std::fs::File::open(&tar_path)?);
                let mut found = Vec::new();
                for entry in archive.entries()? {
                    let mut entry = entry?;
                    if !entry.header().entry_type().is_file() {
                        continue;
                    }
                    let Some(name) = entry
                        .path()?
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                    else {
                        continue;
                    };
                    if plan.members.contains(&name) && !found.contains(&name) {
                        let tmp = dir.join(format!("{name}.tmp"));
                        let mut out = std::fs::File::create(&tmp)?;
                        std::io::copy(&mut entry, &mut out)?;
                        out.sync_all()?;
                        drop(out);
                        replace(&tmp, &dir.join(&name), false)?;
                        found.push(name);
                    }
                }
                ensure_all_found(plan, &found)
            })();
            let _ = std::fs::remove_file(&tar_path);
            result?;
        }
    }
    Ok(())
}

fn ensure_all_found(plan: &Plan, found: &[String]) -> Result<()> {
    let absent: Vec<&String> = plan
        .members
        .iter()
        .filter(|member| !found.contains(member))
        .collect();
    if !absent.is_empty() {
        bail!(
            "{} is missing {}",
            plan.asset,
            absent
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

fn replace(from: &Path, to: &Path, copy: bool) -> Result<()> {
    if copy {
        let tmp = to.with_extension("tmp");
        std::fs::copy(from, &tmp).with_context(|| format!("failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, to)
            .with_context(|| format!("failed to replace {} (is it running?)", to.display()))?;
    } else {
        std::fs::rename(from, to)
            .with_context(|| format!("failed to replace {} (is it running?)", to.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Write;
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn plans_cover_desktop_targets() {
        assert_eq!(
            plan_for(Tool::YtDlp, "windows", "x86_64").unwrap().asset,
            "yt-dlp.exe"
        );
        assert_eq!(
            plan_for(Tool::YtDlp, "macos", "aarch64").unwrap().asset,
            "yt-dlp_macos"
        );
        assert_eq!(
            plan_for(Tool::Ffmpeg, "linux", "x86_64").unwrap().archive,
            Archive::TarXz
        );
        assert!(plan_for(Tool::Ffmpeg, "macos", "aarch64").is_none());
        let deno = plan_for(Tool::Deno, "windows", "x86_64").unwrap();
        assert_eq!(deno.asset, "deno-x86_64-pc-windows-msvc.zip");
        assert_eq!(deno.members, vec!["deno.exe".to_string()]);
    }

    #[test]
    fn parses_gnu_and_powershell_sums() {
        let hash = "a".repeat(64);
        let other = "b".repeat(64);
        let gnu = format!("{other}  yt-dlp\n{hash}  yt-dlp.exe\n");
        assert_eq!(parse_sums(&gnu, "yt-dlp.exe"), Some(hash.clone()));
        assert_eq!(parse_sums(&gnu, "yt-dlp"), Some(other.clone()));
        let star = format!("{hash} *file.zip");
        assert_eq!(parse_sums(&star, "file.zip"), Some(hash.clone()));
        let ps = format!(
            "Algorithm : SHA256\nHash      : {}\nPath      : C:\\a\\deno.zip\n",
            hash.to_uppercase()
        );
        assert_eq!(
            parse_sums(&ps, "deno-x86_64-pc-windows-msvc.zip"),
            Some(hash)
        );
        assert_eq!(parse_sums(&gnu, "missing.zip"), None);
    }

    #[test]
    fn pin_rejects_other_hosts() {
        let github = Github::default();
        assert!(pinned(
            &github,
            "yt-dlp/yt-dlp",
            "https://github.com/yt-dlp/yt-dlp/releases/download/1/yt-dlp.exe"
        )
        .is_ok());
        assert!(pinned(&github, "yt-dlp/yt-dlp", "https://evil.example/yt-dlp.exe").is_err());
        assert!(pinned(
            &github,
            "yt-dlp/yt-dlp",
            "https://github.com/other/repo/releases/download/1/x"
        )
        .is_err());
    }

    fn sha_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn release_json(base: &str, repo: &str, tag: &str, assets: &[&str]) -> Vec<u8> {
        let assets: Vec<serde_json::Value> = assets
            .iter()
            .map(|name| {
                serde_json::json!({
                    "name": name,
                    "browser_download_url": format!("{base}/{repo}/releases/download/{tag}/{name}"),
                })
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "tag_name": tag,
            "published_at": "2026-11-24T10:00:00Z",
            "assets": assets,
        }))
        .unwrap()
    }

    fn raw_plan() -> Plan {
        Plan {
            repo: "o/tool",
            asset: "tool-bin".into(),
            sums: Sums::Combined("SUMS"),
            archive: Archive::Raw,
            members: vec!["tool".into()],
            max_bytes: MB,
        }
    }

    #[tokio::test]
    async fn installs_a_raw_binary_after_checking_its_hash() {
        let body = b"#!/bin/sh\necho hi\n".to_vec();
        let sums = format!("{}  tool-bin\n", sha_hex(&body));
        let mut routes = HashMap::new();
        routes.insert(
            "/o/tool/releases/download/v1/tool-bin".to_string(),
            body.clone(),
        );
        routes.insert(
            "/o/tool/releases/download/v1/SUMS".to_string(),
            sums.into_bytes(),
        );
        let base = serve_with_release(routes, "o/tool", "v1", &["tool-bin", "SUMS"]).await;
        let github = Github {
            api: base.clone(),
            web: base.clone(),
        };
        let dir = tempfile::tempdir().unwrap();
        let client = http_client().unwrap();
        let version = install_with(
            &client,
            &github,
            &raw_plan(),
            dir.path(),
            &ProgressHandle::Hidden,
        )
        .await
        .unwrap();
        assert_eq!(version, "v1");
        assert_eq!(std::fs::read(dir.path().join("tool")).unwrap(), body);
        assert!(!dir.path().join(".download-tool-bin").exists());
    }

    #[tokio::test]
    async fn refuses_a_binary_whose_hash_differs() {
        let mut routes = HashMap::new();
        routes.insert(
            "/o/tool/releases/download/v1/tool-bin".to_string(),
            b"tampered".to_vec(),
        );
        routes.insert(
            "/o/tool/releases/download/v1/SUMS".to_string(),
            format!("{}  tool-bin\n", sha_hex(b"original")).into_bytes(),
        );
        let base = serve_with_release(routes, "o/tool", "v1", &["tool-bin", "SUMS"]).await;
        let github = Github {
            api: base.clone(),
            web: base,
        };
        let dir = tempfile::tempdir().unwrap();
        let error = install_with(
            &http_client().unwrap(),
            &github,
            &raw_plan(),
            dir.path(),
            &ProgressHandle::Hidden,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("checksum mismatch"));
        assert!(!dir.path().join("tool").exists());
    }

    #[tokio::test]
    async fn extracts_members_from_a_zip() {
        let mut zip_bytes = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_bytes));
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file("pkg/bin/ffmpeg", options).unwrap();
            writer.write_all(b"ffmpeg-bytes").unwrap();
            writer.start_file("pkg/bin/ffprobe", options).unwrap();
            writer.write_all(b"ffprobe-bytes").unwrap();
            writer.start_file("pkg/doc/readme.txt", options).unwrap();
            writer.write_all(b"ignored").unwrap();
            writer.finish().unwrap();
        }
        let mut routes = HashMap::new();
        routes.insert(
            "/o/ff/releases/download/latest/ff.zip".to_string(),
            zip_bytes.clone(),
        );
        routes.insert(
            "/o/ff/releases/download/latest/ff.zip.sha256sum".to_string(),
            format!("{}  ff.zip", sha_hex(&zip_bytes)).into_bytes(),
        );
        let base =
            serve_with_release(routes, "o/ff", "latest", &["ff.zip", "ff.zip.sha256sum"]).await;
        let github = Github {
            api: base.clone(),
            web: base,
        };
        let plan = Plan {
            repo: "o/ff",
            asset: "ff.zip".into(),
            sums: Sums::Sidecar,
            archive: Archive::Zip,
            members: vec!["ffmpeg".into(), "ffprobe".into()],
            max_bytes: MB,
        };
        let dir = tempfile::tempdir().unwrap();
        let version = install_with(
            &http_client().unwrap(),
            &github,
            &plan,
            dir.path(),
            &ProgressHandle::Hidden,
        )
        .await
        .unwrap();
        assert_eq!(version, "2026-11-24", "rolling tag uses the publish date");
        assert_eq!(
            std::fs::read(dir.path().join("ffmpeg")).unwrap(),
            b"ffmpeg-bytes"
        );
        assert_eq!(
            std::fs::read(dir.path().join("ffprobe")).unwrap(),
            b"ffprobe-bytes"
        );
        assert!(!dir.path().join("readme.txt").exists());
    }

    /// Start a server whose `/repos/<repo>/releases/latest` names its own URLs.
    async fn serve_with_release(
        mut routes: HashMap<String, Vec<u8>>,
        repo: &str,
        tag: &str,
        assets: &[&str],
    ) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let base = format!("http://127.0.0.1:{port}");
        routes.insert(
            format!("/repos/{repo}/releases/latest"),
            release_json(&base, repo, tag, assets),
        );
        serve_on(port, routes).await;
        base
    }

    async fn serve_on(port: u16, routes: HashMap<String, Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .unwrap();
        let routes = Arc::new(routes);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let routes = Arc::clone(&routes);
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 4096];
                    let mut read = 0;
                    while !buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf[read..]).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => read += n,
                        }
                    }
                    let request = String::from_utf8_lossy(&buf[..read]);
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let (status, body) = match routes.get(&path) {
                        Some(body) => ("200 OK", body.clone()),
                        None => ("404 Not Found", Vec::new()),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
            }
        });
    }
}
