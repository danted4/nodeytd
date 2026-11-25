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
    /// Top-level keys a newer version wrote.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

/// What was in `jobs.json`. Records this version cannot read (a newer kind,
/// a hand edit) and unknown top-level keys are kept in `foreign` / `extra`
/// and written back unchanged, so an older version never loses them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JobList {
    pub jobs: Vec<PersistedJob>,
    pub foreign: Vec<serde_json::Value>,
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl JobList {
    /// Highest id in use, readable records or not.
    pub fn max_id(&self) -> u64 {
        let known = self.jobs.iter().map(|job| job.id);
        let foreign = self
            .foreign
            .iter()
            .filter_map(|value| value.get("id").and_then(serde_json::Value::as_u64));
        known.chain(foreign).max().unwrap_or(0)
    }
}

#[derive(Debug)]
pub enum LoadJobs {
    Empty,
    Loaded(JobList),
    /// The file did not parse; it was moved to `jobs.json.corrupt`.
    Corrupt(String),
}

pub fn load_jobs_from(path: &Path) -> LoadJobs {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return LoadJobs::Empty,
    };
    match serde_json::from_str::<JobFile>(&text) {
        Ok(file) => {
            let mut list = JobList {
                extra: file.extra,
                ..JobList::default()
            };
            for value in file.jobs {
                match serde_json::from_value::<PersistedJob>(value.clone()) {
                    Ok(job) => list.jobs.push(job),
                    Err(_) => list.foreign.push(value),
                }
            }
            if list == JobList::default() {
                LoadJobs::Empty
            } else {
                LoadJobs::Loaded(list)
            }
        }
        Err(error) => {
            let _ = std::fs::rename(path, path.with_extension("json.corrupt"));
            LoadJobs::Corrupt(error.to_string())
        }
    }
}

pub fn save_jobs_to(path: &Path, list: &JobList) -> Result<()> {
    let mut jobs: Vec<serde_json::Value> = list
        .jobs
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<_, _>>()
        .context("serialize jobs")?;
    jobs.extend(list.foreign.iter().cloned());
    let file = JobFile {
        jobs,
        extra: list.extra.clone(),
    };
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
    fn unknown_records_and_keys_survive_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        assert!(matches!(load_jobs_from(&path), LoadJobs::Empty));
        let list = JobList {
            jobs: vec![job(1), job(2)],
            ..JobList::default()
        };
        save_jobs_to(&path, &list).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["jobs"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"id": 9, "kind": "from-the-future"}));
        value["jobs"][0]["priority"] = serde_json::json!(2);
        value["schema"] = serde_json::json!(5);
        std::fs::write(&path, value.to_string()).unwrap();

        let LoadJobs::Loaded(list) = load_jobs_from(&path) else {
            panic!("expected a list");
        };
        assert_eq!(list.jobs.len(), 2);
        assert_eq!(list.foreign.len(), 1);
        assert_eq!(
            list.max_id(),
            9,
            "unreadable records still reserve their id"
        );
        assert_eq!(list.jobs[0].extra["priority"], serde_json::json!(2));

        save_jobs_to(&path, &list).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("from-the-future"));
        assert!(text.contains("\"priority\": 2"));
        assert!(text.contains("\"schema\": 5"));
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
