// Loom — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! The git bridge — meet every developer where they live.
//!
//! After a weave LANDS (green verify + operator Approve, files already
//! applied by `land_weave`), a repo registered with `git_bridge: true`
//! projects the landing into local git history at the repo's configured
//! granularity ([`BridgeMode`]):
//!
//! * **`squash`** (default): one commit on the current branch, message
//!   composed from the lease goal + criteria + the verify result. One lease
//!   = one goal = one commit — the intended granularity.
//! * **`stitches`**: the thread's stitch chain replays as individual
//!   commits on a per-thread branch `loom/<thread-id-short>-<goal-slug>`
//!   ("stitch N of <goal>" + changed-file list each), then that branch
//!   merges into the current branch with a merge commit carrying the weave
//!   message. History shows every checkpoint AND the semantic landing.
//! * **`both`**: squash commit on the current branch + the per-thread
//!   branch preserved (not merged) for archaeology.
//!
//! `loom export` reuses the same replay to write an UNLANDED thread's chain
//! to its per-thread branch for human review — see [`build_thread_branch`].
//!
//! **Replay mechanics (plumbing only).** Each stitch commit is built with a
//! temporary index file (`GIT_INDEX_FILE`): `read-tree` the parent commit's
//! tree, batch-import the changed blobs from loom's content-addressed store
//! via `hash-object -w --stdin-paths`, stage adds/edits/tombstones with
//! `update-index -z --index-info`, then `write-tree` + `commit-tree` +
//! `update-ref refs/heads/<branch>`. Chosen over a scratch worktree because
//! it is O(changed files), never materializes a checkout, and *cannot*
//! touch the user's working tree, real index, or current branch. Stitches
//! whose effective diff vs the previous commit is empty are skipped. The
//! one exception is the final merge in `stitches` mode, which follows the
//! existing land path: the landed working tree is staged (`git add -A`) and
//! becomes the merge commit's tree, exactly what the squash commit records.
//!
//! **Invariants:**
//! * Runs only after a weave landed (export aside, which creates a branch
//!   ref and objects, never commits on the current branch) — the bridge
//!   never creates state of its own, it only projects fabric history down
//!   into git.
//! * **Never pushes.** Nothing here talks to a remote, ever.
//! * Opt-in per repo at registration; off by default.
//! * Failure is reported, not retried — a repo with no git, nothing staged,
//!   or no `user.email` produces an honest error string in the caller's
//!   output and the log; the weave itself already landed and stays landed.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use super::weave::run_verify;
use super::{BridgeMode, LandOutcome, Stitch, VerifyResult, TOMBSTONE};

/// Ceiling on one git invocation — `git add -A` on a huge tree is slow but
/// not 60-seconds slow.
const GIT_TIMEOUT_SECS: u64 = 60;

/// The all-zeros object id `update-index` uses to stage a removal.
const ZERO_OID: &str = "0000000000000000000000000000000000000000";

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

/// Project a landed weave into git at the repo's configured granularity.
/// Returns a short human summary. Refuses (with an honest reason) when the
/// repo was not registered with the bridge on, or is not a git repo.
pub fn commit_landed_weave(out: &LandOutcome) -> Result<String, String> {
    if !out.repo.git_bridge {
        return Err("git bridge is off for this repo (register with git_bridge: true)".into());
    }
    let repo_dir = Path::new(&out.repo.path);
    if !repo_dir.join(".git").exists() {
        return Err(format!("{} is not a git repo", out.repo.path));
    }
    match out.repo.bridge_mode {
        BridgeMode::Squash => squash_commit(out, repo_dir),
        BridgeMode::Stitches => stitches_commit(out, repo_dir),
        BridgeMode::Both => both_commit(out, repo_dir),
    }
}

/// `squash`: `git add -A && git commit` on the current branch — the v1 path.
fn squash_commit(out: &LandOutcome, repo_dir: &Path) -> Result<String, String> {
    let add = run_verify("git add -A", repo_dir, GIT_TIMEOUT_SECS);
    if add.result != VerifyResult::Green {
        return Err(format!("git add failed: {}", add.log_tail));
    }
    // The message file lives OUTSIDE the tree so no git pathspec can ever
    // pick it up; -F keeps the message out of shell quoting entirely.
    static MSG_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let msg_file = std::env::temp_dir().join(format!(
        "loom-commit-msg-{}-{}-{}",
        std::process::id(),
        super::now_ms(),
        MSG_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&msg_file, commit_message(out)).map_err(|e| format!("commit msg: {e}"))?;
    let commit = run_verify(
        &format!("git commit -F '{}'", msg_file.display()),
        repo_dir,
        GIT_TIMEOUT_SECS,
    );
    let _ = std::fs::remove_file(&msg_file);
    if commit.result != VerifyResult::Green {
        return Err(format!("git commit failed: {}", commit.log_tail));
    }
    Ok(format!(
        "committed weave {} on the current branch (never pushed)",
        out.weave.id
    ))
}

/// `stitches`: replay the chain on the per-thread branch, then merge it in
/// with the weave message. The merge's tree is the landed working tree
/// (staged with `git add -A`, like the squash path) — the fabric's state,
/// bit for bit.
fn stitches_commit(out: &LandOutcome, repo_dir: &Path) -> Result<String, String> {
    let head = head_commit(repo_dir)
        .map_err(|e| format!("stitches mode needs a commit on the current branch: {e}"))?;
    let branch = thread_branch_name(&out.thread.id, &out.thread.goal);
    let n = build_thread_branch(
        repo_dir,
        &head,
        &branch,
        &out.thread.goal,
        &out.stitches,
        &out.base_manifest,
        &out.objects_dir,
    )?;
    if n == 0 {
        // Nothing to replay (no stitch changed anything vs the branch base)
        // — a squash commit is the honest fallback.
        let note = squash_commit(out, repo_dir)?;
        return Ok(format!("{note}; no stitch commits to replay"));
    }
    let tip = git(repo_dir, None, None, &["rev-parse", &format!("refs/heads/{branch}")])?;
    // The landed state IS the working tree (land_weave just applied it);
    // stage it and let the merge commit carry exactly that tree.
    let add = run_verify("git add -A", repo_dir, GIT_TIMEOUT_SECS);
    if add.result != VerifyResult::Green {
        return Err(format!("git add failed: {}", add.log_tail));
    }
    let tree = git(repo_dir, None, None, &["write-tree"])?;
    let merge = git(
        repo_dir,
        None,
        Some(commit_message(out).as_bytes()),
        &["commit-tree", &tree, "-p", &head, "-p", &tip],
    )?;
    // Advance the current branch to the merge commit; `<old>` makes a
    // concurrent move fail instead of being clobbered. The working tree
    // already matches the merge's tree, so nothing on disk moves.
    git(repo_dir, None, None, &["update-ref", "HEAD", &merge, &head])?;
    Ok(format!(
        "replayed {n} stitch commit(s) on {branch} and merged into the current branch (never pushed)"
    ))
}

/// `both`: the squash commit, plus the per-thread branch left unmerged.
/// The branch bases at the pre-squash tip, so it diverges visibly.
fn both_commit(out: &LandOutcome, repo_dir: &Path) -> Result<String, String> {
    let pre = head_commit(repo_dir).ok();
    let note = squash_commit(out, repo_dir)?;
    // First-ever commit edge: with no pre-squash tip, base at the squash
    // commit itself.
    let base = match pre {
        Some(c) => c,
        None => head_commit(repo_dir)?,
    };
    let branch = thread_branch_name(&out.thread.id, &out.thread.goal);
    let n = build_thread_branch(
        repo_dir,
        &base,
        &branch,
        &out.thread.goal,
        &out.stitches,
        &out.base_manifest,
        &out.objects_dir,
    )?;
    if n == 0 {
        return Ok(format!("{note}; no stitch commits to preserve"));
    }
    Ok(format!(
        "{note}; preserved {n} stitch commit(s) on {branch} (unmerged)"
    ))
}

/// The current branch tip's commit oid.
pub fn head_commit(repo_dir: &Path) -> Result<String, String> {
    git(repo_dir, None, None, &["rev-parse", "--verify", "HEAD^{commit}"])
}

/// The per-thread branch a thread's checkpoints replay onto:
/// `loom/<thread-id-short>-<goal-slug>`. Deterministic, so land and export
/// refresh the same branch.
pub fn thread_branch_name(thread_id: &str, goal: &str) -> String {
    let short = thread_id.strip_prefix("thread-").unwrap_or(thread_id);
    let slug: String = goal
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug: String = slug.chars().take(40).collect();
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        format!("loom/{short}")
    } else {
        format!("loom/{short}-{slug}")
    }
}

/// Replay a stitch chain (oldest first) as commits on
/// `refs/heads/<branch>`, branching from `base_commit`. Each commit's
/// message is "stitch N of <goal>" (N = position in the chain) plus the
/// changed-file list; a stitch whose effective diff vs the previous commit
/// is empty is skipped. Returns how many commits were made; the branch ref
/// is (re)written only when that is non-zero.
///
/// Pure plumbing on a temporary index — the user's working tree, real
/// index, and current branch are never touched. Blob content comes from
/// loom's object store; a missing blob is an honest error.
pub fn build_thread_branch(
    repo_dir: &Path,
    base_commit: &str,
    branch: &str,
    goal: &str,
    stitches: &[Stitch],
    base_manifest: &BTreeMap<String, String>,
    objects: &Path,
) -> Result<usize, String> {
    let mut parent = base_commit.to_string();
    let mut parent_tree = git(
        repo_dir,
        None,
        None,
        &["rev-parse", &format!("{base_commit}^{{tree}}")],
    )?;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let index = std::env::temp_dir().join(format!(
        "loom-bridge-index-{}-{}-{}",
        std::process::id(),
        super::now_ms(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    // A stitch manifest records TOMBSTONE for deletions; "absent" and
    // "tombstoned" are the same effective state.
    let eff = |m: &BTreeMap<String, String>, rel: &str| -> Option<String> {
        m.get(rel).filter(|h| *h != TOMBSTONE).cloned()
    };
    let result = (|| -> Result<usize, String> {
        let mut prev = base_manifest.clone();
        let mut commits = 0usize;
        for (i, st) in stitches.iter().enumerate() {
            // Effective diff vs the previous checkpoint.
            let mut rels: Vec<&String> = prev.keys().chain(st.files.keys()).collect();
            rels.sort();
            rels.dedup();
            let mut adds: Vec<(String, String)> = Vec::new(); // rel → loom hash
            let mut dels: Vec<String> = Vec::new();
            for rel in rels {
                let before = eff(&prev, rel);
                let after = eff(&st.files, rel);
                if before == after {
                    continue;
                }
                match after {
                    Some(h) => adds.push((rel.clone(), h)),
                    None => dels.push(rel.clone()),
                }
            }
            prev = st.files.clone();
            if adds.is_empty() && dels.is_empty() {
                continue; // empty-diff stitch — no commit
            }
            // Stage onto the parent's tree in the temp index.
            git(
                repo_dir,
                Some(&index),
                None,
                &["read-tree", &parent_tree],
            )?;
            // Batch-import changed blobs from the loom store into git's
            // object db; output oids come back in input order.
            let mut oids: Vec<String> = Vec::new();
            if !adds.is_empty() {
                let mut paths = String::new();
                for (rel, h) in &adds {
                    let p = super::store::blob_path(objects, h);
                    if !p.exists() {
                        return Err(format!("blob for {rel} ({h}) missing from the loom store"));
                    }
                    paths.push_str(&p.to_string_lossy());
                    paths.push('\n');
                }
                let out = git(
                    repo_dir,
                    None,
                    Some(paths.as_bytes()),
                    &["hash-object", "-w", "--stdin-paths"],
                )?;
                oids = out.lines().map(|l| l.trim().to_string()).collect();
                if oids.len() != adds.len() {
                    return Err(format!(
                        "git hash-object returned {} oids for {} blobs",
                        oids.len(),
                        adds.len()
                    ));
                }
            }
            let mut info = Vec::new();
            for ((rel, _), oid) in adds.iter().zip(&oids) {
                info.extend_from_slice(format!("100644 {oid}\t{rel}\0").as_bytes());
            }
            for rel in &dels {
                info.extend_from_slice(format!("0 {ZERO_OID}\t{rel}\0").as_bytes());
            }
            git(
                repo_dir,
                Some(&index),
                Some(&info),
                &["update-index", "-z", "--index-info"],
            )?;
            let tree = git(repo_dir, Some(&index), None, &["write-tree"])?;
            if tree == parent_tree {
                continue; // content-identical after staging — still empty
            }
            let mut msg = format!("stitch {} of {goal}\n\nfiles:\n", i + 1);
            for (rel, _) in &adds {
                msg.push_str(&format!("- {rel}\n"));
            }
            for rel in &dels {
                msg.push_str(&format!("- {rel} (deleted)\n"));
            }
            let commit = git(
                repo_dir,
                None,
                Some(msg.as_bytes()),
                &["commit-tree", &tree, "-p", &parent],
            )?;
            parent = commit;
            parent_tree = tree;
            commits += 1;
        }
        if commits > 0 {
            git(
                repo_dir,
                None,
                None,
                &["update-ref", &format!("refs/heads/{branch}"), &parent],
            )?;
        }
        Ok(commits)
    })();
    let _ = std::fs::remove_file(&index);
    result
}

/// Run one git plumbing command in `repo_dir`, optionally against a
/// dedicated index file (`GIT_INDEX_FILE`) and/or with bytes on stdin.
/// Errors carry git's stderr. Local plumbing is fast; no timeout loop —
/// the porcelain calls that can be slow (`add -A`, `commit`) go through
/// [`run_verify`] with its hard timeout instead.
fn git(
    repo_dir: &Path,
    index: Option<&Path>,
    stdin: Option<&[u8]>,
    args: &[&str],
) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo_dir).args(args);
    if let Some(ix) = index {
        cmd.env("GIT_INDEX_FILE", ix);
    }
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot run git {}: {e}", args.first().unwrap_or(&"?")))?;
    // Feed stdin from a helper thread while the parent drains stdout —
    // `hash-object --stdin-paths` on a big batch writes output as it reads
    // input, and a single-threaded write-then-read could deadlock on full
    // pipe buffers.
    let feeder = stdin.map(|bytes| {
        let mut pipe = child.stdin.take().expect("piped above");
        let bytes = bytes.to_vec();
        std::thread::spawn(move || {
            let _ = pipe.write_all(&bytes);
            // Dropping the handle closes the pipe so git sees EOF.
        })
    });
    let out = child
        .wait_with_output()
        .map_err(|e| format!("git {} wait: {e}", args[0]))?;
    if let Some(f) = feeder {
        let _ = f.join();
    }
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        Loom, RepoConfig, Thread, ThreadStatus, VerifyOutcome, Weave,
    };
    use super::*;
    use std::path::PathBuf;

    fn landed() -> LandOutcome {
        LandOutcome {
            repo: RepoConfig {
                id: "repo-x".into(),
                path: "/tmp/nowhere".into(),
                verify_cmd: "cargo check".into(),
                git_bridge: false,
                bridge_mode: BridgeMode::Squash,
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
            stitches: Vec::new(),
            base_manifest: BTreeMap::new(),
            objects_dir: PathBuf::new(),
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

    #[test]
    fn bridge_mode_parses_defaults_and_serdes() {
        assert_eq!("squash".parse::<BridgeMode>().unwrap(), BridgeMode::Squash);
        assert_eq!("Stitches".parse::<BridgeMode>().unwrap(), BridgeMode::Stitches);
        assert_eq!("both".parse::<BridgeMode>().unwrap(), BridgeMode::Both);
        assert!("weekly".parse::<BridgeMode>().is_err());
        // A RepoConfig persisted before bridge_mode existed loads as Squash.
        let old = r#"{"id":"repo-a","path":"/x","verify_cmd":"true",
                      "git_bridge":true,"registered_ms":0}"#;
        let cfg: RepoConfig = serde_json::from_str(old).expect("old state loads");
        assert_eq!(cfg.bridge_mode, BridgeMode::Squash);
    }

    // -- end-to-end rigs -----------------------------------------------------

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "loom-bridge-{tag}-{}-{}",
            std::process::id(),
            super::super::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mk scratch");
        p
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new(args[0])
            .args(&args[1..])
            .current_dir(dir)
            .output()
            .expect("run");
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A real git repo with one commit, registered with the bridge on.
    fn git_rig(tag: &str, mode: BridgeMode) -> (Loom, PathBuf, RepoConfig) {
        let base = scratch(&format!("{tag}-data"));
        let repo_dir = scratch(&format!("{tag}-repo"));
        std::fs::create_dir_all(repo_dir.join("src")).unwrap();
        std::fs::write(repo_dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(repo_dir.join("src/util.rs"), "pub fn u() {}\n").unwrap();
        for args in [
            vec!["git", "init", "-q"],
            vec!["git", "config", "user.email", "loom@test"],
            vec!["git", "config", "user.name", "loom test"],
            vec!["git", "add", "-A"],
            vec!["git", "commit", "-q", "-m", "base"],
        ] {
            sh(&repo_dir, &args);
        }
        let loom = Loom::at(base);
        let repo = loom
            .register_repo(repo_dir.to_str().unwrap(), Some("true".into()), true)
            .expect("register");
        let repo = loom.set_bridge_mode(&repo.id, mode).expect("set mode");
        (loom, repo_dir, repo)
    }

    /// Lease → edit(s)+stitch(es) → propose → land, returning the LandOutcome.
    fn land_one(
        loom: &Loom,
        repo: &RepoConfig,
        goal: &str,
        edits: &[&[(&str, &str)]],
    ) -> LandOutcome {
        let d = loom
            .declare_lease(&repo.id, "t", goal, vec!["src/**".into()], vec!["tests pass".into()], None)
            .expect("lease");
        let wt = PathBuf::from(d.thread.worktree.as_ref().expect("isolated"));
        for batch in edits {
            for (rel, body) in *batch {
                std::fs::write(wt.join(rel), body).unwrap();
            }
            let s = loom.stitch(&d.lease.id).expect("stitch");
            assert!(!s.unchanged, "each batch changes something");
        }
        let p = loom.propose(&d.thread.id).expect("propose");
        assert!(p.green);
        loom.land_weave(&p.weave.id).expect("land")
    }

    #[test]
    fn squash_mode_projects_one_commit_per_weave() {
        let (loom, repo_dir, repo) = git_rig("squash", BridgeMode::Squash);
        let out = land_one(
            &loom,
            &repo,
            "greet in french",
            &[&[("src/main.rs", "fn main() { /* bonjour */ }\n")]],
        );
        let note = commit_landed_weave(&out).expect("bridge");
        assert!(note.contains("committed weave"), "{note}");
        assert_eq!(sh(&repo_dir, &["git", "rev-list", "--count", "HEAD"]), "2");
        let msg = sh(&repo_dir, &["git", "log", "-1", "--format=%B"]);
        assert!(msg.starts_with("greet in french"), "{msg}");
        assert!(msg.contains("- tests pass"));
        assert!(msg.contains("verify: green (true)"));
        assert_eq!(sh(&repo_dir, &["git", "status", "--porcelain"]), "");
    }

    #[test]
    fn stitches_mode_replays_checkpoints_and_merges_with_the_weave_message() {
        let (loom, repo_dir, repo) = git_rig("stitches", BridgeMode::Stitches);
        let out = land_one(
            &loom,
            &repo,
            "rework main twice",
            &[
                &[("src/main.rs", "fn main() { /* v1 */ }\n")],
                &[
                    ("src/main.rs", "fn main() { /* v2 */ }\n"),
                    ("src/util.rs", "pub fn u() { /* v2 */ }\n"),
                ],
            ],
        );
        assert_eq!(out.stitches.len(), 2, "the chain rides on the outcome");
        let note = commit_landed_weave(&out).expect("bridge");
        assert!(note.contains("replayed 2 stitch commit(s)"), "{note}");
        let branch = thread_branch_name(&out.thread.id, &out.thread.goal);
        assert!(branch.starts_with("loom/"), "{branch}");
        assert!(branch.ends_with("-rework-main-twice"), "{branch}");
        // The branch: base + 2 checkpoint commits, message shape asserted.
        assert_eq!(sh(&repo_dir, &["git", "rev-list", "--count", &branch]), "3");
        let subjects = sh(&repo_dir, &["git", "log", "--format=%s", &branch]);
        assert!(subjects.contains("stitch 1 of rework main twice"), "{subjects}");
        assert!(subjects.contains("stitch 2 of rework main twice"), "{subjects}");
        let body2 = sh(&repo_dir, &["git", "log", "-1", "--format=%B", &branch]);
        assert!(body2.contains("- src/main.rs"), "{body2}");
        assert!(body2.contains("- src/util.rs"), "{body2}");
        // Each checkpoint's content is real: the branch tip has v2.
        let tip_main = sh(&repo_dir, &["git", "show", &format!("{branch}:src/main.rs")]);
        assert!(tip_main.contains("v2"));
        // The current branch got a MERGE commit carrying the weave message.
        let parents = sh(&repo_dir, &["git", "rev-list", "--parents", "-1", "HEAD"]);
        assert_eq!(parents.split_whitespace().count(), 3, "two parents: {parents}");
        let msg = sh(&repo_dir, &["git", "log", "-1", "--format=%B"]);
        assert!(msg.starts_with("rework main twice"), "{msg}");
        assert!(msg.contains("verify: green (true)"));
        // The landed tree is intact and the repo is clean.
        assert!(std::fs::read_to_string(repo_dir.join("src/main.rs"))
            .unwrap()
            .contains("v2"));
        assert_eq!(sh(&repo_dir, &["git", "status", "--porcelain"]), "");
    }

    #[test]
    fn both_mode_squashes_and_keeps_the_branch_unmerged() {
        let (loom, repo_dir, repo) = git_rig("both", BridgeMode::Both);
        let out = land_one(
            &loom,
            &repo,
            "tune util",
            &[&[("src/util.rs", "pub fn u() { /* tuned */ }\n")]],
        );
        let note = commit_landed_weave(&out).expect("bridge");
        assert!(note.contains("preserved 1 stitch commit(s)"), "{note}");
        // Current branch: one squash commit, single parent, goal message.
        assert_eq!(sh(&repo_dir, &["git", "rev-list", "--count", "HEAD"]), "2");
        let parents = sh(&repo_dir, &["git", "rev-list", "--parents", "-1", "HEAD"]);
        assert_eq!(parents.split_whitespace().count(), 2, "one parent: {parents}");
        // The per-thread branch exists and its tip is NOT in HEAD's history.
        let branch = thread_branch_name(&out.thread.id, &out.thread.goal);
        let tip = sh(&repo_dir, &["git", "rev-parse", &branch]);
        let ancestor = std::process::Command::new("git")
            .args(["merge-base", "--is-ancestor", &tip, "HEAD"])
            .current_dir(&repo_dir)
            .status()
            .unwrap();
        assert!(!ancestor.success(), "branch stays unmerged");
        assert_eq!(sh(&repo_dir, &["git", "status", "--porcelain"]), "");
    }

    #[test]
    fn export_writes_the_branch_without_landing_or_dirtying_anything() {
        // Bridge OFF: export must still work — it is review, not landing.
        let (loom, repo_dir, _repo) = git_rig("export", BridgeMode::Squash);
        let repo = loom
            .register_repo(repo_dir.to_str().unwrap(), Some("true".into()), false)
            .expect("re-register bridge off");
        let d = loom
            .declare_lease(&repo.id, "t", "in flight work", vec!["src/**".into()], vec![], None)
            .expect("lease");
        let wt = PathBuf::from(d.thread.worktree.as_ref().unwrap());
        // Exporting before any stitch refuses honestly.
        let err = loom.export_thread(&d.thread.id).unwrap_err();
        assert!(err.contains("capture a stitch first"), "{err}");
        std::fs::write(wt.join("src/main.rs"), "fn main() { /* wip 1 */ }\n").unwrap();
        loom.stitch(&d.lease.id).expect("stitch 1");
        std::fs::write(wt.join("src/main.rs"), "fn main() { /* wip 2 */ }\n").unwrap();
        loom.stitch(&d.lease.id).expect("stitch 2");
        let head_before = sh(&repo_dir, &["git", "rev-parse", "HEAD"]);
        let out = loom.export_thread(&d.thread.id).expect("export");
        assert_eq!(out.commits, 2);
        assert!(out.branch.contains("in-flight-work"), "{}", out.branch);
        // The branch holds the in-flight chain…
        assert_eq!(
            sh(&repo_dir, &["git", "rev-list", "--count", &out.branch]),
            "3"
        );
        let tip = sh(&repo_dir, &["git", "show", &format!("{}:src/main.rs", out.branch)]);
        assert!(tip.contains("wip 2"));
        // …and NOTHING landed: repo tree, HEAD, status, fabric all untouched.
        assert_eq!(
            std::fs::read_to_string(repo_dir.join("src/main.rs")).unwrap(),
            "fn main() {}\n"
        );
        assert_eq!(sh(&repo_dir, &["git", "rev-parse", "HEAD"]), head_before);
        assert_eq!(sh(&repo_dir, &["git", "status", "--porcelain"]), "");
        let snap = loom.snapshot();
        assert!(snap.repo_states[&repo.id].fabric.tip.is_none());
        assert_eq!(
            snap.repo_states[&repo.id].threads[0].status,
            ThreadStatus::Active
        );
        // Re-export after more work refreshes the SAME branch.
        std::fs::write(wt.join("src/main.rs"), "fn main() { /* wip 3 */ }\n").unwrap();
        loom.stitch(&d.lease.id).expect("stitch 3");
        let again = loom.export_thread(&d.thread.id).expect("re-export");
        assert_eq!(again.branch, out.branch);
        assert_eq!(again.commits, 3);
    }

    #[test]
    fn empty_diff_stitches_are_skipped_in_replay() {
        let (_loom, repo_dir, _repo) = git_rig("skip", BridgeMode::Squash);
        let objects = scratch("skip-obj");
        let h1 = super::super::store::put_blob(&objects, b"one\n").unwrap();
        let mk = |id: &str, parent: Option<&str>, files: &[(&str, &str)]| Stitch {
            id: id.into(),
            thread_id: "thread-s".into(),
            parent: parent.map(String::from),
            files: files.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ts_ms: 0,
        };
        let s1 = mk("s1", None, &[("src/new.rs", h1.as_str())]);
        let s2 = mk("s2", Some("s1"), &[("src/new.rs", h1.as_str())]); // no change
        let s3 = mk(
            "s3",
            Some("s2"),
            &[("src/new.rs", TOMBSTONE), ("src/main.rs", h1.as_str())],
        );
        let head = head_commit(&repo_dir).unwrap();
        let n = build_thread_branch(
            &repo_dir,
            &head,
            "loom/skip-test",
            "skip test",
            &[s1, s2, s3],
            &BTreeMap::new(),
            &objects,
        )
        .expect("replay");
        assert_eq!(n, 2, "the identical stitch made no commit");
        assert_eq!(
            sh(&repo_dir, &["git", "rev-list", "--count", "loom/skip-test"]),
            "3"
        );
        let subjects = sh(&repo_dir, &["git", "log", "--format=%s", "loom/skip-test"]);
        assert!(subjects.contains("stitch 1 of skip test"), "{subjects}");
        assert!(
            subjects.contains("stitch 3 of skip test"),
            "numbering follows the chain, not the commits: {subjects}"
        );
        let body = sh(&repo_dir, &["git", "log", "-1", "--format=%B", "loom/skip-test"]);
        assert!(body.contains("- src/new.rs (deleted)"), "{body}");
        // Tombstone applied: the file is gone from the tip tree.
        let ls = sh(&repo_dir, &["git", "ls-tree", "-r", "--name-only", "loom/skip-test"]);
        assert!(!ls.contains("src/new.rs"), "{ls}");
        assert!(ls.contains("src/main.rs"), "{ls}");
        // And the working tree was never touched.
        assert_eq!(sh(&repo_dir, &["git", "status", "--porcelain"]), "");
    }
}
