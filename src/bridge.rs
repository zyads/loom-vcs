// Loom — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! The git bridge — meet every developer where they live.
//!
//! After a weave LANDS (green verify + operator Approve, files already
//! applied by `land_weave`), a repo registered with `git_bridge: true` gets
//! one local git commit on the CURRENT branch: `git add -A && git commit`,
//! message composed from the lease goal + criteria + the verify result.
//!
//! **Invariants:**
//! * Runs only after a weave landed — the bridge never creates state of its
//!   own, it only projects fabric history down into git.
//! * **Never pushes.** Nothing here talks to a remote, ever.
//! * Opt-in per repo at registration; off by default.
//! * Failure is reported, not retried — a repo with no git, nothing staged,
//!   or no `user.email` produces an honest error string in the caller's
//!   output and the log; the weave itself already landed and stays landed.
//!
//! TODO(git-bridge v2): per-thread draft-branch export ("threads can export
//! as draft branches for human review") — a future `loom export` verb.

use std::path::Path;

use super::weave::run_verify;
use super::{LandOutcome, VerifyResult};

/// Ceiling on one git invocation — `git add -A` on a huge tree is slow but
/// not 60-seconds slow.
const GIT_TIMEOUT_SECS: u64 = 60;

/// Compose the commit message a landed weave projects into git.
pub fn commit_message(out: &LandOutcome) -> String {
    let mut msg = out.thread.goal.clone();
    msg.push('\n');
    if !out.criteria.is_empty() {
        msg.push('\n');
        msg.push_str("criteria:\n");
        for c in &out.criteria {
            msg.push_str(&format!("- {c}\n"));
        }
    }
    msg.push_str(&format!("\nverify: green ({})\n", out.weave.verify.cmd));
    msg.push_str(&format!(
        "\nwoven-by: loom thread={} weave={}\n",
        out.thread.id, out.weave.id
    ));
    msg
}

/// Commit a landed weave onto the repo's current branch. Returns a short
/// human summary. Refuses (with an honest reason) when the repo was not
/// registered with the bridge on, or is not a git repo.
pub fn commit_landed_weave(out: &LandOutcome) -> Result<String, String> {
    if !out.repo.git_bridge {
        return Err("git bridge is off for this repo (register with git_bridge: true)".into());
    }
    let repo_dir = Path::new(&out.repo.path);
    if !repo_dir.join(".git").exists() {
        return Err(format!("{} is not a git repo", out.repo.path));
    }
    let add = run_verify("git add -A", repo_dir, GIT_TIMEOUT_SECS);
    if add.result != VerifyResult::Green {
        return Err(format!("git add failed: {}", add.log_tail));
    }
    let msg_file = repo_dir.join(".loom-commit-msg");
    std::fs::write(&msg_file, commit_message(out)).map_err(|e| format!("commit msg: {e}"))?;
    // -F keeps the message out of shell quoting entirely.
    let commit = run_verify("git commit -F .loom-commit-msg", repo_dir, GIT_TIMEOUT_SECS);
    let _ = std::fs::remove_file(&msg_file);
    if commit.result != VerifyResult::Green {
        return Err(format!("git commit failed: {}", commit.log_tail));
    }
    Ok(format!(
        "committed weave {} on the current branch (never pushed)",
        out.weave.id
    ))
}

#[cfg(test)]
mod tests {
    use super::super::{RepoConfig, Thread, ThreadStatus, VerifyOutcome, Weave};
    use super::*;

    fn landed() -> LandOutcome {
        LandOutcome {
            repo: RepoConfig {
                id: "repo-x".into(),
                path: "/tmp/nowhere".into(),
                verify_cmd: "cargo check".into(),
                git_bridge: false,
                registered_ms: 0,
                sync_remote: None,
                auto_sync: false,
            },
            thread: Thread {
                id: "thread-1".into(),
                repo_id: "repo-x".into(),
                goal: "refactor the parser".into(),
                head_stitch: None,
                lease_id: None,
                status: ThreadStatus::Woven,
                note: String::new(),
                approval_id: None,
                worktree: None,
                base_stitch: None,
            },
            weave: Weave {
                id: "weave-9".into(),
                thread_id: "thread-1".into(),
                fabric_parent: None,
                verify: VerifyOutcome {
                    cmd: "cargo check".into(),
                    result: super::super::VerifyResult::Green,
                    log_tail: String::new(),
                },
                ts_ms: 0,
                applied: Default::default(),
            },
            criteria: vec!["tests pass".into(), "no new warnings".into()],
            files_applied: 3,
        }
    }

    #[test]
    fn the_message_carries_goal_criteria_and_verify() {
        let msg = commit_message(&landed());
        assert!(msg.starts_with("refactor the parser\n"));
        assert!(msg.contains("- tests pass\n"));
        assert!(msg.contains("- no new warnings\n"));
        assert!(msg.contains("verify: green (cargo check)"));
        assert!(msg.contains("thread=thread-1 weave=weave-9"));
    }

    #[test]
    fn the_bridge_refuses_when_off_or_not_a_git_repo() {
        let mut out = landed();
        assert!(commit_landed_weave(&out)
            .unwrap_err()
            .contains("git bridge is off"));
        out.repo.git_bridge = true;
        let plain = std::env::temp_dir().join(format!("loom-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&plain).unwrap();
        out.repo.path = plain.to_string_lossy().to_string();
        assert!(commit_landed_weave(&out).unwrap_err().contains("not a git repo"));
    }
}
