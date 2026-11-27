//! Running yt-dlp: video info (`-J`) and downloads with live progress.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::progress::{
    DownloadPaused, DownloadStopped, ProgressHandle, CONVERTING, DOWNLOADING, MERGING,
};
use crate::request::{common_args, is_youtube_url, DownloadKind, DownloadRequest};
use crate::tools::ToolPaths;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Thumbnail {
    pub url: String,
    #[serde(default)]
    pub width: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Format {
    pub format_id: String,
    #[serde(default)]
    pub ext: String,
    #[serde(default)]
    pub vcodec: Option<String>,
    #[serde(default)]
    pub acodec: Option<String>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub fps: Option<f64>,
    #[serde(default)]
    pub abr: Option<f64>,
    #[serde(default)]
    pub tbr: Option<f64>,
    #[serde(default)]
    pub filesize: Option<u64>,
    #[serde(default)]
    pub filesize_approx: Option<u64>,
    #[serde(default)]
    pub format_note: Option<String>,
}

fn real_codec(codec: &Option<String>) -> bool {
    codec
        .as_deref()
        .is_some_and(|c| !c.is_empty() && c != "none")
}

impl Format {
    pub fn has_video(&self) -> bool {
        real_codec(&self.vcodec)
    }

    pub fn has_audio(&self) -> bool {
        real_codec(&self.acodec)
    }

    pub fn size(&self) -> Option<u64> {
        self.filesize.or(self.filesize_approx)
    }

    fn short_codec(codec: &Option<String>) -> String {
        codec
            .as_deref()
            .and_then(|c| c.split('.').next())
            .unwrap_or("")
            .to_string()
    }

    /// `1080p60 · mp4 · avc1 · 120.3 MB` / `129 kbps · m4a · mp4a`.
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.has_video() {
            let mut res = self
                .height
                .map(|h| format!("{h}p"))
                .unwrap_or_else(|| self.format_note.clone().unwrap_or_default());
            if let Some(fps) = self.fps.filter(|fps| *fps > 30.5) {
                res.push_str(&format!("{}", fps.round() as u32));
            }
            parts.push(res);
        } else if let Some(abr) = self.abr.or(self.tbr) {
            parts.push(format!("{} kbps", abr.round() as u32));
        }
        parts.push(self.ext.clone());
        if self.has_video() {
            parts.push(Self::short_codec(&self.vcodec));
        }
        if self.has_audio() && !self.has_video() {
            parts.push(Self::short_codec(&self.acodec));
        }
        if self.format_id.ends_with("-drc") {
            parts.push("stable volume".to_string());
        }
        if let Some(size) = self.size() {
            parts.push(crate::progress::format_size(size));
        }
        parts.retain(|part| !part.is_empty());
        parts.join(" · ")
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct VideoInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub uploader: Option<String>,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub thumbnails: Vec<Thumbnail>,
    #[serde(default)]
    pub webpage_url: Option<String>,
    #[serde(default)]
    pub formats: Vec<Format>,
}

impl VideoInfo {
    /// Video streams to offer, best first. A merge to mp4 offers only mp4
    /// video (as the Node CLI did), so the merge never re-encodes.
    pub fn video_choices(&self, kind: DownloadKind) -> Vec<&Format> {
        let mut list: Vec<&Format> = self
            .formats
            .iter()
            .filter(|f| match kind {
                DownloadKind::Merged => f.has_video() && !f.has_audio() && f.ext == "mp4",
                DownloadKind::Video => f.has_video() && !f.has_audio(),
                DownloadKind::Single => f.has_video() && f.has_audio(),
                DownloadKind::Audio => false,
            })
            .collect();
        list.sort_by(|a, b| {
            let key = |f: &Format| {
                (
                    f.height.unwrap_or(0),
                    f.fps.unwrap_or(0.0) as u32,
                    f.tbr.unwrap_or(0.0) as u64,
                )
            };
            key(b).cmp(&key(a))
        });
        list
    }

    /// Audio streams to offer, best first. A merge offers only m4a.
    pub fn audio_choices(&self, kind: DownloadKind) -> Vec<&Format> {
        let mut list: Vec<&Format> = self
            .formats
            .iter()
            .filter(|f| match kind {
                DownloadKind::Merged => f.has_audio() && !f.has_video() && f.ext == "m4a",
                DownloadKind::Audio => f.has_audio() && !f.has_video(),
                _ => false,
            })
            .collect();
        list.sort_by(|a, b| {
            // Regular tracks before "stable volume" (DRC) ones, then by bitrate.
            let key = |f: &Format| {
                (
                    !f.format_id.ends_with("-drc"),
                    (f.abr.or(f.tbr).unwrap_or(0.0) * 10.0) as u64,
                )
            };
            key(b).cmp(&key(a))
        });
        list
    }

    pub fn format(&self, id: &str) -> Option<&Format> {
        self.formats.iter().find(|f| f.format_id == id)
    }

    /// A JPEG thumbnail around 480 px wide (decoders for it are small and
    /// it is quick to fetch), else whatever yt-dlp lists as the main one.
    pub fn thumbnail_url(&self) -> Option<String> {
        let jpg = |t: &&Thumbnail| {
            let path = t.url.split('?').next().unwrap_or("");
            path.ends_with(".jpg")
        };
        self.thumbnails
            .iter()
            .filter(jpg)
            .filter(|t| t.width.is_none_or(|w| w <= 720))
            .max_by_key(|t| t.width.unwrap_or(0))
            .map(|t| t.url.clone())
            .or_else(|| self.thumbnail.clone())
    }

    pub fn duration_text(&self) -> String {
        let Some(total) = self.duration else {
            return String::new();
        };
        let total = total.round() as u64;
        let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
        if h > 0 {
            format!("{h}:{m:02}:{s:02}")
        } else {
            format!("{m}:{s:02}")
        }
    }
}

fn command(tools: &ToolPaths, args: Vec<OsString>) -> Command {
    let mut cmd = Command::new(&tools.ytdlp);
    cmd.args(args)
        .env("PYTHONIOENCODING", "utf-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: no console flashes up behind the GUI.
        cmd.creation_flags(0x0800_0000);
    }
    #[cfg(unix)]
    {
        // Own process group, so `kill_tree` reaches yt-dlp's inner process.
        cmd.process_group(0);
    }
    cmd
}

/// End yt-dlp and everything it started. The release builds are
/// single-file PyInstaller apps: a launcher runs the real yt-dlp as a child,
/// and killing only the launcher leaves that child downloading.
async fn kill_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(0x0800_0000)
                .status()
                .await;
        }
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// The most useful line of yt-dlp's stderr for an error message.
fn error_line(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    lines
        .iter()
        .rev()
        .find(|line| line.starts_with("ERROR:"))
        .or(lines.last())
        .map(|line| line.trim_start_matches("ERROR:").trim().to_string())
        .unwrap_or_else(|| "yt-dlp failed".to_string())
}

/// `yt-dlp -J` for one video.
pub async fn fetch_info(tools: &ToolPaths, url: &str) -> Result<VideoInfo> {
    let mut args = common_args(tools, url);
    args.extend(["-J", "--no-playlist", "--no-warnings", "--"].map(OsString::from));
    args.push(url.into());
    let output = command(tools, args)
        .output()
        .await
        .with_context(|| format!("failed to run {}", tools.ytdlp.display()))?;
    if !output.status.success() {
        bail!("{}", error_line(&String::from_utf8_lossy(&output.stderr)));
    }
    let info: VideoInfo =
        serde_json::from_slice(&output.stdout).context("yt-dlp returned unexpected JSON")?;
    if info.formats.is_empty() {
        bail!("no downloadable formats found");
    }
    Ok(info)
}

/// A short public video used to check a sign-in when no URL is at hand.
pub const COOKIE_TEST_URL: &str = "https://www.youtube.com/watch?v=jNQXAC9IVRw";

/// Try `source` on `url` before saving it. The error is a plain-words reason
/// (locked, encrypted, not signed in…) ready to show the user.
pub async fn test_cookies(
    tools: &ToolPaths,
    source: &crate::cookies::CookieSource,
    url: &str,
) -> std::result::Result<(), String> {
    let mut with_cookies = tools.clone();
    with_cookies.cookies = Some(source.clone());
    // The sign-in is only sent to YouTube, so test it on a YouTube link.
    let url = if is_youtube_url(url) {
        url
    } else {
        COOKIE_TEST_URL
    };
    fetch_info(&with_cookies, url)
        .await
        .map(|_| ())
        .map_err(|error| crate::cookies::explain(source, &format!("{error:#}")))
}

/// `yt-dlp --version`.
pub async fn version(tools: &ToolPaths) -> Result<String> {
    let output = command(tools, vec!["--version".into()]).output().await?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[derive(Debug, PartialEq)]
enum Line {
    Progress {
        downloaded: u64,
        total: Option<u64>,
        /// `total` is yt-dlp's running estimate (fragmented streams), not
        /// the real size: it moves as fragments arrive.
        estimated: bool,
        speed: Option<f64>,
        eta: Option<u64>,
        format: String,
    },
    Postprocess {
        started: bool,
        name: String,
    },
    /// `before_dl`: extraction is over and the transfer begins.
    Start,
    File(PathBuf),
    Other,
}

fn num<T: std::str::FromStr>(token: Option<&str>) -> Option<T> {
    token
        .filter(|t| *t != "NA" && *t != "None")
        .and_then(|t| t.parse().ok())
}

fn parse_line(line: &str) -> Line {
    let line = line.trim_end_matches(['\r', '\n']);
    if let Some(rest) = line.strip_prefix("[ytd-file] ") {
        return Line::File(PathBuf::from(rest.trim()));
    }
    if line.starts_with("[ytd-start]") {
        return Line::Start;
    }
    if let Some(rest) = line.strip_prefix("[ytd-pp] ") {
        let mut tokens = rest.split_whitespace();
        let started = tokens.next() == Some("started");
        let name = tokens.next().unwrap_or("").to_string();
        return Line::Postprocess { started, name };
    }
    if let Some(rest) = line.strip_prefix("[ytd] ") {
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        let get = |i: usize| tokens.get(i).copied();
        let downloaded = num::<f64>(get(0)).map(|v| v as u64).unwrap_or(0);
        let exact = num::<f64>(get(1)).map(|v| v as u64).filter(|v| *v > 0);
        let estimate = num::<f64>(get(2)).map(|v| v as u64).filter(|v| *v > 0);
        return Line::Progress {
            downloaded,
            total: exact.or(estimate),
            estimated: exact.is_none() && estimate.is_some(),
            speed: num::<f64>(get(3)),
            eta: num::<f64>(get(4)).map(|v| v as u64),
            format: get(5).unwrap_or("").to_string(),
        };
    }
    Line::Other
}

/// The status a line moves a download to. The job stays `starting` (a
/// sliding bar) through extraction and the YouTube check, and becomes
/// `downloading` at the first sign of the transfer: the `before_dl` marker or
/// a progress line, whichever comes first.
fn status_for(line: &Line, started: bool) -> Option<&'static str> {
    match line {
        Line::Start | Line::Progress { .. } if !started => Some(DOWNLOADING),
        Line::Postprocess {
            started: true,
            name,
        } => match name.as_str() {
            "Merger" => Some(MERGING),
            "ExtractAudio" | "VideoConvertor" | "VideoRemuxer" => Some(CONVERTING),
            _ => None,
        },
        _ => None,
    }
}

/// Byte counts per stream (a merge downloads video, then audio).
#[derive(Default)]
struct Totals {
    /// format id -> (downloaded, total, total is an estimate)
    streams: BTreeMap<String, (u64, u64, bool)>,
}

impl Totals {
    fn update(&mut self, format: String, downloaded: u64, total: Option<u64>, estimated: bool) {
        let entry = self.streams.entry(format).or_default();
        entry.0 = downloaded;
        if let Some(total) = total {
            entry.1 = total;
            entry.2 = estimated;
        }
    }

    fn downloaded(&self) -> u64 {
        self.streams.values().map(|(d, _, _)| d).sum()
    }

    fn total(&self, expected: u64) -> u64 {
        let known: u64 = self.streams.values().map(|(_, t, _)| t).sum();
        known.max(expected)
    }

    fn estimated(&self) -> bool {
        self.streams.values().any(|(_, _, estimated)| *estimated)
    }
}

/// Run one download. Returns the final file. Pause and stop kill yt-dlp
/// and come back as [`DownloadPaused`] / [`DownloadStopped`]; a later run
/// with the same request continues from yt-dlp's `.part` files.
pub async fn download(
    tools: &ToolPaths,
    request: &DownloadRequest,
    progress: &ProgressHandle,
) -> Result<PathBuf> {
    if tokio::fs::metadata(&request.dir)
        .await
        .is_ok_and(|meta| !meta.is_dir())
    {
        bail!(
            "{} is a file, not a folder: choose another download folder (CLI: --dir)",
            request.dir.display()
        );
    }
    tokio::fs::create_dir_all(&request.dir)
        .await
        .with_context(|| format!("failed to create {}", request.dir.display()))?;
    let mut child = command(tools, request.ytdlp_args(tools))
        .spawn()
        .with_context(|| format!("failed to run {}", tools.ytdlp.display()))?;
    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;

    let expected = if request.trim().is_set() {
        0
    } else {
        request.expected_size
    };
    if expected > 0 {
        progress.set_length(expected);
    }
    // Progress and the output path come on stdout; yt-dlp writes the
    // postprocessor lines ([ytd-pp]) and errors to stderr, so read both.
    let mut out_lines = BufReader::new(stdout).lines();
    // stderr is read as bytes: a line that is not UTF-8 (a legacy-codepage
    // file name on Windows) must not stop the draining, or yt-dlp blocks on
    // a full pipe and the job hangs.
    // Dropped (closing the pipe) at EOF or on a read error.
    let mut err_reader = Some(BufReader::new(stderr));
    let mut err_buf: Vec<u8> = Vec::new();
    let mut out_open = true;
    let mut stderr = String::new();
    let mut started = false;
    let mut totals = Totals::default();
    let mut file: Option<PathBuf> = None;
    let mut ticker = tokio::time::interval(Duration::from_millis(150));
    while out_open || err_reader.is_some() {
        let line = tokio::select! {
            line = out_lines.next_line(), if out_open => {
                match line.context("failed to read yt-dlp output")? {
                    Some(line) => line,
                    None => {
                        out_open = false;
                        continue;
                    }
                }
            }
            // Cancel safe: bytes read before another branch wins stay in
            // err_buf and the next call goes on from there.
            read = async {
                match err_reader.as_mut() {
                    Some(reader) => reader.read_until(b'\n', &mut err_buf).await,
                    None => Ok(0),
                }
            }, if err_reader.is_some() => {
                match read {
                    Ok(0) | Err(_) => {
                        err_reader = None;
                        continue;
                    }
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&err_buf)
                            .trim_end_matches(['\r', '\n'])
                            .to_string();
                        err_buf.clear();
                        stderr.push_str(&line);
                        stderr.push('\n');
                        line
                    }
                }
            }
            _ = ticker.tick() => {
                if let Err(error) = progress.check_control() {
                    kill_tree(&mut child).await;
                    drop(out_lines);
                    drop(err_reader);
                    // A stop can land while a pause is still killing yt-dlp:
                    // look at the flags again, stop wins.
                    let stopped = crate::progress::is_stopped(&error)
                        || progress
                            .check_control()
                            .err()
                            .is_some_and(|e| crate::progress::is_stopped(&e));
                    if stopped {
                        let request = request.clone();
                        let _ = tokio::task::spawn_blocking(move || remove_partials(&request)).await;
                        bail!(DownloadStopped);
                    }
                    bail!(DownloadPaused);
                }
                continue;
            }
        };
        let parsed = parse_line(&line);
        if let Some(status) = status_for(&parsed, started) {
            started = true;
            progress.set_status(status);
        }
        match parsed {
            Line::Progress {
                downloaded,
                total,
                estimated,
                speed,
                eta,
                format,
            } => {
                totals.update(format, downloaded, total, estimated);
                progress.set_estimated(totals.estimated());
                progress.set_length(totals.total(expected));
                progress.set_position(totals.downloaded());
                progress.set_rate(speed, eta);
            }
            Line::File(path) => file = Some(path),
            Line::Start | Line::Postprocess { .. } | Line::Other => {}
        }
    }
    let status = child.wait().await.context("yt-dlp did not exit")?;
    if !status.success() {
        bail!("{}", error_line(&stderr));
    }
    let file = file.context("yt-dlp finished without reporting the output file")?;
    // The final file has been moved out; drop the job's (now empty) folder.
    let request = request.clone();
    let _ = tokio::task::spawn_blocking(move || remove_partials(&request)).await;
    Ok(file)
}

/// A stopped download is abandoned: delete its partial files (a paused one
/// keeps them so it can resume). They all live in the job's own folder
/// (`DownloadRequest::partial_dir`), so only that folder is removed: files
/// in the download folder itself are never touched. Windows can release
/// file handles a moment after the process tree is gone, so retry briefly.
/// Blocking: call it from a blocking task or thread.
pub fn remove_partials(request: &DownloadRequest) {
    let dir = request.partial_dir();
    for _ in 0..10 {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => std::thread::sleep(Duration::from_millis(300)),
        }
    }
    // Only succeeds once no other job has partial files left.
    if let Some(parts) = dir.parent() {
        let _ = std::fs::remove_dir(parts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_in(dir: &std::path::Path, name: &str, kind: DownloadKind) -> DownloadRequest {
        DownloadRequest {
            url: "u".into(),
            title: "t".into(),
            kind,
            video_format: Some("137".into()),
            audio_format: Some("140".into()),
            name: name.into(),
            dir: dir.to_path_buf(),
            trim_start: None,
            trim_end: None,
            expected_size: 0,
        }
    }

    fn files_in(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn remove_partials_deletes_only_the_jobs_folder() {
        let dir = tempfile::tempdir().unwrap();
        // Finished files that share the job's name must survive, whatever
        // their extension: a video `Song.mp4` next to an mp3 job `Song`.
        for file in ["Song.mp4", "Song.m4a", "Song.f137.mp4", "Song.final.mp4"] {
            std::fs::write(dir.path().join(file), b"x").unwrap();
        }
        let song = request_in(dir.path(), "Song", DownloadKind::Audio);
        let other = request_in(dir.path(), "Other", DownloadKind::Merged);
        for request in [&song, &other] {
            let parts = request.partial_dir();
            std::fs::create_dir_all(&parts).unwrap();
            std::fs::write(parts.join(format!("{}.m4a.part", request.name)), b"x").unwrap();
        }
        std::fs::write(song.partial_dir().join("Song.webm"), b"x").unwrap();

        remove_partials(&song);
        assert!(!song.partial_dir().exists());
        assert!(other.partial_dir().exists(), "another job's partials stay");
        assert_eq!(
            files_in(dir.path()),
            [
                ".ytd-parts",
                "Song.f137.mp4",
                "Song.final.mp4",
                "Song.m4a",
                "Song.mp4"
            ]
        );

        remove_partials(&other);
        assert_eq!(
            files_in(dir.path()),
            ["Song.f137.mp4", "Song.final.mp4", "Song.m4a", "Song.mp4"],
            "the empty parts folder goes too"
        );
        remove_partials(&other); // nothing left: no error, no change
    }

    #[test]
    fn parses_progress_lines() {
        assert_eq!(
            parse_line("[ytd] 1024 4096 NA 512.5 6 137"),
            Line::Progress {
                downloaded: 1024,
                total: Some(4096),
                estimated: false,
                speed: Some(512.5),
                eta: Some(6),
                format: "137".into()
            }
        );
        // Estimate used when the exact total is unknown.
        assert_eq!(
            parse_line("[ytd] 10 NA 2000.0 NA NA 140"),
            Line::Progress {
                downloaded: 10,
                total: Some(2000),
                estimated: true,
                speed: None,
                eta: None,
                format: "140".into()
            }
        );
        assert_eq!(
            parse_line("[ytd-pp] started Merger"),
            Line::Postprocess {
                started: true,
                name: "Merger".into()
            }
        );
        assert_eq!(
            parse_line("[ytd-file] C:\\out\\a b.mp4\r"),
            Line::File(PathBuf::from("C:\\out\\a b.mp4"))
        );
        assert_eq!(parse_line("[youtube] Extracting URL"), Line::Other);
    }

    /// A fake yt-dlp that floods stderr with more than a pipe buffer of
    /// bytes that are not UTF-8, reports a merge there, and then the file.
    /// The download must drain it all and finish (it used to hang).
    #[tokio::test]
    async fn a_file_as_the_folder_gets_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ytd");
        std::fs::write(&file, b"binary").unwrap();
        let tools = ToolPaths {
            ytdlp: dir.path().join("yt-dlp"),
            ffmpeg: dir.path().join("ffmpeg"),
            deno: None,
            cookies: None,
        };
        let request = DownloadRequest {
            url: "https://example.com/v".into(),
            title: "clip".into(),
            kind: DownloadKind::Merged,
            video_format: None,
            audio_format: None,
            name: "clip".into(),
            dir: file.clone(),
            trim_start: None,
            trim_end: None,
            expected_size: 0,
        };
        let err = download(&tools, &request, &ProgressHandle::Hidden)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(&file.display().to_string()), "{err}");
        assert!(err.contains("is a file, not a folder"), "{err}");
        assert!(err.contains("--dir"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn non_utf8_stderr_does_not_hang_and_merging_shows() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("yt-dlp");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             echo '[ytd-start] 18'\n\
             i=0; while [ $i -lt 3000 ]; do printf 'warn \\377\\376 legacy name %04d xxxxxxxxxxxxxxxxxxxx\\n' $i >&2; i=$((i+1)); done\n\
             echo '[ytd] 100 100 NA 50 0 18'\n\
             echo '[ytd-pp] started Merger' >&2\n\
             echo '[ytd-file] /tmp/out.mp4'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tools = ToolPaths {
            ytdlp: script,
            ffmpeg: dir.path().join("ffmpeg"),
            deno: None,
            cookies: None,
        };
        let request = DownloadRequest {
            url: "https://example.com/v".into(),
            title: "clip".into(),
            kind: DownloadKind::Merged,
            video_format: None,
            audio_format: None,
            name: "clip".into(),
            dir: dir.path().join("out"),
            trim_start: None,
            trim_end: None,
            expected_size: 0,
        };
        let snapshot = std::sync::Arc::new(crate::progress::JobSnapshot::default());
        let progress = ProgressHandle::Gui(std::sync::Arc::clone(&snapshot));
        let file = tokio::time::timeout(
            Duration::from_secs(20),
            download(&tools, &request, &progress),
        )
        .await
        .expect("download hung")
        .unwrap();
        assert_eq!(file, PathBuf::from("/tmp/out.mp4"));
        assert_eq!(snapshot.status(), MERGING, "stderr postprocess line seen");
    }

    #[test]
    fn start_marker_parses() {
        assert_eq!(parse_line("[ytd-start] 137+140"), Line::Start);
        assert_eq!(parse_line("[ytd-start] mp4\r"), Line::Start);
    }

    #[test]
    fn stays_starting_until_the_transfer_begins() {
        let progress = |downloaded| Line::Progress {
            downloaded,
            total: Some(100),
            estimated: false,
            speed: None,
            eta: None,
            format: "18".into(),
        };
        let merger = Line::Postprocess {
            started: true,
            name: "Merger".into(),
        };
        // Extraction noise changes nothing; the marker starts the download;
        // later progress keeps it; the merge shows as merging.
        let lines = [Line::Other, Line::Start, progress(10), progress(50), merger];
        let mut started = false;
        let mut seen = Vec::new();
        for line in &lines {
            let status = status_for(line, started);
            started |= status.is_some();
            seen.push(status);
        }
        assert_eq!(seen, [None, Some(DOWNLOADING), None, None, Some(MERGING)]);
        // Without the marker the first progress line starts it.
        assert_eq!(status_for(&progress(1), false), Some(DOWNLOADING));
        assert_eq!(
            status_for(
                &Line::Postprocess {
                    started: true,
                    name: "ExtractAudio".into()
                },
                true
            ),
            Some(CONVERTING)
        );
        assert_eq!(
            status_for(
                &Line::Postprocess {
                    started: false,
                    name: "Merger".into()
                },
                true
            ),
            None,
            "a finished postprocessor leaves the status alone"
        );
    }

    #[test]
    fn totals_add_streams_and_respect_expected() {
        let mut totals = Totals::default();
        totals.update("137".into(), 50, Some(100), true);
        assert_eq!(
            totals.total(150),
            150,
            "expected covers the audio not yet started"
        );
        assert!(totals.estimated());
        totals.update("137".into(), 50, Some(100), false);
        totals.update("140".into(), 10, Some(60), false);
        assert!(!totals.estimated());
        assert_eq!(totals.downloaded(), 60);
        assert_eq!(totals.total(150), 160);
    }

    #[test]
    fn error_line_prefers_error_prefix() {
        let stderr = "WARNING: x\nERROR: [youtube] abc: Video unavailable\n\n";
        assert_eq!(error_line(stderr), "[youtube] abc: Video unavailable");
        assert_eq!(error_line("just this"), "just this");
        assert_eq!(error_line(""), "yt-dlp failed");
    }

    fn sample() -> VideoInfo {
        let json = r#"{
            "id": "x", "title": "T", "duration": 3725.4,
            "thumbnails": [
                {"url": "https://i.ytimg.com/vi/x/hqdefault.jpg?sqp=1", "width": 480},
                {"url": "https://i.ytimg.com/vi/x/maxresdefault.webp", "width": 1280},
                {"url": "https://i.ytimg.com/vi/x/default.jpg", "width": 120}
            ],
            "formats": [
                {"format_id": "sb0", "ext": "mhtml", "vcodec": "none", "acodec": "none"},
                {"format_id": "140", "ext": "m4a", "vcodec": "none", "acodec": "mp4a.40.2", "abr": 129.5, "filesize": 3000000},
                {"format_id": "251", "ext": "webm", "vcodec": "none", "acodec": "opus", "abr": 140.0},
                {"format_id": "137", "ext": "mp4", "vcodec": "avc1.640028", "acodec": "none", "height": 1080, "fps": 30},
                {"format_id": "299", "ext": "mp4", "vcodec": "avc1.64002a", "acodec": "none", "height": 1080, "fps": 60},
                {"format_id": "248", "ext": "webm", "vcodec": "vp9", "acodec": "none", "height": 1080},
                {"format_id": "18", "ext": "mp4", "vcodec": "avc1.42001E", "acodec": "mp4a.40.2", "height": 360}
            ]
        }"#;
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn choices_follow_the_node_cli_rules() {
        let info = sample();
        let ids = |list: Vec<&Format>| list.iter().map(|f| f.format_id.clone()).collect::<Vec<_>>();
        assert_eq!(
            ids(info.video_choices(DownloadKind::Merged)),
            ["299", "137"]
        );
        assert_eq!(
            ids(info.video_choices(DownloadKind::Video)),
            ["299", "137", "248"]
        );
        assert_eq!(ids(info.video_choices(DownloadKind::Single)), ["18"]);
        assert_eq!(ids(info.audio_choices(DownloadKind::Merged)), ["140"]);
        assert_eq!(ids(info.audio_choices(DownloadKind::Audio)), ["251", "140"]);
    }

    #[test]
    fn labels_thumbnail_and_duration() {
        let info = sample();
        assert_eq!(info.format("299").unwrap().label(), "1080p60 · mp4 · avc1");
        assert_eq!(
            info.format("140").unwrap().label(),
            "130 kbps · m4a · mp4a · 2.86 MB"
        );
        assert_eq!(
            info.thumbnail_url().as_deref(),
            Some("https://i.ytimg.com/vi/x/hqdefault.jpg?sqp=1")
        );
        assert_eq!(info.duration_text(), "1:02:05");
    }
}
