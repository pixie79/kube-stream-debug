//! Writing per-cycle JSON snapshots to a directory, with a bounded file count.
//!
//! Each snapshot is a single JSON document (a `{ "as_of": ..., "topics": [...] }`
//! object) named `pth-<timestamp>.json`. When the number of snapshot files in
//! the directory exceeds the configured maximum, the oldest are removed so the
//! directory doesn't grow without bound.

use std::fs;
use std::path::{Path, PathBuf};

use crate::health::TopicHealth;

const PREFIX: &str = "pth-";
const SUFFIX: &str = ".json";

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("failed to create snapshot dir {path}: {source}")]
    CreateDir {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to write snapshot {path}: {source}")]
    Write {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to serialize snapshot: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Build the JSON body: one object carrying the run timestamp and the full
/// per-topic results (same shape as a JSONL line's contents, gathered into an
/// array).
fn snapshot_json(results: &[TopicHealth], run_at: &str) -> serde_json::Result<String> {
    let body = serde_json::json!({
        "as_of": run_at,
        "topics": results,
    });
    serde_json::to_string_pretty(&body)
}

/// Turn an RFC 3339 timestamp into a filesystem-safe filename component.
/// `2026-07-30T11:42:07Z` → `20260730T114207Z`.
fn filename_for(run_at: &str) -> String {
    let compact: String = run_at
        .chars()
        .filter(|c| !matches!(c, '-' | ':'))
        .collect();
    format!("{PREFIX}{compact}{SUFFIX}")
}

/// Write one snapshot into `dir`, creating the directory if needed, then prune
/// to at most `max_files` (0 = keep all).
pub fn write_snapshot(
    dir: &Path,
    results: &[TopicHealth],
    run_at: &str,
    max_files: usize,
) -> Result<PathBuf, SnapshotError> {
    fs::create_dir_all(dir).map_err(|source| SnapshotError::CreateDir {
        path: dir.display().to_string(),
        source,
    })?;

    let body = snapshot_json(results, run_at)?;
    let path = dir.join(filename_for(run_at));
    fs::write(&path, body).map_err(|source| SnapshotError::Write {
        path: path.display().to_string(),
        source,
    })?;

    prune(dir, max_files);
    Ok(path)
}

/// Remove the oldest snapshot files until at most `max_files` remain. Only
/// touches files matching our `pth-*.json` naming, so unrelated files in the
/// directory are never deleted. Pruning failures are ignored — a full disk or a
/// racing deletion shouldn't crash a watch loop.
fn prune(dir: &Path, max_files: usize) {
    if max_files == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    // The compact timestamp in the name sorts lexicographically in time order,
    // so a plain filename sort is chronological.
    let mut snapshots: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_snapshot(p))
        .collect();
    snapshots.sort();

    if snapshots.len() <= max_files {
        return;
    }
    let remove_count = snapshots.len() - max_files;
    for path in snapshots.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
}

fn is_snapshot(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with(PREFIX) && n.ends_with(SUFFIX))
        .unwrap_or(false)
}

/// Read the most recent snapshot file's JSON body from `dir`, if any. Filenames
/// sort chronologically (compact timestamp), so the lexicographically-largest
/// `pth-*.json` is the newest. Returns `None` if the dir is missing/empty or
/// the file can't be read.
pub fn read_latest_snapshot(dir: &Path) -> Option<String> {
    let mut snapshots: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_snapshot(p))
        .collect();
    snapshots.sort();
    let latest = snapshots.pop()?;
    fs::read_to_string(latest).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "pth-snap-test-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn filename_is_filesystem_safe() {
        assert_eq!(filename_for("2026-07-30T11:42:07Z"), "pth-20260730T114207Z.json");
    }

    #[test]
    fn writes_and_prunes_to_max() {
        let dir = temp_dir("prune");
        // Ten snapshots at distinct timestamps, keep 3.
        for i in 0..10 {
            let ts = format!("2026-07-30T11:{:02}:00Z", i);
            write_snapshot(&dir, &[], &ts, 3).expect("write should succeed");
        }
        let mut files: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(files.len(), 3, "should keep only 3 newest");
        // The three kept are the newest timestamps (07, 08, 09).
        assert_eq!(files[0], "pth-20260730T110700Z.json");
        assert_eq!(files[2], "pth-20260730T110900Z.json");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn max_zero_keeps_all() {
        let dir = temp_dir("keepall");
        for i in 0..5 {
            let ts = format!("2026-07-30T12:{:02}:00Z", i);
            write_snapshot(&dir, &[], &ts, 0).expect("write should succeed");
        }
        let count = fs::read_dir(&dir).unwrap().flatten().count();
        assert_eq!(count, 5);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prune_ignores_foreign_files() {
        let dir = temp_dir("foreign");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("keep-me.txt"), "not a snapshot").unwrap();
        for i in 0..5 {
            let ts = format!("2026-07-30T13:{:02}:00Z", i);
            write_snapshot(&dir, &[], &ts, 2).expect("write should succeed");
        }
        assert!(dir.join("keep-me.txt").exists(), "foreign file must survive");
        let snaps = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| is_snapshot(&e.path()))
            .count();
        assert_eq!(snaps, 2);
        fs::remove_dir_all(&dir).ok();
    }
}
