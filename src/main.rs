use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use dialoguer::{Input, Select};
use indicatif::{ProgressBar, ProgressStyle};

use ytd::config;
use ytd::progress::ProgressHandle;
use ytd::request::{clean_name, default_name, DownloadKind, DownloadRequest};
use ytd::tools::{self, Tool, ToolPaths};
use ytd::trim::{parse_flexible_time, TrimRange};
use ytd::ytdlp::{self, Format, VideoInfo};

/// Download videos with yt-dlp and ffmpeg. Run without a command for the
/// interactive prompts.
#[derive(Parser)]
#[command(name = "ytd", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Download one video without prompts (best quality unless told otherwise).
    Get {
        url: String,
        /// What to download.
        #[arg(long, value_enum, default_value_t = DownloadKind::Merged)]
        kind: DownloadKind,
        /// Video stream: a format id from `ytd formats`, a height like 1080p, or best.
        #[arg(long)]
        video: Option<String>,
        /// Audio stream: a format id from `ytd formats`, or best.
        #[arg(long)]
        audio: Option<String>,
        /// Output file name (extension optional).
        #[arg(long)]
        name: Option<String>,
        /// Trim start: 3, 3:00 or 1:02:03.
        #[arg(long)]
        start: Option<String>,
        /// Trim end: 3, 3:00 or 1:02:03.
        #[arg(long)]
        end: Option<String>,
        /// Folder to save into (default: the configured download folder).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// List the formats a video offers.
    Formats { url: String },
    /// Show, install or update yt-dlp, ffmpeg and deno.
    Tools {
        #[command(subcommand)]
        action: Option<ToolsAction>,
    },
}

#[derive(Subcommand, Clone, Copy)]
enum ToolsAction {
    /// Where each tool is and which version is installed.
    Status,
    /// Install whatever is missing.
    Install,
    /// Re-install the tools ytd manages to their latest releases.
    Update,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        None => interactive().await,
        Some(Command::Formats { url }) => {
            let tools = ensure_tools().await?;
            let info = fetch(&tools, &url).await?;
            print_formats(&info);
            Ok(())
        }
        Some(Command::Tools { action }) => {
            tools_command(action.unwrap_or(ToolsAction::Status)).await
        }
        Some(Command::Get {
            url,
            kind,
            video,
            audio,
            name,
            start,
            end,
            dir,
        }) => {
            let tools = ensure_tools().await?;
            let info = fetch(&tools, &url).await?;
            let trim =
                TrimRange::parse(start.as_deref().unwrap_or(""), end.as_deref().unwrap_or(""))?;
            let video = match (kind, video.as_deref()) {
                (DownloadKind::Audio, _) => None,
                (_, Some(spec)) => Some(pick(&info.video_choices(kind), spec)?.format_id.clone()),
                (_, None) => None,
            };
            let audio = match (kind.needs_audio(), audio.as_deref()) {
                (true, Some(spec)) => {
                    Some(pick(&info.audio_choices(kind), spec)?.format_id.clone())
                }
                _ => None,
            };
            let request =
                build_request(&info, &url, kind, video, audio, name.as_deref(), trim, dir);
            download(&tools, &request).await
        }
    }
}

fn print_formats(info: &VideoInfo) {
    println!("{}", info.title);
    println!("{:<12} {:<6} details", "id", "type");
    for format in &info.formats {
        let kind = match (format.has_video(), format.has_audio()) {
            (true, true) => "both",
            (true, false) => "video",
            (false, true) => "audio",
            (false, false) => continue,
        };
        println!("{:<12} {:<6} {}", format.format_id, kind, format.label());
    }
}

/// A format by id, by height (`1080p`), or `best`.
fn pick<'a>(choices: &[&'a Format], spec: &str) -> Result<&'a Format> {
    let spec = spec.trim();
    if choices.is_empty() {
        bail!("this video has no formats for that choice");
    }
    if spec.eq_ignore_ascii_case("best") {
        return Ok(choices[0]);
    }
    if let Some(found) = choices.iter().find(|f| f.format_id == spec) {
        return Ok(found);
    }
    if let Some(height) = spec
        .strip_suffix(['p', 'P'])
        .and_then(|h| h.parse::<u32>().ok())
    {
        if let Some(found) = choices.iter().find(|f| f.height == Some(height)) {
            return Ok(found);
        }
    }
    let ids: Vec<&str> = choices.iter().map(|f| f.format_id.as_str()).collect();
    bail!("no format '{spec}' (choose from: {})", ids.join(", "))
}

#[allow(clippy::too_many_arguments)]
fn build_request(
    info: &VideoInfo,
    url: &str,
    kind: DownloadKind,
    video: Option<String>,
    audio: Option<String>,
    name: Option<&str>,
    trim: TrimRange,
    dir: Option<PathBuf>,
) -> DownloadRequest {
    let ext = output_ext(info, kind, video.as_deref());
    let name = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => clean_name(name, ext.as_deref()),
        None => default_name(&info.title),
    };
    let expected_size = [video.as_deref(), audio.as_deref()]
        .into_iter()
        .flatten()
        .filter_map(|id| info.format(id).and_then(Format::size))
        .sum();
    DownloadRequest {
        url: url.to_string(),
        title: info.title.clone(),
        kind,
        video_format: video,
        audio_format: audio,
        name,
        dir: dir.unwrap_or_else(|| config::load_config().download_dir()),
        trim_start: trim.start,
        trim_end: trim.end,
        expected_size,
    }
}

fn output_ext(info: &VideoInfo, kind: DownloadKind, video: Option<&str>) -> Option<String> {
    match kind {
        DownloadKind::Merged => Some("mp4".into()),
        DownloadKind::Audio => Some("mp3".into()),
        _ => video.and_then(|id| info.format(id)).map(|f| f.ext.clone()),
    }
}

async fn fetch(tools: &ToolPaths, url: &str) -> Result<VideoInfo> {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    spinner.set_message("Fetching video info…");
    spinner.enable_steady_tick(Duration::from_millis(100));
    let result = ytdlp::fetch_info(tools, url).await;
    spinner.finish_and_clear();
    result
}

async fn download(tools: &ToolPaths, request: &DownloadRequest) -> Result<()> {
    let progress = ProgressHandle::cli("starting");
    match ytdlp::download(tools, request, &progress).await {
        Ok(path) => {
            progress.finish();
            println!("Saved as {}", path.display());
            Ok(())
        }
        Err(error) => {
            progress.abandon();
            Err(error)
        }
    }
}

/// Tools, installing what is missing on first use.
async fn ensure_tools() -> Result<ToolPaths> {
    let paths = match tools::resolve() {
        Ok(paths) if paths.deno.is_some() || !tools::can_install(Tool::Deno) => paths,
        _ => {
            println!("Setting up tools in {}", config::tools_dir().display());
            tools::install_missing(|tool| ProgressHandle::cli(tool.key())).await?
        }
    };
    if paths.deno.is_none() {
        eprintln!("Warning: deno not found; some YouTube formats may be missing.");
    }
    Ok(paths)
}

async fn tools_command(action: ToolsAction) -> Result<()> {
    match action {
        ToolsAction::Status => {
            let config = config::load_config();
            println!("tools folder: {}", config::tools_dir().display());
            for tool in Tool::ALL {
                let location = tools::locate(tool)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "missing".into());
                let version = config
                    .tools
                    .get(tool.key())
                    .map(|r| format!(" ({}, installed {})", r.version, r.installed_at))
                    .unwrap_or_default();
                println!("{:<7} {location}{version}", tool.key());
            }
            Ok(())
        }
        ToolsAction::Install => {
            let missing = tools::missing();
            if missing.is_empty() {
                println!("All tools are installed.");
                return Ok(());
            }
            tools::install_missing(|tool| ProgressHandle::cli(tool.key())).await?;
            println!("Tools ready.");
            Ok(())
        }
        ToolsAction::Update => {
            tools::update_all(|tool| ProgressHandle::cli(tool.key())).await?;
            println!("Tools updated.");
            Ok(())
        }
    }
}

/// The Node CLI's flow: URL, type, quality, name, trim.
async fn interactive() -> Result<()> {
    let url: String = Input::new()
        .with_prompt("Enter the video URL")
        .validate_with(|value: &String| -> Result<(), &str> {
            let value = value.trim();
            if value.starts_with("http://") || value.starts_with("https://") {
                Ok(())
            } else {
                Err("Invalid URL!")
            }
        })
        .interact_text()?;
    let url = url.trim().to_string();
    let tools = ensure_tools().await?;
    let info = fetch(&tools, &url).await?;
    println!("{}  {}", info.title, info.duration_text());

    let kinds: Vec<&str> = DownloadKind::ALL.iter().map(|k| k.label()).collect();
    let kind = DownloadKind::ALL[Select::new()
        .with_prompt("What do you want to download?")
        .items(&kinds)
        .default(0)
        .interact()?];

    let video = if kind == DownloadKind::Audio {
        None
    } else {
        let prompt = if kind == DownloadKind::Single {
            "Choose the format"
        } else {
            "Choose the video resolution"
        };
        Some(choose(&info.video_choices(kind), prompt)?)
    };
    let audio = if kind.needs_audio() {
        Some(choose(
            &info.audio_choices(kind),
            "Choose the audio quality",
        )?)
    } else {
        None
    };

    let ext = output_ext(&info, kind, video.as_deref());
    let suggested = match &ext {
        Some(ext) => format!("{}.{ext}", default_name(&info.title)),
        None => default_name(&info.title),
    };
    let name: String = Input::new()
        .with_prompt("Rename output file (leave as is for default)")
        .default(suggested)
        .interact_text()?;

    let trim = loop {
        let start = ask_time("Trim start time (blank for start)")?;
        let end = ask_time("Trim end time (blank for end)")?;
        match TrimRange::parse(&start, &end) {
            Ok(range) => break range,
            Err(error) => eprintln!("{error}"),
        }
    };

    let request = build_request(&info, &url, kind, video, audio, Some(&name), trim, None);
    download(&tools, &request).await
}

fn choose(choices: &[&Format], prompt: &str) -> Result<String> {
    if choices.is_empty() {
        bail!("no compatible formats for this choice; try another download type");
    }
    let labels: Vec<String> = choices.iter().map(|f| f.label()).collect();
    let index = Select::new()
        .with_prompt(prompt)
        .items(&labels)
        .default(0)
        .interact()
        .context("selection cancelled")?;
    Ok(choices[index].format_id.clone())
}

fn ask_time(prompt: &str) -> Result<String> {
    Ok(Input::<String>::new()
        .with_prompt(prompt)
        .allow_empty(true)
        .validate_with(|value: &String| -> Result<(), String> {
            parse_flexible_time(value)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .interact_text()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(id: &str, height: u32) -> Format {
        Format {
            format_id: id.into(),
            height: Some(height),
            ..Default::default()
        }
    }

    #[test]
    fn pick_by_id_height_or_best() {
        let a = fmt("299", 1080);
        let b = fmt("136", 720);
        let list = vec![&a, &b];
        assert_eq!(pick(&list, "best").unwrap().format_id, "299");
        assert_eq!(pick(&list, "720p").unwrap().format_id, "136");
        assert_eq!(pick(&list, "136").unwrap().format_id, "136");
        assert!(pick(&list, "480p").is_err());
        assert!(pick(&[], "best").is_err());
    }
}
