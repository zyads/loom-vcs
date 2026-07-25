// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! Heddle persistence — "boring on purpose":
//!
//! ```text
//! <data dir>/                (default ~/.heddle; HEDDLE_DATA overrides)
//!   repos.json               repo registry (RepoConfig list)
//!   <repo_id>/state.json     current threads/leases/stitches/weaves/fabric
//!   <repo_id>/log.jsonl      append-only event log (bounded by rotation)
//!   <repo_id>/objects/ab/<sha256>   content-addressed whole-file blobs
//! ```
//!
//! Every file is written 0o600. JSONL reads are corrupt-line tolerant: a
//! torn or garbage line is skipped, never fatal. The log is bounded by size
//! — past [`MAX_LOG_BYTES`] the oldest half is dropped (append-only in
//! spirit; rotation is the one concession that keeps "bounded" true).
//!
//! Blobs are WHOLE files keyed by sha256 of content (sha2 is already in the
//! tree — no new dependency). Identical content across stitches, threads and
//! repos-with-the-same-file stores once per repo. TODO(chunking): rolling-
//! hash chunk manifests would dedup large files across small edits; v1
//! trades those bytes for simplicity.

use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{HeddleState, RepoConfig, RepoState};

/// Log rotation threshold. At ~200 bytes/event this is thousands of events
/// per repo — plenty of history for a status view, bounded on disk.
pub const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

/// Files larger than this are skipped by stitch capture (recorded in the
/// outcome's `skipped` list) — Heddle snapshots source, not artifacts.
pub const MAX_SNAPSHOT_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// sha256 hex of raw bytes — the one content-address used everywhere.
pub fn content_hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn repos_path(base: &Path) -> PathBuf {
    base.join("repos.json")
}

pub fn state_path(base: &Path, repo_id: &str) -> PathBuf {
    base.join(repo_id).join("state.json")
}

pub fn log_path(base: &Path, repo_id: &str) -> PathBuf {
    base.join(repo_id).join("log.jsonl")
}

pub fn objects_dir(base: &Path, repo_id: &str) -> PathBuf {
    base.join(repo_id).join("objects")
}

/// Where isolated threads' git worktrees live: outside the repo's own tree,
/// so they never appear in anyone's scope, stitch or git status.
pub fn worktrees_dir(base: &Path, repo_id: &str) -> PathBuf {
    base.join(repo_id).join("worktrees")
}

/// Load the whole picture: registry + one state per registered repo. Any
/// unreadable or corrupt file degrades to its default — Heddle would rather
/// start an empty repo state than refuse to start.
pub fn load(base: &Path) -> HeddleState {
    let repos: Vec<RepoConfig> = std::fs::read_to_string(repos_path(base))
        .ok()
        .and_then(|b| serde_json::from_str(&b).ok())
        .unwrap_or_default();
    let mut repo_states = std::collections::HashMap::new();
    for r in &repos {
        let rs: RepoState = std::fs::read_to_string(state_path(base, &r.id))
            .ok()
            .and_then(|b| serde_json::from_str(&b).ok())
            .unwrap_or_default();
        repo_states.insert(r.id.clone(), rs);
    }
    HeddleState { repos, repo_states }
}

/// Persist registry + every repo state. Small states, whole-file writes.
pub fn persist(base: &Path, state: &HeddleState) {
    write_json_0600(&repos_path(base), &state.repos);
    for (id, rs) in &state.repo_states {
        write_json_0600(&state_path(base, id), rs);
    }
}

/// Append one event line to the repo's log, rotating past the size bound.
pub fn append_event(base: &Path, repo_id: &str, event: &Value) {
    let path = log_path(base, repo_id);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_LOG_BYTES {
            rotate(&path);
        }
    }
    let Ok(line) = serde_json::to_string(event) else {
        return;
    };
    use std::io::Write;
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path);
    if let Ok(mut f) = file {
        let _ = writeln!(f, "{line}");
    }
    restrict(&path);
}

/// Keep the newest half of the log's lines. Corrupt lines are dropped here
/// too — rotation is the natural cleanup point.
fn rotate(path: &Path) {
    let Ok(body) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = body
        .lines()
        .filter(|l| serde_json::from_str::<Value>(l).is_ok())
        .collect();
    let keep = &lines[lines.len() / 2..];
    let mut out = keep.join("\n");
    out.push('\n');
    let _ = std::fs::write(path, out);
    restrict(path);
}

/// The newest `max` events, oldest first. Corrupt lines are skipped.
pub fn read_events(base: &Path, repo_id: &str, max: usize) -> Vec<Value> {
    let Ok(body) = std::fs::read_to_string(log_path(base, repo_id)) else {
        return Vec::new();
    };
    let all: Vec<Value> = body
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let skip = all.len().saturating_sub(max);
    all.into_iter().skip(skip).collect()
}

/// Store bytes under their content hash; a hash that already exists is a
/// no-op (that IS the dedup). Fan-out by the first two hex chars keeps any
/// one directory small.
pub fn put_blob(objects: &Path, bytes: &[u8]) -> Result<String, String> {
    let hash = content_hash(bytes);
    let path = blob_path(objects, &hash);
    if path.exists() {
        return Ok(hash);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("blob dir: {e}"))?;
    }
    std::fs::write(&path, bytes).map_err(|e| format!("blob write: {e}"))?;
    restrict(&path);
    Ok(hash)
}

pub fn read_blob(objects: &Path, hash: &str) -> Result<Vec<u8>, String> {
    // Refuse anything that is not plain lowercase hex — a hash is data, and
    // data never gets to pick filesystem paths.
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("not a content hash: {hash:?}"));
    }
    std::fs::read(blob_path(objects, hash)).map_err(|e| format!("blob {hash} unreadable: {e}"))
}

/// Where a content hash lives on disk. Crate-visible so the git bridge can
/// hand blob files straight to `git hash-object --stdin-paths` without
/// copying the bytes through memory.
pub(crate) fn blob_path(objects: &Path, hash: &str) -> PathBuf {
    objects.join(&hash[..2]).join(hash)
}

fn write_json_0600(path: &Path, value: &impl serde::Serialize) {
    let Ok(body) = serde_json::to_string_pretty(value) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, body);
    restrict(path);
}

fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "heddle-store-{tag}-{}-{}",
            std::process::id(),
            super::super::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mk scratch");
        p
    }

    #[test]
    fn blobs_dedup_by_content_and_reject_non_hash_names() {
        let dir = scratch("blob");
        let h1 = put_blob(&dir, b"hello").expect("put");
        let h2 = put_blob(&dir, b"hello").expect("put again");
        assert_eq!(h1, h2);
        assert_eq!(read_blob(&dir, &h1).expect("read"), b"hello");
        // Path traversal via a fake "hash" is refused before touching fs.
        assert!(read_blob(&dir, "../../etc/passwd").is_err());
        assert!(read_blob(&dir, &"a".repeat(63)).is_err());
    }

    #[test]
    fn corrupt_log_lines_are_skipped_not_fatal() {
        let base = scratch("log");
        append_event(&base, "repo-x", &serde_json::json!({"kind": "one"}));
        // A torn write lands mid-log.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(log_path(&base, "repo-x"))
            .unwrap();
        f.write_all(b"{ torn").unwrap();
        f.write_all(b"\n").unwrap();
        drop(f);
        append_event(&base, "repo-x", &serde_json::json!({"kind": "two"}));
        let events = read_events(&base, "repo-x", 10);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["kind"], "one");
        assert_eq!(events[1]["kind"], "two");
        // Tail bound holds.
        assert_eq!(read_events(&base, "repo-x", 1).len(), 1);
        assert_eq!(read_events(&base, "repo-x", 1)[0]["kind"], "two");
    }
}
