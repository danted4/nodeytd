//! `jobs.json`: the GUI's download list, kept between runs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::fsutil::atomic_write;
use crate::request::DownloadRequest;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedJob {
    pub id: u64,
    pub request: DownloadRequest,
    #[serde(default)]
    pub added: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub downloaded: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Fields a newer version wrote: kept on rewrite.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct JobFile {
    #[serde(default)]
    jobs: Vec<serde_json::Value>,
}

#[derive(Debug)]
pub enum LoadJobs {
    Empty,
    Loaded(Vec<PersistedJob>),
    /// The file did not parse; it was moved to `jobs.json.corrupt`.
    Corrupt(String),
}

/// Records that do not parse (a newer kind, a hand edit) are skipped, not
/// fatal: one bad entry never loses the rest of the list.
pub fn load_jobs_from(path: &Path) -> LoadJobs {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return LoadJobs::Empty,
    };
    match serde_json::from_str::<JobFile>(&text) {
        Ok(file) => {
            let jobs: Vec<PersistedJob> = file
                .jobs
                .into_iter()
                .filter_map(|value| serde_json::from_value(value).ok())
                .collect();
            if jobs.is_empty() {
                LoadJobs::Empty
            } else {
                LoadJobs::Loaded(jobs)
            }
        }
        Err(error) => {
            let _ = std::fs::rename(path, path.with_extension("json.corrupt"));
            LoadJobs::Corrupt(error.to_string())
        }
    }
}

pub fn save_jobs_to(path: &Path, jobs: &[PersistedJob]) -> Result<()> {
    let file = serde_json::json!({ "jobs": jobs });
    let bytes = serde_json::to_vec_pretty(&file).context("serialize jobs")?;
    atomic_write(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::DownloadKind;

    fn job(id: u64) -> PersistedJob {
        PersistedJob {
            id,
            request: DownloadRequest {
                url: "https://youtu.be/x".into(),
                title: "t".into(),
                kind: DownloadKind::Audio,
                video_format: None,
                audio_format: Some("140".into()),
                name: "t".into(),
                dir: PathBuf::from("/out"),
                trim_start: None,
                trim_end: None,
                expected_size: 0,
            },
            added: "2026-11-25".into(),
            status: "done".into(),
            downloaded: 5,
            total: 5,
            output: Some(PathBuf::from("/out/t.mp3")),
            error: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn round_trip_and_skip_bad_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        assert!(matches!(load_jobs_from(&path), LoadJobs::Empty));
        save_jobs_to(&path, &[job(1), job(2)]).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["jobs"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"id": 3, "kind": "from-the-future"}));
        std::fs::write(&path, value.to_string()).unwrap();
        match load_jobs_from(&path) {
            LoadJobs::Loaded(jobs) => {
                assert_eq!(jobs.len(), 2);
                assert_eq!(jobs[0], job(1));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn corrupt_file_is_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        std::fs::write(&path, "{broken").unwrap();
        assert!(matches!(load_jobs_from(&path), LoadJobs::Corrupt(_)));
        assert!(dir.path().join("jobs.json.corrupt").exists());
        assert!(!path.exists());
    }
}
