# Changelog

## 4.1.1

- **update tools only downloads what is out of date:** pressing it now asks GitHub for the latest release of each tool and downloads only the ones that are missing or behind (or have no recorded version). When everything is current it says "tools are up to date" instead of downloading yt-dlp, ffmpeg and deno again. The check runs only when you press it. `ytd tools update` works the same way; `ytd tools update --force` re-installs everything as before.

## 4.1.0

- **Update 4.0.x by hand once:** 4.0.0 and 4.0.1 find this release through GitHub's redirect but refuse to install it, because its download links name the new repository, so their auto-update fails. Download 4.1.0 from the [release page](https://github.com/danted4/ytd/releases/latest) once; updates work again from 4.1.0 on.
- **New name: ytd.** The repository is now `danted4/ytd` (renamed from `rustYTD`), and the header, window title and CLI show `danted4/ytd v4.1.0`. The binaries, `ytd.app` and the settings folder were already called ytd.
- **New default download folder:** `Downloads/ytd` instead of `Downloads/rustYTD` (`Downloads/ytd-downloads` if a file named `ytd`, such as the CLI, is already there). A folder you chose yourself is kept; downloads paused in 4.0.x resume in their old folder. A download folder that is a file now fails with a clear message.
- **New icon:** a "Y" sitting in the download tray, in torrenter's shape and ytd's coral colours.
- **Download folder on the main page:** the folder sits under the link box, as in torrenter. Click it to choose another one.
- **Link box:** it has a border that lights up on hover and focus, and a **×** that clears it. The × or Esc starts over, also while a preview is loading. **dismiss** now clears the link too.
- **Queue search:** filter downloads by title, link, folder, type, date or status.
- **No more stuck 0%:** a new download shows **starting** with a sliding bar until the transfer begins, instead of an empty bar. A trimmed download, which has no size until it ends, keeps a sliding bar. **merging** and **converting** now show while ffmpeg finishes a file (yt-dlp reports them on stderr, which ytd did not read before).
- Long titles in the queue are cut short instead of running under the buttons.
- **macOS window glass:** the window is frosted, as in torrenter, with a **window tint** setting (0–100%, 60% by default, 100% in fullscreen). Windows and Linux stay opaque.
- **Settings is a page:** the cog opens it with tabs for appearance, downloads, YouTube sign-in and updates; the title or **← settings** goes back. The download folder moved to the main page. Settings are saved in order off the window's thread, and quitting waits for a save still in flight.
- **ffmpeg on macOS without Homebrew:** on a Mac with no ffmpeg, ytd installs its own static build from [danted4/ffmpeg-macos](https://github.com/danted4/ffmpeg-macos) (Apple silicon and Intel, checked against the release's SHA256SUMS). It is installed only when no ffmpeg is found: a Mac that already has one, such as Homebrew's, keeps using it.

## 4.0.1

- The GUI header, the window title and the CLI start with the `danted4/rustYTD v4.0.1` signature, as in torrenter.
- The repository is now `danted4/rustYTD` (renamed from `nodeytd`). Self-update now checks it directly (v4.0.0 reached it through GitHub's redirect).

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
