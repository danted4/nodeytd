# Changelog

## Unreleased

- The repository is now `danted4/rustYTD` (renamed from `nodeytd`). App updates are checked there; v4.0.0 builds still reach it through GitHub's redirect.

## 4.0.0

A full rewrite in Rust that replaces the Node CLI.

- **Desktop GUI (egui):** paste a link and get a preview with thumbnail, title, channel and duration. Choose merged mp4, mp3, video only or a single file, then the quality, a file name and trim times.
- **Download queue:** parallel downloads with a limit in settings (default 2). Pause keeps the partial file and resume continues from it. Stop cancels and removes partial files, which live in a per-download folder so a finished file is never deleted. The list is saved between runs.
- **CLI:** keeps the Node CLI's interactive prompts, and adds `ytd get`, `ytd formats` and `ytd tools`.
- **No manual setup:** yt-dlp, ffmpeg (Windows and Linux) and deno are installed on first run from their GitHub releases and checked against the published SHA-256 sums. On macOS, install ffmpeg with `brew install ffmpeg`.
- **YouTube sign-in:** when YouTube asks to "confirm you're not a bot", ytd offers the browsers installed on the computer and uses that browser's YouTube cookies. Each choice is tested before it is saved, with plain-words reasons when one fails. The cookies are only passed for YouTube links. A cookies.txt import works with any browser. CLI: `ytd cookies …`, `--cookies-from`, `--cookies`.
- **Auto-update toggle (off by default):** updates ytd itself and the tools it manages.
- **Settings:** download folder (default `Downloads/rustYTD`), downloads at once, five colour themes, tool versions and "update tools".
- **Trimming:** only the chosen section is downloaded (`--download-sections`), instead of downloading the whole video and trimming it afterwards.
- **Releases:** Windows x64, macOS (Apple silicon and Intel, with an `.app` bundle) and Linux x64, with `SHA256SUMS`.

## 3.0.0

- Upgraded `@distube/ytdl-core` to v5 so downloads work again. Requires Node 26+.

## 2.0.0 and earlier

- Node CLI built on `@distube/ytdl-core`.
