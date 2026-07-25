// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! Solo-mode state pointer — "which lease am I holding right now?"
//!
//! The standalone CLI is one human (or one agent session) per terminal, and
//! typing lease ids at every verb would be ceremony. `solo.json` in the heddle
//! data dir remembers, per repo, the caller's current lease + thread — set
//! by `heddle lease` and `heddle adopt`, read by `stitch`/`propose`/`withdraw`/
//! `status`, cleared when the thread weaves.
//!
//! This is a *convenience pointer*, not state the engine trusts: every verb
//! re-validates the ids against the engine, and a stale pointer (thread
//! woven, lease gone) is reported honestly and dropped rather than silently
//! recreated. Multiple terminals sharing one data dir share one pointer per
//! repo — solo mode means one work-line per repo at a time; use explicit ids
//! (or one data dir per agent via `HEDDLE_DATA`) for anything fancier.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The pointer for one repo: the lease the solo caller currently holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SoloPointer {
    pub lease_id: String,
    pub thread_id: String,
}

fn path(base: &Path) -> PathBuf {
    base.join("solo.json")
}

fn load(base: &Path) -> BTreeMap<String, SoloPointer> {
    std::fs::read_to_string(path(base))
        .ok()
        .and_then(|b| serde_json::from_str(&b).ok())
        .unwrap_or_default()
}

fn save(base: &Path, map: &BTreeMap<String, SoloPointer>) {
    let Ok(body) = serde_json::to_string_pretty(map) else {
        return;
    };
    let _ = std::fs::create_dir_all(base);
    let _ = std::fs::write(path(base), body);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path(base), std::fs::Permissions::from_mode(0o600));
    }
}

/// Remember the current lease/thread for `repo_id`.
pub fn set(base: &Path, repo_id: &str, lease_id: &str, thread_id: &str) {
    let mut map = load(base);
    map.insert(
        repo_id.to_string(),
        SoloPointer {
            lease_id: lease_id.to_string(),
            thread_id: thread_id.to_string(),
        },
    );
    save(base, &map);
}

/// The current pointer for `repo_id`, if one was set.
pub fn get(base: &Path, repo_id: &str) -> Option<SoloPointer> {
    load(base).remove(repo_id)
}

/// Drop the pointer for `repo_id` (thread wove, or the pointer went stale).
pub fn clear(base: &Path, repo_id: &str) {
    let mut map = load(base);
    if map.remove(repo_id).is_some() {
        save(base, &map);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "heddle-solo-{tag}-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mk scratch");
        p
    }

    #[test]
    fn set_get_clear_roundtrip_per_repo() {
        let base = scratch("roundtrip");
        assert!(get(&base, "repo-a").is_none());
        set(&base, "repo-a", "lease-1", "thread-1");
        set(&base, "repo-b", "lease-2", "thread-2");
        assert_eq!(
            get(&base, "repo-a"),
            Some(SoloPointer {
                lease_id: "lease-1".into(),
                thread_id: "thread-1".into()
            })
        );
        // Re-leasing overwrites; other repos untouched.
        set(&base, "repo-a", "lease-3", "thread-3");
        assert_eq!(get(&base, "repo-a").unwrap().lease_id, "lease-3");
        assert_eq!(get(&base, "repo-b").unwrap().lease_id, "lease-2");
        clear(&base, "repo-a");
        assert!(get(&base, "repo-a").is_none());
        assert!(get(&base, "repo-b").is_some());
    }

    #[test]
    fn a_corrupt_pointer_file_degrades_to_empty_not_a_crash() {
        let base = scratch("corrupt");
        std::fs::write(path(&base), b"{ not json").unwrap();
        assert!(get(&base, "repo-a").is_none());
        set(&base, "repo-a", "l", "t");
        assert!(get(&base, "repo-a").is_some());
    }
}
