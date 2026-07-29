// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! Per-thread git worktree isolation — the mechanism that makes two threads
//! on one repo physically unable to clobber each other's edits.
//!
//! An **isolated** thread gets its own `git worktree` under the heddle data
//! dir (`<data>/<repo_id>/worktrees/<thread-id>` — outside the repo's own
//! tree, so it never shows up in anyone's scope or git status). The holder
//! edits THERE; stitches capture from there; the real repo tree changes only
//! when a weave lands, through the merge rules in `land_weave`.
//!
//! This module is deliberately thin: it shells out to the repo's own `git`
//! for `worktree add`/`worktree remove` and nothing else. It never commits,
//! never pushes, never touches branches — worktrees are added `--detach` at
//! HEAD so no branch is ever "already checked out" or moved.
//!
//! When the repo is not a git repo (or `git worktree add` fails and the
//! caller asked for Auto), the engine falls back to **in-place** mode — the
//! v0.1 behavior where the thread edits the repo tree directly — and says so
//! on the thread's note. Isolation degrades honestly, never silently.

use std::path::Path;
use std::process::Command;

/// Is `repo` a git repo we can add worktrees to? (`.git` may be a directory
/// — a normal repo — or a file — itself a worktree; both count.)
pub fn is_git_repo(repo: &Path) -> bool {
    repo.join(".git").exists()
}

/// Run one git subcommand in `repo`, capturing output. Errors carry git's
/// stderr so the caller can be honest about *why* isolation failed.
fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&"?"),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Create a detached worktree of `repo`'s HEAD at `dest`. Fails honestly on
/// a repo with no commits yet ("HEAD" doesn't resolve) — the engine treats
/// that as "isolation unavailable" and falls back or refuses per mode.
pub fn add(repo: &Path, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("worktree parent dir: {e}"))?;
    }
    let dest_str = dest.to_string_lossy().to_string();
    git(repo, &["worktree", "add", "--detach", &dest_str, "HEAD"])?;
    Ok(())
}

/// The repo's **root commit** — the sha of its first parentless commit. This
/// is the one identity that survives `mv`: paths change, remotes change, the
/// working tree changes, but a repo's root commit is fixed at `git init`.
/// Heddle keys a moved repo back to its state with it (see `repair_repos`).
///
/// `None` when the repo has no commits yet, isn't a git repo, or has several
/// root commits (grafted/merged histories) — an ambiguous identity is no
/// identity, and Heddle would rather fall back to a name match it tells you
/// about than silently bind state to the wrong repo.
pub fn root_commit(repo: &Path) -> Option<String> {
    let out = git(repo, &["rev-list", "--max-parents=0", "HEAD"]).ok()?;
    let mut roots = out.split_whitespace();
    let first = roots.next()?.to_string();
    if roots.next().is_some() {
        return None;
    }
    Some(first)
}

/// Re-point this repo's worktree metadata after the repo (or the worktrees)
/// moved on disk — the same job `git worktree repair` does, which is exactly
/// why it IS `git worktree repair`.
///
/// `moved` MUST list the worktrees' NEW paths. Bare `git worktree repair`
/// only fixes worktrees it can still find at their recorded locations, so
/// when the worktrees themselves moved it repairs nothing — and a `prune`
/// after that deletes their administrative entries, orphaning intact
/// checkouts. Naming the new paths is what lets git re-point both sides.
///
/// Best-effort: a repo with no worktrees succeeds trivially, and a failure
/// here never blocks a rebind.
pub fn repair(repo: &Path, moved: &[String]) -> Result<(), String> {
    let mut args: Vec<&str> = vec!["worktree", "repair"];
    args.extend(moved.iter().map(String::as_str));
    git(repo, &args).map(|_| ())
}

/// Every worktree directory directly under `dir` — the new locations handed
/// to [`repair`] after a state dir moved.
pub fn dirs_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .map(|p| p.to_string_lossy().to_string())
        .collect()
}

/// Drop worktree registrations whose directory is gone. `git worktree prune`
/// is safe by construction — it only forgets entries git can no longer find.
pub fn prune(repo: &Path) -> Result<(), String> {
    git(repo, &["worktree", "prune"]).map(|_| ())
}

/// Remove a worktree at `dest`. `--force` because the worktree is EXPECTED
/// to be dirty relative to its checkout (the thread's edits live there) —
/// callers run their own "is everything captured in a stitch?" check first;
/// git's dirtiness check would refuse every legitimately-used worktree.
/// Falls back to a plain directory delete + `git worktree prune` when git
/// itself refuses (e.g. the repo's worktree metadata is already gone).
pub fn remove(repo: &Path, dest: &Path) -> Result<(), String> {
    let dest_str = dest.to_string_lossy().to_string();
    if git(repo, &["worktree", "remove", "--force", &dest_str]).is_ok() {
        return Ok(());
    }
    std::fs::remove_dir_all(dest).map_err(|e| format!("remove worktree dir: {e}"))?;
    let _ = git(repo, &["worktree", "prune"]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "heddle-wt-{tag}-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mk scratch");
        p
    }

    /// A tiny real git repo with one commit.
    pub(crate) fn git_repo(tag: &str) -> PathBuf {
        let dir = scratch(tag);
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "heddle@test"],
            vec!["config", "user.name", "heddle test"],
        ] {
            git(&dir, &args).expect("git setup");
        }
        std::fs::write(dir.join("f.txt"), "base\n").unwrap();
        git(&dir, &["add", "-A"]).expect("git add");
        git(&dir, &["commit", "-q", "-m", "base"]).expect("git commit");
        dir
    }

    #[test]
    fn add_and_remove_roundtrip_and_no_commit_repo_fails_honestly() {
        let repo = git_repo("rt");
        let dest = scratch("rt-dest").join("wt");
        add(&repo, &dest).expect("worktree add");
        assert!(dest.join("f.txt").exists(), "worktree has HEAD's files");
        assert!(dest.join(".git").exists(), "worktree is a git worktree");
        // Dirty worktrees still remove (callers checked capture first).
        std::fs::write(dest.join("f.txt"), "edited\n").unwrap();
        remove(&repo, &dest).expect("worktree remove");
        assert!(!dest.exists());
        // A git repo with no commits cannot host a worktree — honest error.
        let empty = scratch("rt-empty");
        git(&empty, &["init", "-q"]).unwrap();
        let e = add(&empty, &scratch("rt-e").join("wt")).unwrap_err();
        assert!(e.contains("git worktree failed"), "{e}");
    }
}
