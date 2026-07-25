// Loom — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! Stitch capture, the weave gate's scratch worktree, and overlay apply.
//!
//! **Where the trust boundary runs through this file:**
//!
//! * [`capture_scope`] only READS the repo. It walks the tree, skips the
//!   junk directories every tool skips ([`EXCLUDED_DIRS`] — a minimal
//!   .gitignore stand-in, deliberately not a full gitignore parser; v1
//!   documents that limit rather than half-implementing the format), and
//!   snapshots matching files into the content-addressed store.
//! * [`run_gate`] runs the repo's verify command in a SCRATCH COPY of the
//!   repo, never in the real tree, via `sh -c` with a hard timeout
//!   ([`VERIFY_TIMEOUT_SECS`]) — a hung build cannot wedge the caller, and
//!   output is captured to temp files (not pipes), so a chatty build cannot
//!   deadlock on a full pipe buffer either.
//! * [`apply_overlay`] is the ONLY function that writes into the real repo,
//!   and the engine calls it exclusively from `land_weave` — which callers
//!   reach only after an explicit human yes. Manifest paths were
//!   produced by our own walker, but they are re-validated here anyway
//!   (relative, no `..`): stored data never gets to pick filesystem paths.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::store::{self, MAX_SNAPSHOT_FILE_BYTES};
use super::{now_ms, RepoConfig, VerifyOutcome, VerifyResult, TOMBSTONE};

/// Names never captured, never copied into scratch worktrees, whatever the
/// entry type — `.git` in particular is a *file* inside a git worktree.
/// Always excluded; a `.loomignore` can only ADD to this list, never
/// un-ignore it.
pub const EXCLUDED_DIRS: &[&str] = &[".git", "target", "node_modules"];

/// Per-repo ignore file at the repo (or worktree) root: one pattern per
/// line in Loom's own glob grammar (`**`, `*`, `?`; a fully-literal line
/// ignores that file or everything under that directory), `#` comments and
/// blank lines skipped. Applied AFTER the built-in excludes above — it
/// extends them and cannot re-include them.
pub const LOOMIGNORE_FILE: &str = ".loomignore";

/// Load the root's `.loomignore` patterns (empty when absent/unreadable).
/// Invalid patterns (absolute, `..`, backslashes) are dropped silently —
/// an ignore file must never become a path-escape vector.
pub fn load_loomignore(root: &Path) -> Vec<String> {
    let Ok(body) = std::fs::read_to_string(root.join(LOOMIGNORE_FILE)) else {
        return Vec::new();
    };
    body.lines()
        .map(|l| l.trim().trim_start_matches("./").to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter(|l| !l.starts_with('/') && !l.contains('\\'))
        .filter(|l| !l.split('/').any(|seg| seg == ".." || seg.is_empty()))
        .take(200)
        .collect()
}

/// Does any ignore pattern match this repo-relative path? Works for files
/// and for directories (a literal pattern matches the directory itself and
/// everything under it — same rule as lease scopes).
fn ignored(patterns: &[String], rel: &str) -> bool {
    patterns
        .iter()
        .any(|p| super::lease::glob_match(p, rel) || p == rel)
}

/// Hard ceiling on one verify run.
pub const VERIFY_TIMEOUT_SECS: u64 = 300;

/// Combined stdout+stderr tail kept per verify.
pub const MAX_LOG_TAIL_CHARS: usize = 4000;

/// Sanity cap on files per stitch — a scope matching more than this is a
/// scope that should be narrower.
pub const MAX_FILES_PER_STITCH: usize = 5000;

/// What a capture found: the manifest plus anything skipped (too big,
/// unreadable) so the caller can be honest about coverage.
pub struct Captured {
    pub manifest: BTreeMap<String, String>,
    pub skipped: Vec<String>,
}

/// Snapshot every file under `root` matching any scope pattern into the
/// blob store. Read-only with respect to the repo.
pub fn capture_scope(
    root: &Path,
    scope: &[String],
    objects: &Path,
) -> Result<Captured, String> {
    let ignore = load_loomignore(root);
    let max_bytes = max_snapshot_bytes();
    let mut manifest = BTreeMap::new();
    let mut skipped = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                skipped.push(format!("{}: {e}", dir.display()));
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue; // never follow links out of the repo
            }
            // Built-in excludes apply to ANY entry type: `.git` is a file
            // inside a git worktree.
            if EXCLUDED_DIRS.contains(&name.as_str()) || name == LOOMIGNORE_FILE {
                continue;
            }
            let Some(rel) = relative_slash(root, &path) else {
                continue;
            };
            if ignored(&ignore, &rel) {
                continue;
            }
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !scope.iter().any(|p| super::lease::glob_match(p, &rel)) {
                continue;
            }
            if manifest.len() >= MAX_FILES_PER_STITCH {
                return Err(format!(
                    "scope matches more than {MAX_FILES_PER_STITCH} files — narrow the lease"
                ));
            }
            match std::fs::metadata(&path) {
                Ok(m) if m.len() > max_bytes => {
                    skipped.push(format!(
                        "{rel} (too large: {} bytes > the {} MiB snapshot cap — \
                         raise LOOM_MAX_FILE_MB or .loomignore it)",
                        m.len(),
                        max_bytes / (1024 * 1024),
                    ));
                    continue;
                }
                Err(e) => {
                    skipped.push(format!("{rel}: {e}"));
                    continue;
                }
                _ => {}
            }
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let hash = store::put_blob(objects, &bytes)?;
                    manifest.insert(rel, hash);
                }
                Err(e) => skipped.push(format!("{rel}: {e}")),
            }
        }
    }
    Ok(Captured { manifest, skipped })
}

/// The weave gate: copy the repo (minus excludes) to a scratch dir, overlay
/// the thread's manifest, run the verify command there. Any failure to even
/// set the stage is an honest Red with the reason in the log tail — a gate
/// that cannot run its check has NOT verified anything.
pub fn run_gate(
    repo: &RepoConfig,
    manifest: &BTreeMap<String, String>,
    objects: &Path,
) -> VerifyOutcome {
    // pid + ms + an atomic counter: two gates racing in the same process
    // and millisecond (parallel tests, parallel agents) must never share a
    // scratch dir — one's cleanup would eat the other's verify log.
    static GATE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let scratch = std::env::temp_dir().join(format!(
        "loom-gate-{}-{}-{}",
        std::process::id(),
        now_ms(),
        GATE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let outcome = (|| -> Result<VerifyOutcome, String> {
        copy_tree(Path::new(&repo.path), &scratch)?;
        apply_overlay(&scratch, manifest, objects)?;
        Ok(run_verify(&repo.verify_cmd, &scratch, VERIFY_TIMEOUT_SECS))
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    match outcome {
        Ok(v) => v,
        Err(e) => VerifyOutcome {
            cmd: repo.verify_cmd.clone(),
            result: VerifyResult::Red,
            log_tail: format!("weave gate could not stage the verify: {e}"),
        },
    }
}

/// Apply a manifest onto a directory (used for BOTH the scratch overlay and
/// — via `land_weave` only — the real tree). A [`TOMBSTONE`] entry deletes
/// the file (deleting what is already absent is a quiet no-op). Returns
/// files written or deleted.
pub fn apply_overlay(
    root: &Path,
    manifest: &BTreeMap<String, String>,
    objects: &Path,
) -> Result<usize, String> {
    let mut applied = 0;
    for (rel, hash) in manifest {
        let dest = safe_join(root, rel)?;
        if hash == TOMBSTONE {
            if dest.exists() {
                std::fs::remove_file(&dest).map_err(|e| format!("delete {rel}: {e}"))?;
                applied += 1;
            }
            continue;
        }
        let bytes = store::read_blob(objects, hash)?;
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("mkdir for {rel}: {e}"))?;
        }
        std::fs::write(&dest, bytes).map_err(|e| format!("write {rel}: {e}"))?;
        applied += 1;
    }
    Ok(applied)
}

/// Make `dst_root`'s scope files identical to `src_root`'s (copying changed
/// or missing files, deleting scope files `src_root` does not have), and
/// return `src_root`'s captured scope — used right after `git worktree add`
/// so a fresh worktree starts from the FABRIC's live state, not from git
/// HEAD (landed weaves live in the working tree until the git bridge or a
/// human commits them). The returned capture is exactly the thread's base.
pub fn align_tree(
    src_root: &Path,
    dst_root: &Path,
    scope: &[String],
    objects: &Path,
) -> Result<Captured, String> {
    let src = capture_scope(src_root, scope, objects)?;
    let dst = capture_scope(dst_root, scope, objects)?;
    for (rel, h) in &src.manifest {
        if dst.manifest.get(rel) != Some(h) {
            let bytes = store::read_blob(objects, h)?;
            let to = safe_join(dst_root, rel)?;
            if let Some(dir) = to.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("align mkdir {rel}: {e}"))?;
            }
            std::fs::write(&to, bytes).map_err(|e| format!("align write {rel}: {e}"))?;
        }
    }
    for rel in dst.manifest.keys() {
        if !src.manifest.contains_key(rel) {
            if let Ok(gone) = safe_join(dst_root, rel) {
                let _ = std::fs::remove_file(gone);
            }
        }
    }
    Ok(src)
}

/// The content hash of one repo-relative file as it stands on disk, `None`
/// when it does not exist (or the path is unsafe).
pub fn hash_on_disk(root: &Path, rel: &str) -> Option<String> {
    let p = safe_join(root, rel).ok()?;
    std::fs::read(p).ok().map(|b| store::content_hash(&b))
}

/// The file-level three-way merge decision for landing an isolated thread:
/// given the thread's head manifest and its base (the fabric snapshot it
/// branched from), against the LIVE tree at `root`:
///
/// * files the thread did not change vs base → not applied (the fabric's
///   version, whatever it now is, stays);
/// * files only the thread changed → applied (edits and tombstones alike);
/// * files changed in BOTH (edit-vs-edit, edit-vs-delete, delete-vs-edit)
///   where the two sides disagree → **conflicts**; the caller must refuse.
///
/// `base: None` (an in-place thread) returns the whole manifest unchanged —
/// v0.1 semantics.
pub fn merge_plan(
    root: &Path,
    head: &BTreeMap<String, String>,
    base: Option<&BTreeMap<String, String>>,
) -> (BTreeMap<String, String>, Vec<String>) {
    let Some(base) = base else {
        return (head.clone(), Vec::new());
    };
    let mut apply = BTreeMap::new();
    let mut conflicts = Vec::new();
    for (rel, h) in head {
        let base_h = base.get(rel);
        if base_h == Some(h) {
            continue; // the thread didn't change it — never overwrite fabric
        }
        if base_h.is_none() && h == TOMBSTONE {
            continue; // created then deleted within the thread — nothing to do
        }
        // What the fabric has right now (absence reads as a tombstone).
        let cur = hash_on_disk(root, rel).unwrap_or_else(|| TOMBSTONE.to_string());
        if cur == *h {
            continue; // fabric already agrees with the thread
        }
        let fabric_changed = match base_h {
            Some(b) => cur != *b,
            None => cur != TOMBSTONE, // not in base; fabric changed iff it exists now
        };
        if fabric_changed {
            conflicts.push(rel.clone());
        } else {
            apply.insert(rel.clone(), h.clone());
        }
    }
    (apply, conflicts)
}

/// How the head manifest differs from what is on disk right now. v1's
/// fabric materializes into the real working tree, so "vs fabric" IS "vs
/// worktree". Deletions are not tracked (a stitch snapshots what exists;
/// files the thread deleted simply stop appearing) — documented limit.
pub fn diff_vs_worktree(root: &Path, manifest: &BTreeMap<String, String>) -> serde_json::Value {
    let mut changed = Vec::new();
    let mut added = Vec::new();
    let mut unchanged = 0usize;
    for (rel, hash) in manifest {
        let Ok(dest) = safe_join(root, rel) else { continue };
        match std::fs::read(&dest) {
            Ok(_) if hash == TOMBSTONE => changed.push(rel.clone()), // thread deleted it
            Ok(bytes) if store::content_hash(&bytes) == *hash => unchanged += 1,
            Ok(_) => changed.push(rel.clone()),
            Err(_) if hash == TOMBSTONE => unchanged += 1, // deleted both sides
            Err(_) => added.push(rel.clone()),
        }
    }
    serde_json::json!({
        "changed": changed,
        "added": added,
        "unchanged": unchanged,
    })
}

/// Recursive copy skipping [`EXCLUDED_DIRS`] (any entry type), symlinks and
/// `.loomignore` matches.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    let ignore = load_loomignore(src);
    copy_tree_rec(src, dst, src, &ignore)
}

fn copy_tree_rec(src: &Path, dst: &Path, root: &Path, ignore: &[String]) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    let entries = std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        let from = entry.path();
        let to = dst.join(&name);
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || EXCLUDED_DIRS.contains(&name_str.as_str()) {
            continue;
        }
        if let Some(rel) = relative_slash(root, &from) {
            if ignored(ignore, &rel) {
                continue;
            }
        }
        if ft.is_dir() {
            copy_tree_rec(&from, &to, root, ignore)?;
        } else {
            std::fs::copy(&from, &to)
                .map_err(|e| format!("copy {}: {e}", from.display()))?;
        }
    }
    Ok(())
}

/// The per-file snapshot cap: [`MAX_SNAPSHOT_FILE_BYTES`] (8 MiB) unless
/// the `LOOM_MAX_FILE_MB` env var picks another ceiling (clamped 1–1024).
/// Larger files are SKIPPED and named in the stitch outcome's `skipped`
/// list — loom snapshots source, not artifacts; put artifacts in
/// `.loomignore` instead of raising the cap.
pub fn max_snapshot_bytes() -> u64 {
    std::env::var("LOOM_MAX_FILE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|mb| mb.clamp(1, 1024) * 1024 * 1024)
        .unwrap_or(MAX_SNAPSHOT_FILE_BYTES)
}

/// Run a shell command with a hard timeout, output to temp files (never
/// pipes — a full pipe buffer would deadlock a chatty build). Returns the
/// outcome with a bounded log tail. Also used by the git bridge.
pub fn run_verify(cmd: &str, cwd: &Path, timeout_secs: u64) -> VerifyOutcome {
    // The log lives OUTSIDE `cwd`: the git bridge runs `git add -A` through
    // here, and a log file inside the tree would be staged into the commit.
    static LOG_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let log_path = std::env::temp_dir().join(format!(
        "loom-verify-{}-{}-{}.log",
        std::process::id(),
        now_ms(),
        LOG_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let red = |tail: String| VerifyOutcome {
        cmd: cmd.to_string(),
        result: VerifyResult::Red,
        log_tail: tail,
    };
    let file = match std::fs::File::create(&log_path) {
        Ok(f) => f,
        Err(e) => return red(format!("cannot open verify log: {e}")),
    };
    let file_err = match file.try_clone() {
        Ok(f) => f,
        Err(e) => return red(format!("cannot clone verify log handle: {e}")),
    };
    let child = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(file)
        .stderr(file_err)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return red(format!("cannot spawn verify command: {e}")),
    };
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                return red(format!("verify wait failed: {e}"));
            }
        }
    };
    let mut tail = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);
    if tail.chars().count() > MAX_LOG_TAIL_CHARS {
        let cut: String = tail
            .chars()
            .rev()
            .take(MAX_LOG_TAIL_CHARS)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        tail = format!("…{cut}");
    }
    match status {
        Some(s) if s.success() => VerifyOutcome {
            cmd: cmd.to_string(),
            result: VerifyResult::Green,
            log_tail: tail,
        },
        Some(s) => red(format!("exit {s}\n{tail}")),
        None => red(format!(
            "timed out after {timeout_secs}s (killed)\n{tail}"
        )),
    }
}

/// Join a stored relative path under `root`, refusing absolutes, `..` and
/// backslashes. Stored data never picks paths.
pub(crate) fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.starts_with('/') || rel.contains('\\') {
        return Err(format!("unsafe manifest path {rel:?}"));
    }
    if rel.split('/').any(|seg| seg == ".." || seg.is_empty() || seg == ".") {
        return Err(format!("unsafe manifest path {rel:?}"));
    }
    Ok(root.join(rel))
}

fn relative_slash(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let s = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "loom-weave-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mk scratch");
        p
    }

    #[test]
    fn capture_skips_junk_dirs_and_respects_scope() {
        let repo = scratch("cap");
        for (rel, body) in [
            ("src/main.rs", "fn main() {}"),
            ("src/lib.rs", "pub fn x() {}"),
            ("target/debug/junk.rs", "artifact"),
            (".git/HEAD", "ref"),
            ("node_modules/x/y.js", "js"),
            ("README.md", "hi"),
        ] {
            let p = repo.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let objects = scratch("cap-obj");
        let c = capture_scope(&repo, &["**/*.rs".to_string()], &objects).expect("capture");
        assert_eq!(
            c.manifest.keys().cloned().collect::<Vec<_>>(),
            vec!["src/lib.rs".to_string(), "src/main.rs".to_string()],
            "excluded dirs never contribute, scope filters the rest"
        );
        assert!(c.skipped.is_empty());
    }

    #[test]
    fn loomignore_extends_the_builtin_excludes_for_capture_and_copy() {
        let repo = scratch("ign");
        for (rel, body) in [
            ("src/keep.rs", "kept"),
            ("logs/noise.rs", "ignored dir"),
            ("src/scratch.tmp.rs", "ignored glob"),
            ("target/debug/junk.rs", "builtin"),
        ] {
            let p = repo.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        std::fs::write(
            repo.join(".loomignore"),
            "# artifacts\nlogs\n**/*.tmp.rs\n../evil\n",
        )
        .unwrap();
        let objects = scratch("ign-obj");
        let c = capture_scope(&repo, &["**".to_string()], &objects).expect("capture");
        assert_eq!(
            c.manifest.keys().cloned().collect::<Vec<_>>(),
            vec!["src/keep.rs".to_string()],
            ".loomignore itself, its patterns, and built-ins all excluded"
        );
        // The gate's scratch copy honors the same rules.
        let dst = scratch("ign-copy");
        copy_tree(&repo, &dst).expect("copy");
        assert!(dst.join("src/keep.rs").exists());
        assert!(!dst.join("logs").exists());
        assert!(!dst.join("src/scratch.tmp.rs").exists());
        assert!(!dst.join("target").exists());
    }

    #[test]
    fn merge_plan_applies_thread_only_changes_and_refuses_both_sided_ones() {
        let root = scratch("mp");
        let objects = scratch("mp-obj");
        let base_hash = store::put_blob(&objects, b"base").unwrap();
        let thread_hash = store::put_blob(&objects, b"thread").unwrap();
        std::fs::write(root.join("fabric-moved.txt"), "fabric").unwrap();
        std::fs::write(root.join("untouched.txt"), "base").unwrap();
        std::fs::write(root.join("agrees.txt"), "thread").unwrap();
        std::fs::write(root.join("del-thread.txt"), "base").unwrap();
        let mut base = BTreeMap::new();
        for f in [
            "fabric-moved.txt",
            "untouched.txt",
            "agrees.txt",
            "gone.txt",
            "del-thread.txt",
        ] {
            base.insert(f.to_string(), base_hash.clone());
        }
        let mut head = BTreeMap::new();
        head.insert("fabric-moved.txt".into(), thread_hash.clone()); // both changed → conflict
        head.insert("untouched.txt".into(), base_hash.clone()); // thread didn't change
        head.insert("agrees.txt".into(), thread_hash.clone()); // fabric already there
        head.insert("gone.txt".into(), TOMBSTONE.to_string()); // deleted both sides → no-op
        head.insert("del-thread.txt".into(), TOMBSTONE.to_string()); // thread-only delete
        head.insert("new.txt".into(), thread_hash.clone()); // thread-only add
        let (apply, conflicts) = merge_plan(&root, &head, Some(&base));
        assert_eq!(conflicts, vec!["fabric-moved.txt".to_string()]);
        assert_eq!(
            apply.keys().cloned().collect::<Vec<_>>(),
            vec!["del-thread.txt".to_string(), "new.txt".to_string()]
        );
        assert_eq!(apply["del-thread.txt"], TOMBSTONE);
        // In-place (no base): the whole manifest applies, v0.1 semantics.
        let (apply, conflicts) = merge_plan(&root, &head, None);
        assert_eq!(apply.len(), head.len());
        assert!(conflicts.is_empty());
    }

    #[test]
    fn the_gate_is_green_on_true_and_red_on_false_with_a_tail() {
        let repo_dir = scratch("gate");
        std::fs::write(repo_dir.join("f.txt"), "x").unwrap();
        let objects = scratch("gate-obj");
        let mk = |cmd: &str| RepoConfig {
            id: "repo-t".into(),
            path: repo_dir.to_string_lossy().to_string(),
            verify_cmd: cmd.into(),
            git_bridge: false,
            bridge_mode: Default::default(),
            registered_ms: 0,
            sync_remote: None,
            auto_sync: false,
        };
        let manifest = BTreeMap::new();
        let green = run_gate(&mk("true"), &manifest, &objects);
        assert_eq!(green.result, VerifyResult::Green);
        let red = run_gate(&mk("echo boom >&2; false"), &manifest, &objects);
        assert_eq!(red.result, VerifyResult::Red);
        assert!(red.log_tail.contains("boom"), "stderr captured: {}", red.log_tail);
    }

    #[test]
    fn a_hung_verify_is_killed_and_reported_red() {
        let repo_dir = scratch("hang");
        std::fs::write(repo_dir.join("f.txt"), "x").unwrap();
        let v = run_verify("sleep 30", &repo_dir, 1);
        assert_eq!(v.result, VerifyResult::Red);
        assert!(v.log_tail.contains("timed out"), "{}", v.log_tail);
    }

    #[test]
    fn overlay_refuses_unsafe_paths_and_applies_safe_ones() {
        let root = scratch("ovl");
        let objects = scratch("ovl-obj");
        let hash = store::put_blob(&objects, b"content").unwrap();
        let mut bad = BTreeMap::new();
        bad.insert("../escape.txt".to_string(), hash.clone());
        assert!(apply_overlay(&root, &bad, &objects).is_err());
        let mut abs = BTreeMap::new();
        abs.insert("/etc/evil".to_string(), hash.clone());
        assert!(apply_overlay(&root, &abs, &objects).is_err());
        let mut good = BTreeMap::new();
        good.insert("deep/dir/file.txt".to_string(), hash);
        assert_eq!(apply_overlay(&root, &good, &objects).unwrap(), 1);
        assert_eq!(
            std::fs::read_to_string(root.join("deep/dir/file.txt")).unwrap(),
            "content"
        );
    }
}
