# AGENTS.md

Guidance for coding agents working in this repo. `readme.md` describes the product's behaviour. This file covers how to build it and which rules must hold.

## Commands

```bash
cargo fmt --all
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked --all-targets --all-features
cargo run -- get <url> --kind merged            # CLI one-shot
cargo run --features gui --bin ytd-gui          # GUI
```

Always pass `--all-features` to clippy and tests so the GUI module compiles, the same way CI does.

## Layout

| File | Role |
|---|---|
| `src/tools.rs` | finds, installs and updates yt-dlp, ffmpeg and deno (pinned GitHub hosts, SHA-256 checks, size caps) |
| `src/ytdlp.rs` | `fetch_info` (`yt-dlp -J`) and `download` (progress parsing, pause and stop by killing the whole process tree) |
| `src/cookies.rs` | YouTube sign-in: `CookieSource` (a browser or a cookies.txt file), installed-browser detection per OS, and plain-words explanations of yt-dlp's cookie errors |
| `src/request.rs` | `DownloadRequest` and the yt-dlp arguments it maps to |
| `src/session.rs` | the GUI queue: synchronous API, an engine thread with its own Tokio runtime, and `Slots` for the parallel limit |
| `src/progress.rs` | `JobSnapshot`, `ProgressHandle`, the pause and stop errors, and formatters |
| `src/jobs.rs`, `src/config.rs` | `jobs.json` and `config.json`, written with `fsutil::atomic_write` |
| `src/update.rs` | app self-update for the auto-update toggle |
| `src/gui.rs` | egui front. Reads snapshots only and never blocks on I/O |
| `src/main.rs` | clap CLI |
| `.github/workflows/` | `ci.yml` (fmt, clippy, tests on 3 OSes) and `release.yml` (tag `v*.*.*` builds the assets and `SHA256SUMS`) |

## Rules that must hold

- Never block the GUI thread. Network and process work goes to the worker thread or `Session`.
- Fronts only read `JobSnapshot`s. Pause and stop are flags that surface as the `DownloadPaused` and `DownloadStopped` errors, never as failures.
- Killing yt-dlp kills its whole process tree (`ytdlp::kill_tree`). The release builds are PyInstaller launchers with a child process.
- Tool and app downloads only come from `https://github.com/<repo>/releases/download/`, with a SHA-256 check and a size cap. Never widen this to other hosts.
- reqwest uses rustls only, a 20 s connect and 60 s read timeout, and refuses https-to-http redirects.
- `config.json` and `jobs.json` keep unknown keys on rewrite. A file that does not parse is moved aside, never fatal.
- Use `anyhow` at boundaries. No `unwrap` on I/O outside tests.
- Release asset names must match `update::asset_name` and the table in `readme.md`.
- Tests live in the same file. Network tests use the in-process HTTP server in `tools.rs` and never hit real GitHub or YouTube.

## Commits

Short, imperative, about why ("fix resume after pause", "kill yt-dlp's child on stop"). When behaviour changes, update `readme.md` and `CHANGELOG.md` in the same change.
