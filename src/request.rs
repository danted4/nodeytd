//! What the user asked for, and the yt-dlp arguments that produce it.

use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::fsutil::{sanitize_file_name, title_slug};
use crate::tools::ToolPaths;
use crate::trim::TrimRange;

/// The four choices the Node CLI offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum DownloadKind {
    /// Video and audio merged into one mp4 (recommended).
    Merged,
    /// Audio only, converted to mp3.
    Audio,
    /// Video only, no audio track.
    Video,
    /// One stream that already has both (often lower quality).
    Single,
}

impl DownloadKind {
    pub const ALL: [DownloadKind; 4] = [
        DownloadKind::Merged,
        DownloadKind::Audio,
        DownloadKind::Video,
        DownloadKind::Single,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DownloadKind::Merged => "Recommended (merged mp4)",
            DownloadKind::Audio => "Audio only (mp3)",
            DownloadKind::Video => "Video only",
            DownloadKind::Single => "Single file (best available)",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            DownloadKind::Merged => "MP4",
            DownloadKind::Audio => "MP3",
            DownloadKind::Video => "VIDEO",
            DownloadKind::Single => "FILE",
        }
    }

    pub fn needs_video(self) -> bool {
        matches!(self, DownloadKind::Merged | DownloadKind::Video)
    }

    pub fn needs_audio(self) -> bool {
        matches!(self, DownloadKind::Merged | DownloadKind::Audio)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadRequest {
    pub url: String,
    pub title: String,
    pub kind: DownloadKind,
    /// yt-dlp format id of the video stream (`Single`: the combined stream).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_format: Option<String>,
    /// File name without extension.
    pub name: String,
    pub dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trim_start: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trim_end: Option<f64>,
    /// Sum of the chosen streams' sizes, when yt-dlp knows them. Used as the
    /// bar's total while the second stream of a merge has not started yet.
    #[serde(default)]
    pub expected_size: u64,
}

/// `<Title-Slug>-YYYY-MM-DD-HH-MM-SS`, the Node CLI's default name.
pub fn default_name(title: &str) -> String {
    let stamp = chrono::Local::now().format("%Y-%m-%d-%H-%M-%S");
    format!("{}-{stamp}", title_slug(title))
}

/// Accept a user-typed name with or without the final extension.
pub fn clean_name(raw: &str, ext: Option<&str>) -> String {
    let mut name = raw.trim();
    if let Some(ext) = ext {
        let suffix = format!(".{ext}");
        if name.len() > suffix.len() && name.to_ascii_lowercase().ends_with(&suffix) {
            name = &name[..name.len() - suffix.len()];
        }
    }
    sanitize_file_name(name)
}

impl DownloadRequest {
    pub fn trim(&self) -> TrimRange {
        TrimRange {
            start: self.trim_start,
            end: self.trim_end,
        }
    }

    /// yt-dlp `-f`. Falls back to sensible "best" selectors when no format
    /// was picked (CLI `get` without `--video`/`--audio`).
    pub fn format_selector(&self) -> String {
        let video = self.video_format.as_deref();
        let audio = self.audio_format.as_deref();
        match self.kind {
            DownloadKind::Merged => match (video, audio) {
                (Some(v), Some(a)) => format!("{v}+{a}"),
                (Some(v), None) => format!("{v}+ba[ext=m4a]/{v}+ba"),
                (None, Some(a)) => format!("bv*[ext=mp4]+{a}/bv*+{a}"),
                (None, None) => "bv*[ext=mp4]+ba[ext=m4a]/bv*+ba/b".to_string(),
            },
            DownloadKind::Audio => audio.unwrap_or("ba/b").to_string(),
            DownloadKind::Video => video.unwrap_or("bv*/b").to_string(),
            DownloadKind::Single => video.unwrap_or("b").to_string(),
        }
    }

    /// Final extension when the kind fixes it.
    pub fn fixed_ext(&self) -> Option<&'static str> {
        match self.kind {
            DownloadKind::Merged => Some("mp4"),
            DownloadKind::Audio => Some("mp3"),
            DownloadKind::Video | DownloadKind::Single => None,
        }
    }

    /// This job's own folder for everything yt-dlp writes before the final
    /// move: `.part` files, the streams of a merge, an mp3 job's source
    /// audio. Cleanup deletes this folder and nothing else, so a finished
    /// file in the download folder can never be mistaken for a leftover.
    ///
    /// Named by a short hash of the file name, not the name itself: the name
    /// already appears in every partial file's name, and repeating it would
    /// push long titles past Windows' 260-character path limit. The hash is
    /// stable across restarts (resume finds its files again), and the folder
    /// stays inside `.ytd-parts` whatever `name` holds (`..`, a separator,
    /// or an empty name from a damaged `jobs.json`).
    pub fn partial_dir(&self) -> PathBuf {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.name.as_bytes());
        let folder: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
        self.dir.join(PARTS_DIR).join(folder)
    }

    /// Every yt-dlp argument for this download, URL last.
    pub fn ytdlp_args(&self, tools: &ToolPaths) -> Vec<OsString> {
        let mut args: Vec<OsString> = common_args(tools, &self.url);
        let mut home = OsString::from("home:");
        home.push(self.dir.as_os_str());
        let mut temp = OsString::from("temp:");
        temp.push(self.partial_dir().as_os_str());
        args.extend(
            [
                "--no-playlist",
                "--newline",
                "--progress",
                "--continue",
                "--no-mtime",
                "--progress-template",
                "download:[ytd] %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s %(progress.eta)s %(info.format_id)s",
                "--progress-template",
                "postprocess:[ytd-pp] %(progress.status)s %(progress.postprocessor)s",
                "--print",
                "after_move:[ytd-file] %(filepath)s",
                "-f",
            ]
            .map(OsString::from),
        );
        args.push(self.format_selector().into());
        match self.kind {
            DownloadKind::Merged => {
                args.push("--merge-output-format".into());
                args.push("mp4".into());
            }
            DownloadKind::Audio => {
                args.extend(
                    ["-x", "--audio-format", "mp3", "--audio-quality", "0"].map(OsString::from),
                );
            }
            DownloadKind::Video | DownloadKind::Single => {}
        }
        if let Some(sections) = self.trim().sections_arg() {
            args.push("--download-sections".into());
            args.push(sections.into());
            args.push("--force-keyframes-at-cuts".into());
        }
        // `-o` must stay relative: an absolute template makes yt-dlp ignore
        // both paths and write the partial files next to the final one.
        args.push("-P".into());
        args.push(home);
        args.push("-P".into());
        args.push(temp);
        args.push("-o".into());
        args.push(format!("{}.%(ext)s", self.name).into());
        args.push("--".into());
        args.push(self.url.clone().into());
        args
    }
}

/// Folder (inside the download folder) that holds each job's partial files.
pub const PARTS_DIR: &str = ".ytd-parts";

/// YouTube's own hosts. The sign-in cookies are only passed for these.
pub fn is_youtube_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    ["youtube.com", "youtu.be", "youtube-nocookie.com"]
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

/// Arguments every yt-dlp call for `url` gets: our ffmpeg and deno, UTF-8
/// output, and the YouTube sign-in when `url` is a YouTube link (other
/// sites never get the browser's cookies).
pub fn common_args(tools: &ToolPaths, url: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--encoding".into(),
        "utf-8".into(),
        "--no-colors".into(),
        "--ffmpeg-location".into(),
        tools.ffmpeg_dir().into_os_string(),
    ];
    if let Some(deno) = &tools.deno {
        let mut runtime = OsString::from("deno:");
        runtime.push(deno.as_os_str());
        args.push("--js-runtimes".into());
        args.push(runtime);
    }
    if let Some(cookies) = tools.cookies.as_ref().filter(|_| is_youtube_url(url)) {
        args.extend(cookies.args());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> ToolPaths {
        ToolPaths {
            ytdlp: PathBuf::from("/t/yt-dlp"),
            ffmpeg: PathBuf::from("/t/bin/ffmpeg"),
            deno: Some(PathBuf::from("/t/deno")),
            cookies: None,
        }
    }

    fn request(kind: DownloadKind) -> DownloadRequest {
        DownloadRequest {
            url: "https://www.youtube.com/watch?v=jNQXAC9IVRw".into(),
            title: "Me at the zoo".into(),
            kind,
            video_format: Some("137".into()),
            audio_format: Some("140".into()),
            name: "zoo".into(),
            dir: PathBuf::from("/out"),
            trim_start: None,
            trim_end: None,
            expected_size: 0,
        }
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn selectors_per_kind() {
        assert_eq!(request(DownloadKind::Merged).format_selector(), "137+140");
        assert_eq!(request(DownloadKind::Audio).format_selector(), "140");
        assert_eq!(request(DownloadKind::Video).format_selector(), "137");
        let mut best = request(DownloadKind::Merged);
        best.video_format = None;
        best.audio_format = None;
        assert_eq!(best.format_selector(), "bv*[ext=mp4]+ba[ext=m4a]/bv*+ba/b");
    }

    #[test]
    fn merged_args_include_tools_merge_and_url_last() {
        let args = strings(&request(DownloadKind::Merged).ytdlp_args(&tools()));
        let joined = args.join(" ");
        assert!(joined.contains("--merge-output-format mp4"));
        assert!(joined.contains("--js-runtimes deno:"));
        assert!(
            args.iter().any(|a| a.ends_with("bin")),
            "ffmpeg folder, not the exe"
        );
        assert_eq!(
            args.last().unwrap(),
            "https://www.youtube.com/watch?v=jNQXAC9IVRw"
        );
        assert_eq!(
            args[args.len() - 2],
            "--",
            "a URL can never be read as an option"
        );
        assert!(!joined.contains("--download-sections"));
    }

    #[test]
    fn sign_in_is_passed_to_youtube_only() {
        let mut with_cookies = tools();
        let youtube = "https://www.youtube.com/watch?v=x";
        assert!(!strings(&common_args(&with_cookies, youtube))
            .join(" ")
            .contains("--cookies"));
        with_cookies.cookies = Some(crate::cookies::CookieSource::Browser {
            browser: "firefox".into(),
        });
        let joined = strings(&request(DownloadKind::Merged).ytdlp_args(&with_cookies)).join(" ");
        assert!(joined.contains("--cookies-from-browser firefox"));
        for other in [
            "https://vimeo.com/1",
            "https://notyoutube.com/watch?v=x",
            "https://youtube.com.evil.example/watch?v=x",
            "not a url",
        ] {
            let joined = strings(&common_args(&with_cookies, other)).join(" ");
            assert!(!joined.contains("--cookies"), "{other}");
        }
    }

    #[test]
    fn youtube_hosts() {
        assert!(is_youtube_url("https://youtu.be/x"));
        assert!(is_youtube_url("https://music.youtube.com/watch?v=x"));
        assert!(is_youtube_url("https://m.YouTube.com./watch?v=x"));
        assert!(is_youtube_url("https://www.youtube-nocookie.com/embed/x"));
        assert!(!is_youtube_url("https://fakeyoutube.com/x"));
    }

    #[test]
    fn partial_files_go_to_the_jobs_own_folder() {
        let req = request(DownloadKind::Merged);
        let parts = req.partial_dir();
        assert_eq!(
            parts.parent().unwrap(),
            PathBuf::from("/out").join(".ytd-parts")
        );
        assert_eq!(
            parts,
            req.clone().partial_dir(),
            "stable, so resume finds it"
        );
        let args = strings(&req.ytdlp_args(&tools()));
        let at = |flag: &str| args.iter().position(|a| a == flag).unwrap();
        assert_eq!(args[at("-o") + 1], "zoo.%(ext)s", "relative, so -P applies");
        let paths: Vec<&String> = args
            .iter()
            .enumerate()
            .filter(|(i, _)| *i > 0 && args[i - 1] == "-P")
            .map(|(_, a)| a)
            .collect();
        assert_eq!(paths.len(), 2);
        assert!(paths[0].starts_with("home:") && paths[0].ends_with("out"));
        assert_eq!(*paths[1], format!("temp:{}", parts.display()));
    }

    #[test]
    fn partial_folder_has_a_fixed_short_name_whatever_the_job_name() {
        let mut long = request(DownloadKind::Merged);
        long.name = "x".repeat(150);
        let mut other = long.clone();
        other.name = "y".repeat(150);
        let parts = PathBuf::from("/out").join(".ytd-parts");
        for req in [&long, &other] {
            let dir = req.partial_dir();
            assert_eq!(dir.parent().unwrap(), parts);
            assert_eq!(dir.file_name().unwrap().len(), 12);
        }
        assert_ne!(long.partial_dir(), other.partial_dir());
        for bad in ["..", ".", "", "a/../..", "/abs"] {
            let mut req = request(DownloadKind::Merged);
            req.name = bad.into();
            assert_eq!(req.partial_dir().parent().unwrap(), parts, "{bad:?}");
        }
    }

    #[test]
    fn audio_and_trim_args() {
        let mut req = request(DownloadKind::Audio);
        req.trim_start = Some(3.0);
        req.trim_end = Some(10.0);
        let joined = strings(&req.ytdlp_args(&tools())).join(" ");
        assert!(joined.contains("-x --audio-format mp3"));
        assert!(joined.contains("--download-sections *3-10 --force-keyframes-at-cuts"));
    }

    #[test]
    fn names() {
        assert_eq!(clean_name("clip.mp4", Some("mp4")), "clip");
        assert_eq!(clean_name("clip.MP4", Some("mp4")), "clip");
        assert_eq!(clean_name("clip.mp3", Some("mp4")), "clip.mp3");
        assert_eq!(clean_name("../x", None), "-x");
        assert!(default_name("Me at the zoo").starts_with("Me-at-the-zoo-20"));
    }

    #[test]
    fn request_round_trips_as_json() {
        let req = request(DownloadKind::Merged);
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"kind\":\"merged\""));
        let back: DownloadRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }
}
