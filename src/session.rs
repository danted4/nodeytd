//! The GUI's download queue.
//!
//! `Session` is synchronous on its public side (state in
//! `Arc<Mutex<Inner>>`), so the GUI thread never awaits. A background
//! `engine_loop` thread owns a Tokio runtime and receives [`EngineCmd`]s over
//! a channel; each download waits for a [`Slots`] slot, runs yt-dlp, and
//! reports through its `JobSnapshot`. Callers poll [`Session::drain_events`]
//! and [`Session::persist`] periodically.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::jobs::{self, JobList, LoadJobs, PersistedJob};
use crate::progress::{self, JobSnapshot, ProgressHandle, QUEUED, STARTING};
use crate::request::DownloadRequest;
use crate::slots::Slots;
use crate::tools::ToolPaths;
use crate::ytdlp;

const PERSIST_EVERY: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct Job {
    pub id: u64,
    pub request: DownloadRequest,
    pub snapshot: Arc<JobSnapshot>,
    pub added: String,
    /// Fields a newer version wrote on this record: written back as-is.
    pub extra: BTreeMap<String, serde_json::Value>,
}

enum EngineCmd {
    Download {
        id: u64,
        request: Box<DownloadRequest>,
        snapshot: Arc<JobSnapshot>,
        tools: ToolPaths,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    Finished {
        id: u64,
        title: String,
        path: PathBuf,
    },
    Failed {
        id: u64,
        title: String,
        error: String,
    },
}

#[derive(Debug)]
pub enum Hydrate {
    Empty,
    Loaded(usize),
    Corrupt(String),
}

struct Inner {
    jobs: Vec<Job>,
    next_id: u64,
    tools: Option<ToolPaths>,
    /// Records and top-level keys from `jobs.json` this version cannot read.
    foreign: Vec<serde_json::Value>,
    file_extra: BTreeMap<String, serde_json::Value>,
    last_persist: Option<Instant>,
    last_fingerprint: String,
}

impl Inner {
    /// Another active job (other than `except`) writes the same file.
    fn clashes(&self, request: &DownloadRequest, except: Option<u64>) -> bool {
        self.jobs.iter().any(|job| {
            Some(job.id) != except
                && job.snapshot.is_active()
                && job.request.dir == request.dir
                && job.request.name == request.name
        })
    }
}

pub struct Session {
    inner: Arc<Mutex<Inner>>,
    engine: Sender<EngineCmd>,
    events: Mutex<Receiver<SessionEvent>>,
    slots: Arc<Slots>,
    jobs_path: PathBuf,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Session {
    pub fn open(jobs_path: PathBuf, max_downloads: u8) -> Self {
        let (engine, commands) = mpsc::channel();
        let (events_tx, events) = mpsc::channel();
        let slots = Slots::new(max_downloads);
        let engine_slots = Arc::clone(&slots);
        std::thread::Builder::new()
            .name("ytd-engine".into())
            .spawn(move || engine_loop(commands, events_tx, engine_slots))
            .expect("failed to start the download engine thread");
        Self {
            inner: Arc::new(Mutex::new(Inner {
                jobs: Vec::new(),
                next_id: 1,
                tools: None,
                foreign: Vec::new(),
                file_extra: BTreeMap::new(),
                last_persist: None,
                last_fingerprint: String::new(),
            })),
            engine,
            events: Mutex::new(events),
            slots,
            jobs_path,
        }
    }

    pub fn set_tools(&self, tools: ToolPaths) {
        lock(&self.inner).tools = Some(tools);
    }

    pub fn tools(&self) -> Option<ToolPaths> {
        lock(&self.inner).tools.clone()
    }

    pub fn max_downloads(&self) -> u8 {
        self.slots.cap()
    }

    pub fn set_max_downloads(&self, cap: u8) {
        self.slots.set_cap(cap);
    }

    /// Restore the saved list. Unfinished jobs come back paused.
    pub fn hydrate(&self) -> Hydrate {
        match jobs::load_jobs_from(&self.jobs_path) {
            LoadJobs::Empty => Hydrate::Empty,
            LoadJobs::Corrupt(error) => Hydrate::Corrupt(error),
            LoadJobs::Loaded(list) => {
                let mut inner = lock(&self.inner);
                inner.next_id = inner.next_id.max(list.max_id() + 1);
                inner.foreign = list.foreign;
                inner.file_extra = list.extra;
                let count = list.jobs.len();
                for record in list.jobs {
                    let snapshot = Arc::new(JobSnapshot::default());
                    snapshot.set_title(record.request.title.clone());
                    snapshot.set_output(record.output.clone());
                    snapshot.restore(
                        record.downloaded,
                        record.total,
                        &record.status,
                        record.error.clone(),
                    );
                    inner.jobs.push(Job {
                        id: record.id,
                        request: record.request,
                        snapshot,
                        added: record.added,
                        extra: record.extra,
                    });
                }
                Hydrate::Loaded(count)
            }
        }
    }

    /// Add a download and start it (it waits for a slot). Refuses a second
    /// active job writing the same file.
    pub fn queue(&self, request: DownloadRequest) -> Result<u64, String> {
        let tools = self
            .tools()
            .ok_or_else(|| "tools are not ready yet".to_string())?;
        let mut inner = lock(&self.inner);
        if inner.clashes(&request, None) {
            return Err(format!("'{}' is already in the queue", request.name));
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let snapshot = Arc::new(JobSnapshot::default());
        snapshot.set_title(request.title.clone());
        let added = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
        inner.jobs.push(Job {
            id,
            request: request.clone(),
            snapshot: Arc::clone(&snapshot),
            added,
            extra: BTreeMap::new(),
        });
        drop(inner);
        self.dispatch(id, request, snapshot, tools);
        Ok(id)
    }

    fn dispatch(
        &self,
        id: u64,
        request: DownloadRequest,
        snapshot: Arc<JobSnapshot>,
        tools: ToolPaths,
    ) {
        if !snapshot.try_claim() {
            return;
        }
        snapshot.set_status(QUEUED);
        let _ = self.engine.send(EngineCmd::Download {
            id,
            request: Box::new(request),
            snapshot,
            tools,
        });
    }

    fn find(&self, id: u64) -> Option<Job> {
        lock(&self.inner)
            .jobs
            .iter()
            .find(|job| job.id == id)
            .cloned()
    }

    /// Pause a running or queued job; resume a paused one. Resuming needs
    /// the tools: without them nothing changes and the reason comes back.
    pub fn toggle_pause(&self, id: u64) -> Result<(), String> {
        let Some(job) = self.find(id) else {
            return Ok(());
        };
        if job.snapshot.is_finished() {
            return Ok(());
        }
        if job.snapshot.is_paused() {
            let tools = self
                .tools()
                .ok_or_else(|| "tools are not ready yet".to_string())?;
            job.snapshot.set_paused(false);
            job.snapshot.set_status(QUEUED);
            self.dispatch(id, job.request, job.snapshot, tools);
        } else {
            job.snapshot.set_paused(true);
            job.snapshot.set_status(progress::PAUSED);
        }
        Ok(())
    }

    /// Cancel a job. A running one is killed by its engine task, which also
    /// removes the partial files; a paused, restored or waiting one has no
    /// task, so its partial files are removed here (off the GUI thread).
    pub fn stop(&self, id: u64) {
        let Some(job) = self.find(id) else {
            return;
        };
        if job.snapshot.is_finished() {
            return;
        }
        job.snapshot.request_stop();
        if !job.snapshot.is_running() {
            job.snapshot.mark_stopped();
            self.clean_up(id, job.request);
        }
    }

    /// Delete a dead job's partial files, unless a newer active job with the
    /// same name and folder is now using that partial folder.
    fn clean_up(&self, id: u64, request: DownloadRequest) {
        if !lock(&self.inner).clashes(&request, Some(id)) {
            remove_partials_in_background(request);
        }
    }

    /// Pause every running job and wait (up to `timeout`) for their yt-dlp
    /// process trees to end, so closing the app never leaves one behind.
    /// Paused jobs come back paused on the next start.
    pub fn shutdown(&self, timeout: Duration) {
        for job in self.jobs() {
            if job.snapshot.is_active() && !job.snapshot.is_paused() {
                job.snapshot.set_paused(true);
                job.snapshot.set_status(progress::PAUSED);
            }
        }
        let deadline = Instant::now() + timeout;
        while self.any_running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Start a finished, stopped or failed job again, under a fresh snapshot.
    pub fn restart(&self, id: u64) -> Result<(), String> {
        let tools = self
            .tools()
            .ok_or_else(|| "tools are not ready yet".to_string())?;
        let mut inner = lock(&self.inner);
        let Some(request) = inner
            .jobs
            .iter()
            .find(|job| job.id == id)
            .map(|job| job.request.clone())
        else {
            return Err("no such job".into());
        };
        if inner.clashes(&request, Some(id)) {
            return Err(format!("'{}' is already in the queue", request.name));
        }
        let Some(job) = inner.jobs.iter_mut().find(|job| job.id == id) else {
            return Err("no such job".into());
        };
        if job.snapshot.is_active() {
            return Ok(());
        }
        let snapshot = Arc::new(JobSnapshot::default());
        snapshot.set_title(job.request.title.clone());
        job.snapshot = Arc::clone(&snapshot);
        let request = job.request.clone();
        drop(inner);
        self.dispatch(id, request, snapshot, tools);
        Ok(())
    }

    /// Drop a job from the list (stopping it first if needed). Finished
    /// downloads stay on disk; an unfinished one's partial files go.
    pub fn remove(&self, id: u64) {
        let Some(job) = self.find(id) else {
            return;
        };
        if job.snapshot.is_finished() {
            if !job.snapshot.is_done() {
                self.clean_up(id, job.request);
            }
        } else {
            self.stop(id);
        }
        lock(&self.inner).jobs.retain(|job| job.id != id);
        self.persist(true);
    }

    /// Remove every finished job from the list.
    pub fn clear_finished(&self) {
        lock(&self.inner)
            .jobs
            .retain(|job| !job.snapshot.is_finished());
        self.persist(true);
    }

    pub fn jobs(&self) -> Vec<Job> {
        lock(&self.inner).jobs.clone()
    }

    pub fn any_active(&self) -> bool {
        lock(&self.inner)
            .jobs
            .iter()
            .any(|job| job.snapshot.is_active() && !job.snapshot.is_paused())
    }

    /// Any job an engine task is working on (tools must not be replaced then).
    pub fn any_running(&self) -> bool {
        lock(&self.inner)
            .jobs
            .iter()
            .any(|job| job.snapshot.is_running())
    }

    pub fn drain_events(&self) -> Vec<SessionEvent> {
        lock(&self.events).try_iter().collect()
    }

    /// Save the list when it changed, at most every 2 s unless `force`.
    pub fn persist(&self, force: bool) {
        let mut inner = lock(&self.inner);
        let due = inner
            .last_persist
            .is_none_or(|at| at.elapsed() >= PERSIST_EVERY);
        if !force && !due {
            return;
        }
        let list = JobList {
            jobs: inner.jobs.iter().map(record).collect(),
            foreign: inner.foreign.clone(),
            extra: inner.file_extra.clone(),
        };
        let fingerprint = serde_json::to_string(&list.jobs).unwrap_or_default();
        if fingerprint == inner.last_fingerprint {
            inner.last_persist = Some(Instant::now());
            return;
        }
        if jobs::save_jobs_to(&self.jobs_path, &list).is_ok() {
            inner.last_fingerprint = fingerprint;
        }
        inner.last_persist = Some(Instant::now());
    }
}

fn record(job: &Job) -> PersistedJob {
    let snap = &job.snapshot;
    PersistedJob {
        id: job.id,
        request: job.request.clone(),
        added: job.added.clone(),
        status: snap.status(),
        downloaded: snap.downloaded.load(Ordering::Relaxed),
        total: snap.total.load(Ordering::Relaxed),
        output: snap.output(),
        error: snap.error(),
        extra: job.extra.clone(),
    }
}

fn remove_partials_in_background(request: DownloadRequest) {
    let _ = std::thread::Builder::new()
        .name("ytd-cleanup".into())
        .spawn(move || ytdlp::remove_partials(&request));
}

async fn stop_and_clean(snapshot: &JobSnapshot, request: &DownloadRequest) {
    let request = request.clone();
    let _ = tokio::task::spawn_blocking(move || ytdlp::remove_partials(&request)).await;
    snapshot.mark_stopped();
}

fn engine_loop(commands: Receiver<EngineCmd>, events: Sender<SessionEvent>, slots: Arc<Slots>) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return,
    };
    while let Ok(command) = commands.recv() {
        let events = events.clone();
        let slots = Arc::clone(&slots);
        runtime.spawn(async move {
            match command {
                EngineCmd::Download {
                    id,
                    request,
                    snapshot,
                    tools,
                } => run_job(id, *request, snapshot, tools, slots, events).await,
            }
        });
    }
}

/// One job, from waiting for a slot to done. Loops when the user resumed
/// while yt-dlp was still being killed for a pause.
async fn run_job(
    id: u64,
    request: DownloadRequest,
    snapshot: Arc<JobSnapshot>,
    tools: ToolPaths,
    slots: Arc<Slots>,
    events: Sender<SessionEvent>,
) {
    loop {
        let outcome = async {
            let _slot = slots.acquire(&snapshot).await?;
            snapshot.set_status(STARTING);
            let progress = ProgressHandle::gui(Arc::clone(&snapshot));
            ytdlp::download(&tools, &request, &progress).await
        }
        .await;
        match outcome {
            Ok(path) => {
                snapshot.set_output(Some(path.clone()));
                snapshot.mark_finished();
                let _ = events.send(SessionEvent::Finished {
                    id,
                    title: request.title.clone(),
                    path,
                });
            }
            // Also reached when the stop came while waiting for a slot, before
            // yt-dlp (and its own cleanup) ever ran for this resume.
            Err(error) if progress::is_stopped(&error) => {
                stop_and_clean(&snapshot, &request).await;
            }
            Err(error) if progress::is_pause_abort(&error) => {
                snapshot.set_status(progress::PAUSED);
            }
            Err(error) => {
                let message = format!("{error:#}");
                snapshot.set_error(message.clone());
                let _ = events.send(SessionEvent::Failed {
                    id,
                    title: request.title.clone(),
                    error: message,
                });
            }
        }
        snapshot.release_claim();
        // A stop that landed after yt-dlp's last look at the flags (it was
        // ending for a pause): `stop` saw the job running and left it to us.
        if snapshot.should_stop() && !snapshot.is_finished() {
            stop_and_clean(&snapshot, &request).await;
            return;
        }
        // Resumed while we were winding down: take the job again.
        let resumed = !snapshot.is_finished() && !snapshot.is_paused() && !snapshot.should_stop();
        if resumed && snapshot.try_claim() {
            snapshot.set_status(QUEUED);
            continue;
        }
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::DownloadKind;

    fn request(name: &str) -> DownloadRequest {
        DownloadRequest {
            url: "https://youtu.be/x".into(),
            title: "t".into(),
            kind: DownloadKind::Audio,
            video_format: None,
            audio_format: None,
            name: name.into(),
            dir: PathBuf::from("/out"),
            trim_start: None,
            trim_end: None,
            expected_size: 0,
        }
    }

    fn fake_tools(dir: &std::path::Path) -> ToolPaths {
        ToolPaths {
            ytdlp: dir.join("no-such-yt-dlp"),
            ffmpeg: dir.join("ffmpeg"),
            deno: None,
            cookies: None,
        }
    }

    #[test]
    fn queue_refuses_duplicates_and_failure_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::open(dir.path().join("jobs.json"), 1);
        assert!(session.queue(request("a")).is_err(), "no tools yet");
        session.set_tools(fake_tools(dir.path()));
        let id = session.queue(request("a")).unwrap();
        // The fake yt-dlp does not exist: the job fails quickly.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut events = Vec::new();
        while events.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            events = session.drain_events();
        }
        assert!(matches!(events.as_slice(), [SessionEvent::Failed { id: got, .. }] if *got == id));
        // A failed job no longer blocks the name.
        assert!(session.queue(request("a")).is_ok());
    }

    #[test]
    fn persist_and_hydrate_bring_jobs_back_paused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        {
            let session = Session::open(path.clone(), 1);
            let snapshot = Arc::new(JobSnapshot::default());
            snapshot.set_paused(true);
            snapshot.set_status(progress::PAUSED);
            lock(&session.inner).jobs.push(Job {
                id: 7,
                request: request("b"),
                snapshot,
                added: "now".into(),
                extra: BTreeMap::new(),
            });
            session.persist(true);
        }
        let session = Session::open(path, 1);
        assert!(matches!(session.hydrate(), Hydrate::Loaded(1)));
        let jobs = session.jobs();
        assert_eq!(jobs[0].id, 7);
        assert!(jobs[0].snapshot.is_paused());
        session.stop(7);
        assert_eq!(session.jobs()[0].snapshot.status(), progress::STOPPED);
    }

    fn paused_job(session: &Session, id: u64, request: DownloadRequest) {
        let snapshot = Arc::new(JobSnapshot::default());
        snapshot.set_paused(true);
        snapshot.set_status(progress::PAUSED);
        lock(&session.inner).jobs.push(Job {
            id,
            request,
            snapshot,
            added: "now".into(),
            extra: BTreeMap::new(),
        });
    }

    #[test]
    fn stopping_a_paused_job_removes_its_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::open(dir.path().join("jobs.json"), 1);
        let req = request_with_partial(dir.path(), "clip");
        let parts = req.partial_dir();
        std::fs::write(dir.path().join("clip.mp4"), b"x").unwrap();
        paused_job(&session, 1, req);
        session.stop(1);
        wait_until_gone(&parts);
        assert!(!parts.exists());
        assert!(
            dir.path().join("clip.mp4").exists(),
            "finished file of the same name stays"
        );
    }

    fn request_with_partial(dir: &std::path::Path, name: &str) -> DownloadRequest {
        let mut req = request(name);
        req.dir = dir.to_path_buf();
        std::fs::create_dir_all(req.partial_dir()).unwrap();
        std::fs::write(req.partial_dir().join(format!("{name}.webm.part")), b"x").unwrap();
        req
    }

    fn wait_until_gone(path: &std::path::Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn stopping_a_resumed_job_waiting_for_a_slot_removes_its_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::open(dir.path().join("jobs.json"), 1);
        session.set_tools(fake_tools(dir.path()));
        // Another download holds the only slot.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _held = runtime
            .block_on(session.slots.acquire(&JobSnapshot::default()))
            .unwrap();
        let req = request_with_partial(dir.path(), "clip");
        let parts = req.partial_dir();
        paused_job(&session, 1, req);
        session.toggle_pause(1).unwrap();
        let snapshot = session.jobs()[0].snapshot.clone();
        assert!(snapshot.is_running(), "claimed, waiting for the slot");
        session.stop(1);
        wait_until_gone(&parts);
        assert!(!parts.exists());
        let deadline = Instant::now() + Duration::from_secs(5);
        while snapshot.status() != progress::STOPPED && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(snapshot.status(), progress::STOPPED);
    }

    #[test]
    fn removing_an_old_job_keeps_the_partials_of_an_active_one_with_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::open(dir.path().join("jobs.json"), 1);
        let req = request_with_partial(dir.path(), "clip");
        let failed = Arc::new(JobSnapshot::default());
        failed.set_error("boom");
        lock(&session.inner).jobs.push(Job {
            id: 1,
            request: req.clone(),
            snapshot: failed,
            added: "now".into(),
            extra: BTreeMap::new(),
        });
        paused_job(&session, 2, req.clone());
        session.remove(1);
        std::thread::sleep(Duration::from_millis(300));
        assert!(req.partial_dir().exists(), "job 2 still needs them");
    }

    #[test]
    fn resume_without_tools_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::open(dir.path().join("jobs.json"), 1);
        paused_job(&session, 1, request("a"));
        assert!(session.toggle_pause(1).is_err());
        let job = &session.jobs()[0];
        assert!(job.snapshot.is_paused());
        assert_eq!(job.snapshot.status(), progress::PAUSED);
    }

    #[test]
    fn restart_refuses_a_name_another_active_job_uses() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::open(dir.path().join("jobs.json"), 1);
        session.set_tools(fake_tools(dir.path()));
        let stopped = Arc::new(JobSnapshot::default());
        stopped.mark_stopped();
        lock(&session.inner).jobs.push(Job {
            id: 1,
            request: request("clip"),
            snapshot: stopped,
            added: "now".into(),
            extra: BTreeMap::new(),
        });
        paused_job(&session, 2, request("clip"));
        let error = session.restart(1).unwrap_err();
        assert!(error.contains("already in the queue"));
    }

    #[test]
    fn unknown_fields_survive_hydrate_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        {
            let session = Session::open(path.clone(), 1);
            paused_job(&session, 3, request("a"));
            session.persist(true);
        }
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["jobs"][0]["priority"] = serde_json::json!(2);
        value["jobs"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"id": 40, "kind": "torrent"}));
        value["schema"] = serde_json::json!(5);
        std::fs::write(&path, value.to_string()).unwrap();

        let session = Session::open(path.clone(), 1);
        session.hydrate();
        session.set_tools(fake_tools(dir.path()));
        let id = session.queue(request("b")).unwrap();
        assert!(id > 40, "ids of unreadable records stay reserved");
        session.persist(true);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"priority\": 2"));
        assert!(text.contains("\"torrent\""));
        assert!(text.contains("\"schema\": 5"));
    }
}
