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
use super::{now_ms, RepoConfig, VerifyOutcome, VerifyResult};

/// Directories never captured, never copied into scratch worktrees. A
/// minimal, hard-coded stand-in for .gitignore; real gitignore parsing is
/// future work and the limitation is on purpose (boring beats subtly wrong).
pub const EXCLUDED_DIRS: &[&str] = &[".git", "target", "node_modules"];

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
            if ft.is_dir() {
                if !EXCLUDED_DIRS.contains(&name.as_str()) {
                    stack.push(path);
                }
                continue;
            }
            let Some(rel) = relative_slash(root, &path) else {
                continue;
            };
            if !scope.iter().any(|p| super::lease::glob_match(p, &rel)) {
                continue;
            }
            if manifest.len() >= MAX_FILES_PER_STITCH {
                return Err(format!(
                    "scope matches more than {MAX_FILES_PER_STITCH} files — narrow the lease"
                ));
            }
            match std::fs::metadata(&path) {
                Ok(m) if m.len() > MAX_SNAPSHOT_FILE_BYTES => {
                    skipped.push(format!("{rel} (too large: {} bytes)", m.len()));
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
/// — via `land_weave` only — the real tree). Returns files written.
pub fn apply_overlay(
    root: &Path,
    manifest: &BTreeMap<String, String>,
    objects: &Path,
) -> Result<usize, String> {
    let mut applied = 0;
    for (rel, hash) in manifest {
        let dest = safe_join(root, rel)?;
        let bytes = store::read_blob(objects, hash)?;
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("mkdir for {rel}: {e}"))?;
        }
        std::fs::write(&dest, bytes).map_err(|e| format!("write {rel}: {e}"))?;
        applied += 1;
    }
    Ok(applied)
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
            Ok(bytes) if store::content_hash(&bytes) == *hash => unchanged += 1,
            Ok(_) => changed.push(rel.clone()),
            Err(_) => added.push(rel.clone()),
        }
    }
    serde_json::json!({
        "changed": changed,
        "added": added,
        "unchanged": unchanged,
    })
}

/// Recursive copy skipping [`EXCLUDED_DIRS`] and symlinks.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    let entries = std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        let from = entry.path();
        let to = dst.join(&name);
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if !EXCLUDED_DIRS.contains(&name_str.as_str()) {
                copy_tree(&from, &to)?;
            }
        } else {
            std::fs::copy(&from, &to)
                .map_err(|e| format!("copy {}: {e}", from.display()))?;
        }
    }
    Ok(())
}

/// Run a shell command with a hard timeout, output to temp files (never
/// pipes — a full pipe buffer would deadlock a chatty build). Returns the
/// outcome with a bounded log tail. Also used by the git bridge.
pub fn run_verify(cmd: &str, cwd: &Path, timeout_secs: u64) -> VerifyOutcome {
    let log_path = cwd.join(".loom-verify.log");
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
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, String> {
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
    fn the_gate_is_green_on_true_and_red_on_false_with_a_tail() {
        let repo_dir = scratch("gate");
        std::fs::write(repo_dir.join("f.txt"), "x").unwrap();
        let objects = scratch("gate-obj");
        let mk = |cmd: &str| RepoConfig {
            id: "repo-t".into(),
            path: repo_dir.to_string_lossy().to_string(),
            verify_cmd: cmd.into(),
            git_bridge: false,
            registered_ms: 0,
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
