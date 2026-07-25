// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! Consent — the human yes that stands between a green verify and the real
//! working tree.
//!
//! The engine's invariant is that [`crate::Heddle::land_weave`] — the only
//! path that writes into the real repo — runs only past an explicit human
//! yes. *How* that yes is collected is the embedder's business:
//!
//! * The standalone `heddle` binary asks at the terminal ([`TerminalConsent`]):
//!   it shows the repo, the goal, and the verify result BEFORE asking, and
//!   refuses outright when stdin is not a terminal — a prompt nobody can see
//!   is not consent.
//! * Non-interactive contexts (tests, the MCP server, CI) use [`AutoDeny`],
//!   which always refuses and says why. A green verify still gets recorded;
//!   the honest answer stays *"verified green; nothing was applied."*
//! * An embedding host with its own approvals queue implements
//!   [`WeaveConsent`] over that queue instead (parking an approval and
//!   resolving it later — see `Heddle::mark_parked` / `Heddle::reconcile_parked`
//!   for the bookkeeping that keeps async approvals honest).
//!
//! [`Heddle::propose_with_consent`] is the shared propose→consent→land flow
//! for synchronous consenters (CLI, MCP): red prints nothing here and lands
//! nothing; green asks; a refusal withdraws the thread back to Active with
//! the reason noted, so a later `heddle propose` at a real terminal can run
//! the whole gate again.

use crate::{bridge, LandOutcome, Heddle, Thread, Weave};

/// The one question: "apply this verified-green weave to the real working
/// tree?" `Ok(())` is an explicit yes; `Err(reason)` is a refusal with the
/// reason a human (and the thread's note) will read.
pub trait WeaveConsent {
    fn confirm(&self, summary: &str) -> Result<(), String>;
}

/// Interactive y/N prompt on the controlling terminal. Prints the summary
/// (repo, goal, verify result — composed by the caller) before asking, and
/// refuses when stdin is not a terminal: consent cannot be assumed, defaulted
/// or piped in.
pub struct TerminalConsent;

impl WeaveConsent for TerminalConsent {
    fn confirm(&self, summary: &str) -> Result<(), String> {
        use std::io::{BufRead, IsTerminal, Write};
        if !std::io::stdin().is_terminal() {
            return Err(
                "stdin is not a terminal — refusing to apply without a human at the prompt; \
                 re-run `heddle propose` interactively"
                    .to_string(),
            );
        }
        eprintln!("{summary}");
        eprint!("Apply this weave to the working tree? [y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| format!("could not read the answer: {e}"))?;
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => Ok(()),
            _ => Err("declined at the terminal".to_string()),
        }
    }
}

/// Always refuses, and says why. For tests, the MCP server, and any other
/// non-interactive context: a green verify is still recorded, but nothing
/// reaches the working tree without a human at a terminal.
pub struct AutoDeny;

impl WeaveConsent for AutoDeny {
    fn confirm(&self, _summary: &str) -> Result<(), String> {
        Err(
            "non-interactive context — applying a weave to the real working tree requires an \
             explicit human yes; run `heddle propose` at a terminal"
                .to_string(),
        )
    }
}

/// How one propose→consent→land pass ended. Every variant is honest about
/// what did and did not touch the working tree.
#[derive(Debug)]
pub enum WeaveDisposition {
    /// Verify was red. Nothing can land; the thread stays Active with the
    /// failure noted. The weave carries the bounded log tail.
    Red { weave: Weave, thread: Thread },
    /// Verify was green but consent was refused. Nothing was applied; the
    /// thread was withdrawn back to Active with the reason, ready to
    /// re-propose.
    Refused {
        weave: Weave,
        thread: Thread,
        reason: String,
    },
    /// Verify was green, consent was given, and the weave landed: files
    /// applied to the working tree, fabric advanced, lease released. `git`
    /// is the bridge result when the repo has `git_bridge: true` (`None`
    /// when the bridge is off).
    Landed {
        land: LandOutcome,
        git: Option<Result<String, String>>,
    },
}

impl Heddle {
    /// The full synchronous gate: run [`Heddle::propose`] (scratch worktree +
    /// verify), then on green ask `consent`, then on yes [`Heddle::land_weave`]
    /// (+ the git bridge when the repo opted in). On refusal the thread is
    /// withdrawn back to Active with the refusal reason — re-proposable at
    /// any time. Blocking, like `propose`.
    pub fn propose_with_consent(
        &self,
        thread_id: &str,
        consent: &dyn WeaveConsent,
    ) -> Result<WeaveDisposition, String> {
        let out = self.propose(thread_id)?;
        if !out.green {
            return Ok(WeaveDisposition::Red {
                weave: out.weave,
                thread: out.thread,
            });
        }
        let summary = format!(
            "weave ready to land\n  repo:     {}\n  goal:     {}\n  verify:   green ({})\n  files:    {} in head stitch\n  landing applies those files to the working tree{}",
            out.repo.path,
            out.thread.goal,
            out.weave.verify.cmd,
            head_stitch_files(self, thread_id),
            if out.repo.git_bridge {
                " and makes one local git commit (never a push)"
            } else {
                ""
            },
        );
        match consent.confirm(&summary) {
            Ok(()) => {
                let land = self.land_weave(&out.weave.id)?;
                let git = if land.repo.git_bridge {
                    Some(bridge::commit_landed_weave(&land))
                } else {
                    None
                };
                Ok(WeaveDisposition::Landed { land, git })
            }
            Err(reason) => {
                // Record the refusal on the weave, then withdraw the thread
                // to Active so a plain re-propose works — the standalone
                // flow has no pending-approvals queue to wait on.
                let _ = self.deny_weave(&out.weave.id, &format!("not applied: {reason}"));
                let (thread, _aid) = self.withdraw(
                    thread_id,
                    &format!("verified green, not applied ({reason}) — re-propose when ready"),
                )?;
                Ok(WeaveDisposition::Refused {
                    weave: out.weave,
                    thread,
                    reason,
                })
            }
        }
    }
}

/// File count in the thread's head stitch, best-effort (for the summary).
fn head_stitch_files(heddle: &Heddle, thread_id: &str) -> usize {
    let snap = heddle.snapshot();
    snap.repo_states
        .values()
        .flat_map(|rs| rs.threads.iter().map(move |t| (t, rs)))
        .find(|(t, _)| t.id == thread_id)
        .and_then(|(t, rs)| {
            t.head_stitch
                .as_ref()
                .and_then(|h| rs.stitches.iter().find(|s| s.id == *h))
        })
        .map(|s| s.files.len())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ThreadStatus;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "heddle-consent-{tag}-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mk scratch");
        p
    }

    fn rig(tag: &str, verify: &str) -> (Heddle, crate::RepoConfig) {
        let base = scratch(&format!("{tag}-data"));
        let repo_dir = scratch(&format!("{tag}-repo"));
        std::fs::create_dir_all(repo_dir.join("src")).unwrap();
        std::fs::write(repo_dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        let heddle = Heddle::at(base);
        let repo = heddle
            .register_repo(repo_dir.to_str().unwrap(), Some(verify.into()), false)
            .expect("register");
        (heddle, repo)
    }

    struct AlwaysYes;
    impl WeaveConsent for AlwaysYes {
        fn confirm(&self, _s: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn auto_deny_refuses_green_and_the_thread_returns_to_active_reproposable() {
        let (heddle, repo) = rig("deny", "true");
        let d = heddle
            .declare_lease(&repo.id, "t", "goal", vec!["src/**".into()], vec![], None)
            .expect("lease");
        heddle.stitch(&d.lease.id).expect("stitch");
        let disp = heddle
            .propose_with_consent(&d.thread.id, &AutoDeny)
            .expect("gate runs");
        match disp {
            WeaveDisposition::Refused { thread, reason, .. } => {
                assert_eq!(thread.status, ThreadStatus::Active);
                assert!(reason.contains("non-interactive"), "states why: {reason}");
                assert!(
                    thread.note.contains("verified green, not applied"),
                    "honest note: {}",
                    thread.note
                );
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        // Nothing landed.
        let snap = heddle.snapshot();
        assert!(snap.repo_states[&repo.id].fabric.tip.is_none());
        // Recovery is real: consent that says yes lands on re-propose.
        let disp = heddle
            .propose_with_consent(&d.thread.id, &AlwaysYes)
            .expect("re-propose");
        match disp {
            WeaveDisposition::Landed { land, git } => {
                assert_eq!(land.thread.status, ThreadStatus::Woven);
                assert!(git.is_none(), "bridge off for this repo");
            }
            other => panic!("expected Landed, got {other:?}"),
        }
        assert!(heddle.snapshot().repo_states[&repo.id].fabric.tip.is_some());
    }

    #[test]
    fn red_verify_never_asks_for_consent() {
        struct MustNotAsk;
        impl WeaveConsent for MustNotAsk {
            fn confirm(&self, _s: &str) -> Result<(), String> {
                panic!("consent must never be asked for a red verify");
            }
        }
        let (heddle, repo) = rig("red", "false");
        let d = heddle
            .declare_lease(&repo.id, "t", "goal", vec!["src/**".into()], vec![], None)
            .expect("lease");
        heddle.stitch(&d.lease.id).expect("stitch");
        let disp = heddle
            .propose_with_consent(&d.thread.id, &MustNotAsk)
            .expect("gate runs");
        match disp {
            WeaveDisposition::Red { thread, .. } => {
                assert_eq!(thread.status, ThreadStatus::Active);
                assert!(thread.note.starts_with("verify red"));
            }
            other => panic!("expected Red, got {other:?}"),
        }
        assert!(heddle.snapshot().repo_states[&repo.id].fabric.tip.is_none());
    }
}
