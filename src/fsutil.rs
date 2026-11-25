//! Small filesystem helpers: atomic writes and filename cleanup.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};

static FILES: Mutex<()> = Mutex::new(());

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Write `<path>.<pid>.tmp`, sync it, then rename it over `path`, so a crash
/// never leaves a torn settings or jobs file. `std::fs::rename` replaces an
/// existing file on every platform we ship.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let _guard = FILES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = tmp_path(path);
    {
        let mut file = std::fs::File::create(&tmp)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("failed to write {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))
}

/// The default-name rule from the Node CLI: every character that is not an
/// ASCII letter or digit becomes `-`.
pub fn title_slug(title: &str) -> String {
    let slug: String = title
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    if slug.is_empty() {
        "video".to_string()
    } else {
        slug
    }
}

/// Make a user-typed file name safe on every OS: no path separators, no
/// reserved characters, no trailing dots or spaces (Windows), never empty.
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            ch if ch.is_control() => '-',
            ch => ch,
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']);
    let trimmed = trimmed.trim_start_matches('.');
    if trimmed.is_empty() {
        "video".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_node_cli_rule() {
        assert_eq!(title_slug("Me at the zoo"), "Me-at-the-zoo");
        assert_eq!(title_slug("A/B: c!"), "A-B--c-");
        assert_eq!(title_slug(""), "video");
    }

    #[test]
    fn sanitize_strips_separators_and_dots() {
        assert_eq!(sanitize_file_name("../evil"), "-evil");
        assert_eq!(sanitize_file_name("a\\b/c"), "a-b-c");
        assert_eq!(sanitize_file_name("name. "), "name");
        assert_eq!(sanitize_file_name("   "), "video");
        assert_eq!(sanitize_file_name("..."), "video");
    }

    #[test]
    fn atomic_write_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        atomic_write(&path, b"one").unwrap();
        atomic_write(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let leftovers = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(leftovers, 1, "no temp files left behind");
    }
}
