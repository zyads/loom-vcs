// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! `heddle savings` — did Heddle earn its keep in this repo? Counted facts
//! first; one clearly-labeled estimate after, with every constant printed.
//!
//! **The honesty rules of this module, stated up front:**
//!
//! * Heddle cannot know counterfactual token spend. There is no "you saved
//!   1.2M tokens" banner here and there never will be. What it CAN do is
//!   count real events — collisions warned at lease time, same-file
//!   concurrent edits the worktree isolation absorbed, lands refused because
//!   the fabric moved, rebases — and report those as facts.
//! * The estimate section prices exactly ONE thing: same-file concurrent-edit
//!   pairs ([`crate::OverlapEdit`]) — the moments git would have let one
//!   thread silently overwrite another. Warnings and refusals are listed but
//!   never monetized: an agent that self-partitioned after a warning saved
//!   re-work this module cannot honestly measure.
//! * Every constant in the model is either measured from this repo's own
//!   data or printed as a stated assumption next to the number it produced.
//!   Real token counts recorded via `--record-tokens` replace the assumption.
//! * When nothing measurable was prevented, the report says exactly that.
//!   That sentence is the feature.
//!
//! Counts come from the repo's event log, which is size-bounded
//! ([`store::MAX_LOG_BYTES`]) — so every number is a floor, and the report
//! says so.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{store, Heddle, RepoConfig, TOMBSTONE};

/// The stated assumption used ONLY when no real token counts were recorded:
/// tokens of agent work per stitched line. A line of code is ~10 output
/// tokens, but re-doing it costs context re-reads and retries on top; 20 is
/// a deliberate lowball guess, not data — which is why the report prints it
/// inline and tells you how to replace it.
pub const ASSUMED_TOKENS_PER_STITCHED_LINE: u64 = 20;

/// One unordered thread pair that edited the same file(s) while both were
/// live, deduplicated across the whole log.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OverlapPair {
    pub thread_a: String,
    pub thread_b: String,
    pub files: Vec<String>,
}

/// The counted facts — every field is a floor (the log rotates).
#[derive(Clone, Debug, Default, Serialize)]
pub struct Counted {
    pub threads_declared: u64,
    pub threads_woven: u64,
    pub orphan_events: u64,
    pub adoptions: u64,
    pub exports: u64,
    pub stitches: u64,
    /// Toe-steps: scope overlap with a live lease, surfaced at declaration.
    pub collisions_at_lease: u64,
    /// Warned thread pairs with no same-file edit recorded so far —
    /// self-partitioned or naturally disjoint; Heddle cannot tell which.
    pub warned_pairs_no_overlap_yet: u64,
    /// Same-file concurrent edits the isolation absorbed.
    pub overlap_pairs: u64,
    pub overlap_files: u64,
    pub overlap_detail: Vec<OverlapPair>,
    /// Lands refused with "fabric moved under you".
    pub lands_refused_fabric_moved: u64,
    pub rebases: u64,
    pub rebase_fast_forwarded_files: u64,
    pub rebase_conflict_files: u64,
}

impl Counted {
    /// The honest zero: nothing collision-shaped ever happened here.
    pub fn prevented_nothing(&self) -> bool {
        self.collisions_at_lease == 0
            && self.overlap_pairs == 0
            && self.lands_refused_fabric_moved == 0
            && self.rebases == 0
    }
}

/// Real token counts the user attached to threads (`--record-tokens`).
#[derive(Clone, Debug, Default, Serialize)]
pub struct TokenGroundTruth {
    pub recorded: BTreeMap<String, u64>,
    pub median: Option<u64>,
}

/// The one estimate, with its inputs alongside so the arithmetic is
/// checkable at a glance.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Estimate {
    /// What is being priced: same-file concurrent-edit pairs.
    pub overlap_pairs: u64,
    /// Cost of re-doing one thread, when it could be derived at all.
    pub redo_cost_tokens: Option<u64>,
    /// Where that cost came from — measured, or a printed assumption.
    pub redo_cost_basis: String,
    /// `overlap_pairs × redo_cost_tokens`, when both exist.
    pub tokens_avoided: Option<u64>,
    /// Median changed lines per thread, measured from local blobs.
    pub measured_median_stitched_lines: Option<u64>,
    /// How many threads that median was measured over.
    pub measured_lines_threads: u64,
    /// Set only when the per-line assumption was actually used.
    pub assumed_tokens_per_line: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SavingsReport {
    pub repo_id: String,
    pub repo_path: String,
    pub counted: Counted,
    pub tokens: TokenGroundTruth,
    pub estimate: Estimate,
}

/// Where recorded token counts live: beside the repo's state, so they
/// survive thread pruning and ride along if the state dir is moved.
pub fn tokens_path(base: &Path, repo_id: &str) -> PathBuf {
    base.join(repo_id).join("tokens.json")
}

fn load_tokens(base: &Path, repo_id: &str) -> BTreeMap<String, u64> {
    std::fs::read_to_string(tokens_path(base, repo_id))
        .ok()
        .and_then(|b| serde_json::from_str(&b).ok())
        .unwrap_or_default()
}

/// Attach a real token count to a thread. The thread must be known — in the
/// current state or anywhere in the event log — so a typo cannot mint
/// ground truth for work that never happened. Re-recording overwrites
/// (harnesses report totals, not deltas).
pub fn record_tokens(
    engine: &Heddle,
    repo_id: &str,
    thread_id: &str,
    tokens: u64,
) -> Result<TokenGroundTruth, String> {
    let known_in_state = engine
        .snapshot()
        .repo_states
        .get(repo_id)
        .map(|rs| rs.threads.iter().any(|t| t.id == thread_id))
        .unwrap_or(false);
    let base = engine.base();
    let known_in_log = known_in_state
        || store::read_events(&base, repo_id, usize::MAX)
            .iter()
            .any(|e| e.get("thread").and_then(|v| v.as_str()) == Some(thread_id));
    if !known_in_log {
        return Err(format!(
            "no thread {thread_id} in this repo's state or event log — \
             `heddle status` / `heddle log` list real thread ids"
        ));
    }
    let mut map = load_tokens(&base, repo_id);
    map.insert(thread_id.to_string(), tokens);
    store::write_json_0600(&tokens_path(&base, repo_id), &map);
    store::append_event(
        &base,
        repo_id,
        &serde_json::json!({
            "ts_ms": super::now_ms(), "kind": "tokens_recorded",
            "thread": thread_id, "tokens": tokens,
        }),
    );
    let median = median(&map.values().copied().collect::<Vec<_>>());
    Ok(TokenGroundTruth { recorded: map, median })
}

/// Compute the whole report for one repo from its log, state and blobs.
pub fn compute(engine: &Heddle, repo: &RepoConfig) -> SavingsReport {
    let base = engine.base();
    let events = store::read_events(&base, &repo.id, usize::MAX);
    let snap = engine.snapshot();
    let rs = snap.repo_states.get(&repo.id).cloned().unwrap_or_default();

    let mut counted = Counted::default();
    // lease id → thread id, learned from the log so pruned threads still map.
    let mut lease_thread: BTreeMap<String, String> = BTreeMap::new();
    // Unordered pair → files, deduplicated across every logged event.
    let mut overlap: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut warned_lease_pairs: BTreeSet<(String, String)> = BTreeSet::new();
    for e in &events {
        let kind = e.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        match kind {
            "lease_declared" => {
                counted.threads_declared += 1;
                if let (Some(l), Some(t)) = (
                    e.get("lease").and_then(|v| v.as_str()),
                    e.get("thread").and_then(|v| v.as_str()),
                ) {
                    lease_thread.insert(l.to_string(), t.to_string());
                }
            }
            "weave_landed" => counted.threads_woven += 1,
            "orphaned" => counted.orphan_events += 1,
            "adopted" => counted.adoptions += 1,
            "exported" => counted.exports += 1,
            "stitch" => counted.stitches += 1,
            "toe_step" | "toe_step_cross_machine" => {
                counted.collisions_at_lease += 1;
                if let (Some(a), Some(b)) = (
                    e.get("lease_a").and_then(|v| v.as_str()),
                    e.get("lease_b").and_then(|v| v.as_str()),
                ) {
                    let (a, b) = sort_pair(a, b);
                    warned_lease_pairs.insert((a, b));
                }
            }
            "conflicting_edits_avoided" => {
                if let (Some(a), Some(b)) = (
                    e.get("thread_a").and_then(|v| v.as_str()),
                    e.get("thread_b").and_then(|v| v.as_str()),
                ) {
                    let set = overlap.entry(sort_pair(a, b)).or_default();
                    if let Some(files) = e.get("files").and_then(|v| v.as_array()) {
                        for f in files.iter().filter_map(|v| v.as_str()) {
                            set.insert(f.to_string());
                        }
                    }
                }
            }
            "weave_conflict" => counted.lands_refused_fabric_moved += 1,
            "rebased" => {
                counted.rebases += 1;
                counted.rebase_fast_forwarded_files +=
                    e.get("fast_forwarded").and_then(|v| v.as_u64()).unwrap_or(0);
                counted.rebase_conflict_files += e
                    .get("conflicts")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len() as u64)
                    .unwrap_or(0);
            }
            _ => {}
        }
    }
    counted.overlap_pairs = overlap.len() as u64;
    counted.overlap_files = overlap.values().map(|s| s.len() as u64).sum();
    counted.overlap_detail = overlap
        .iter()
        .map(|((a, b), files)| OverlapPair {
            thread_a: a.clone(),
            thread_b: b.clone(),
            files: files.iter().cloned().collect(),
        })
        .collect();
    // Warned pairs (as THREAD pairs, via the lease map) that never showed a
    // same-file edit. "yet" is load-bearing: a live pair may still collide.
    counted.warned_pairs_no_overlap_yet = warned_lease_pairs
        .iter()
        .filter_map(|(la, lb)| {
            let ta = lease_thread.get(la)?;
            let tb = lease_thread.get(lb)?;
            Some(sort_pair(ta, tb))
        })
        .collect::<BTreeSet<_>>()
        .iter()
        .filter(|pair| !overlap.contains_key(*pair))
        .count() as u64;

    // Token ground truth.
    let recorded = load_tokens(&base, &repo.id);
    let tokens = TokenGroundTruth {
        median: median(&recorded.values().copied().collect::<Vec<_>>()),
        recorded,
    };

    // Measured thread size: changed lines vs base, per isolated thread whose
    // blobs are still on disk. Deletions re-do for free, so tombstones are
    // skipped; a pruned blob just drops that file from the measurement.
    let objects = store::objects_dir(&base, &repo.id);
    let mut line_sums: Vec<u64> = Vec::new();
    for t in &rs.threads {
        let (Some(bid), Some(hid)) = (&t.base_stitch, &t.head_stitch) else { continue };
        let Some(b) = rs.stitches.iter().find(|s| &s.id == bid) else { continue };
        let Some(h) = rs.stitches.iter().find(|s| &s.id == hid) else { continue };
        let mut lines = 0u64;
        for (rel, hash) in &h.files {
            if hash == TOMBSTONE || b.files.get(rel) == Some(hash) {
                continue;
            }
            if let Ok(bytes) = store::read_blob(&objects, hash) {
                lines += bytes.split(|&c| c == b'\n').filter(|l| !l.is_empty()).count() as u64;
            }
        }
        if lines > 0 {
            line_sums.push(lines);
        }
    }
    let median_lines = median(&line_sums);

    let estimate = build_estimate(
        counted.overlap_pairs,
        tokens.median,
        tokens.recorded.len() as u64,
        median_lines,
        line_sums.len() as u64,
    );

    SavingsReport {
        repo_id: repo.id.clone(),
        repo_path: repo.path.clone(),
        counted,
        tokens,
        estimate,
    }
}

/// The estimate arithmetic, pure so a test can check every branch:
/// recorded-token median beats measured-lines × assumption beats "no basis".
pub fn build_estimate(
    overlap_pairs: u64,
    token_median: Option<u64>,
    tokens_recorded: u64,
    median_lines: Option<u64>,
    lines_threads: u64,
) -> Estimate {
    let mut est = Estimate {
        overlap_pairs,
        measured_median_stitched_lines: median_lines,
        measured_lines_threads: lines_threads,
        ..Default::default()
    };
    if let Some(m) = token_median {
        est.redo_cost_tokens = Some(m);
        est.redo_cost_basis = format!(
            "median of the {tokens_recorded} real token count(s) you recorded — measured, not assumed"
        );
    } else if let Some(lines) = median_lines {
        let cost = lines * ASSUMED_TOKENS_PER_STITCHED_LINE;
        est.redo_cost_tokens = Some(cost);
        est.assumed_tokens_per_line = Some(ASSUMED_TOKENS_PER_STITCHED_LINE);
        est.redo_cost_basis = format!(
            "{lines} median stitched lines (measured over {lines_threads} thread(s)) \
             × {ASSUMED_TOKENS_PER_STITCHED_LINE} tokens/line — the {ASSUMED_TOKENS_PER_STITCHED_LINE} \
             is a stated guess, not data; `--record-tokens` replaces it"
        );
    } else {
        est.redo_cost_basis =
            "no recorded token counts and no measurable stitched lines — no basis, so no number"
                .to_string();
    }
    if overlap_pairs > 0 {
        est.tokens_avoided = est.redo_cost_tokens.map(|c| overlap_pairs * c);
    }
    est
}

fn sort_pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

/// Median of a sample (lower middle for even sizes — never invents a value
/// that was not observed). `None` on empty input, never a fake zero.
pub fn median(xs: &[u64]) -> Option<u64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_unstable();
    Some(v[(v.len() - 1) / 2])
}

/// `12345678` → `12,345,678` — big token numbers must be readable.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl SavingsReport {
    /// The human rendering: facts, ground truth, then the labeled estimate.
    pub fn render(&self) -> String {
        let c = &self.counted;
        let mut out = String::new();
        let p = |out: &mut String, line: &str| {
            out.push_str(line);
            out.push('\n');
        };
        p(&mut out, &format!("heddle savings — {} ({})", self.repo_id, self.repo_path));
        p(&mut out, "");
        if c.prevented_nothing() {
            p(&mut out, &format!(
                "Heddle has not yet prevented anything measurable in this repo: no \
                 collisions warned, no same-file concurrent edits, no refused lands, \
                 no rebases (over {} thread(s), {} stitch(es)).",
                c.threads_declared, c.stitches
            ));
            p(&mut out,
                "If the work here was sequenced or naturally disjoint, that is the \
                 expected honest answer — isolation and scope discipline were on \
                 duty and cost nothing, but they saved nothing measurable either.");
            return out;
        }
        p(&mut out, "COUNTED — from this repo's event log (size-bounded, so every number is a floor):");
        p(&mut out, &format!(
            "  threads        {} declared · {} woven · {} orphan event(s) · {} adopted · {} exported",
            c.threads_declared, c.threads_woven, c.orphan_events, c.adoptions, c.exports
        ));
        p(&mut out, &format!("  checkpoints    {} stitch(es)", c.stitches));
        p(&mut out, &format!(
            "  collisions warned at lease time         {}   (scope overlap surfaced before work began)",
            c.collisions_at_lease
        ));
        if c.warned_pairs_no_overlap_yet > 0 {
            p(&mut out, &format!(
                "  warned pairs, no same-file edit yet     {}   (self-partitioned or naturally \
                 disjoint — heddle cannot tell which, so it prices neither)",
                c.warned_pairs_no_overlap_yet
            ));
        }
        p(&mut out, &format!(
            "  same-file concurrent edits absorbed     {} pair(s), {} file(s)",
            c.overlap_pairs, c.overlap_files
        ));
        for pair in &c.overlap_detail {
            p(&mut out, &format!(
                "      {} × {} — {}",
                pair.thread_a,
                pair.thread_b,
                pair.files.join(", ")
            ));
        }
        p(&mut out, &format!(
            "  lands refused (\"fabric moved under you\")  {}",
            c.lands_refused_fabric_moved
        ));
        p(&mut out, &format!(
            "  rebases after the fabric moved          {}   ({} file(s) fast-forwarded, {} conflict(s) kept for review)",
            c.rebases, c.rebase_fast_forwarded_files, c.rebase_conflict_files
        ));
        p(&mut out, "");
        p(&mut out, "TOKEN GROUND TRUTH");
        if self.tokens.recorded.is_empty() {
            p(&mut out, "  none recorded. When your harness knows what a thread cost, attach it:");
            p(&mut out, "      heddle savings --record-tokens <thread-id> <tokens>");
            p(&mut out, "  Recorded numbers replace the assumption in the estimate below.");
        } else {
            p(&mut out, &format!(
                "  {} thread(s) recorded, median {} tokens:",
                self.tokens.recorded.len(),
                thousands(self.tokens.median.unwrap_or(0)),
            ));
            for (t, n) in &self.tokens.recorded {
                p(&mut out, &format!("      {t}: {} tokens", thousands(*n)));
            }
        }
        p(&mut out, "");
        p(&mut out, "ESTIMATE — a model, not a measurement; every constant is printed");
        if c.overlap_pairs == 0 {
            p(&mut out, &format!(
                "  Nothing to price: {} collision(s) were warned early, but no same-file \
                 concurrent edits ever materialized. Heddle will not turn warnings into a \
                 token number — a thread that self-partitioned after a warning saved re-work \
                 this report cannot honestly measure.",
                c.collisions_at_lease
            ));
            return out;
        }
        p(&mut out,
            "  Model: each same-file concurrent-edit pair is one silent overwrite git \
             would have allowed — one thread's work on those files re-done, priced at \
             one median thread. Nothing else is priced: warnings and refused lands \
             stay facts, not tokens.");
        p(&mut out, &format!(
            "    pairs absorbed                 {}   [counted]",
            c.overlap_pairs
        ));
        match self.estimate.redo_cost_tokens {
            Some(cost) => {
                let tag = if self.estimate.assumed_tokens_per_line.is_some() {
                    "ASSUMED"
                } else {
                    "measured"
                };
                p(&mut out, &format!(
                    "    re-do cost per pair            {} tokens   [{}: {}]",
                    thousands(cost),
                    tag,
                    self.estimate.redo_cost_basis
                ));
                p(&mut out, &format!(
                    "    ≈ tokens of re-work avoided    {} × {} = {}",
                    c.overlap_pairs,
                    thousands(cost),
                    thousands(self.estimate.tokens_avoided.unwrap_or(0))
                ));
            }
            None => {
                p(&mut out, &format!(
                    "    re-do cost per pair            unknown — {}",
                    self.estimate.redo_cost_basis
                ));
                p(&mut out,
                    "    No number is printed without a basis. The counted pairs above are \
                     the honest answer; add --record-tokens data to price them.");
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "heddle-savings-{tag}-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mk scratch");
        p
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A real git repo with one commit, so leases isolate under Auto.
    fn git_rig(tag: &str) -> (Heddle, PathBuf, RepoConfig) {
        let base = scratch(&format!("{tag}-data"));
        let repo_dir = scratch(&format!("{tag}-repo"));
        for (rel, body) in [
            ("src/main.rs", "fn main() {}\n"),
            ("src/util.rs", "pub fn u() {}\n"),
        ] {
            let p = repo_dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
        }
        git_ok(&repo_dir, &["init", "-q"]);
        git_ok(&repo_dir, &["config", "user.email", "heddle@test"]);
        git_ok(&repo_dir, &["config", "user.name", "heddle test"]);
        git_ok(&repo_dir, &["add", "-A"]);
        git_ok(&repo_dir, &["commit", "-q", "-m", "base"]);
        let heddle = Heddle::at(base);
        let repo = heddle
            .register_repo(repo_dir.to_str().unwrap(), Some("true".into()), false)
            .expect("register");
        (heddle, repo_dir, repo)
    }

    #[test]
    fn a_repo_with_nothing_prevented_says_exactly_that() {
        let base = scratch("empty-data");
        let repo_dir = scratch("empty-repo");
        std::fs::write(repo_dir.join("a.txt"), "hi\n").unwrap();
        let heddle = Heddle::at(base);
        let repo = heddle
            .register_repo(repo_dir.to_str().unwrap(), Some("true".into()), false)
            .expect("register");
        let report = compute(&heddle, &repo);
        assert!(report.counted.prevented_nothing());
        let text = report.render();
        assert!(
            text.contains("has not yet prevented anything measurable"),
            "{text}"
        );
        assert!(text.contains("honest answer"), "{text}");
        // No invented numbers anywhere near it.
        assert!(!text.contains("ESTIMATE"), "{text}");
        assert_eq!(report.estimate.tokens_avoided, None);
    }

    #[test]
    fn the_full_collision_story_counts_end_to_end() {
        let (heddle, _repo_dir, repo) = git_rig("story");
        let da = heddle
            .declare_lease(&repo.id, "alice", "restyle main", vec!["src/**".into()], vec![], None)
            .expect("lease a");
        let db = heddle
            .declare_lease(&repo.id, "bob", "rework main too", vec!["src/**".into()], vec![], None)
            .expect("lease b");
        assert!(!db.toe_steps.is_empty(), "collision warned at lease time");
        let wa = PathBuf::from(da.thread.worktree.as_ref().unwrap());
        let wb = PathBuf::from(db.thread.worktree.as_ref().unwrap());
        // Both edit the SAME file while both threads are live.
        std::fs::write(wa.join("src/main.rs"), "fn main() { /* alice */ }\n").unwrap();
        std::fs::write(wb.join("src/main.rs"), "fn main() { /* bob */ }\n").unwrap();
        heddle.stitch(&da.lease.id).expect("stitch a");
        heddle.stitch(&db.lease.id).expect("stitch b");
        // The absorbed clobber is on the state, pair sorted, file named.
        let snap = heddle.snapshot();
        let oe = &snap.repo_states[&repo.id].overlap_edits;
        assert_eq!(oe.len(), 1, "{oe:?}");
        assert!(oe[0].thread_a < oe[0].thread_b);
        assert_eq!(oe[0].files, vec!["src/main.rs".to_string()]);
        // Re-stitching the same collision records NOTHING new.
        std::fs::write(wb.join("src/main.rs"), "fn main() { /* bob v2 */ }\n").unwrap();
        heddle.stitch(&db.lease.id).expect("stitch b again");
        assert_eq!(
            heddle.snapshot().repo_states[&repo.id].overlap_edits[0].files.len(),
            1,
            "deduplicated per pair+file"
        );
        // Alice lands; Bob is refused (fabric moved) and rebases.
        let pa = heddle.propose(&da.thread.id).expect("propose a");
        heddle.land_weave(&pa.weave.id).expect("land a");
        let pb = heddle.propose(&db.thread.id).expect("propose b");
        assert!(pb.green);
        assert!(heddle.land_weave(&pb.weave.id).is_err(), "fabric moved");
        heddle.rebase_thread(&db.thread.id).expect("rebase b");

        let report = compute(&heddle, &repo);
        let c = &report.counted;
        assert!(c.collisions_at_lease >= 1, "{c:?}");
        assert_eq!(c.overlap_pairs, 1, "{c:?}");
        assert_eq!(c.overlap_files, 1, "{c:?}");
        assert_eq!(c.overlap_detail[0].files, vec!["src/main.rs".to_string()]);
        assert_eq!(c.lands_refused_fabric_moved, 1, "{c:?}");
        assert_eq!(c.rebases, 1, "{c:?}");
        assert_eq!(c.threads_declared, 2);
        assert_eq!(c.threads_woven, 1);
        assert!(!c.prevented_nothing());

        // No token data yet: the estimate rests on measured lines × the
        // STATED assumption, and says so.
        assert!(report.tokens.recorded.is_empty());
        assert_eq!(
            report.estimate.assumed_tokens_per_line,
            Some(ASSUMED_TOKENS_PER_STITCHED_LINE)
        );
        assert!(report.estimate.measured_median_stitched_lines.is_some());
        assert!(report.estimate.tokens_avoided.is_some());
        let text = report.render();
        assert!(text.contains("ASSUMED"), "{text}");
        assert!(text.contains("guess, not data"), "{text}");
        assert!(text.contains("floor"), "log-rotation honesty: {text}");

        // Ground truth recorded → the assumption leaves the model.
        savings_record(&heddle, &repo, &da.thread.id, 50_000);
        savings_record(&heddle, &repo, &db.thread.id, 30_000);
        let report = compute(&heddle, &repo);
        assert_eq!(report.tokens.median, Some(30_000), "lower middle of {{30k, 50k}}");
        assert_eq!(report.estimate.redo_cost_tokens, Some(30_000));
        assert_eq!(report.estimate.tokens_avoided, Some(30_000));
        assert!(report.estimate.assumed_tokens_per_line.is_none());
        let text = report.render();
        assert!(text.contains("measured"), "{text}");
        assert!(text.contains("1 × 30,000 = 30,000"), "arithmetic shown: {text}");

        // A typo'd thread id cannot mint ground truth.
        assert!(record_tokens(&heddle, &repo.id, "thread-nope", 1).is_err());
    }

    fn savings_record(heddle: &Heddle, repo: &RepoConfig, thread: &str, n: u64) {
        record_tokens(heddle, &repo.id, thread, n).expect("record tokens");
    }

    #[test]
    fn warned_but_disjoint_pairs_are_counted_and_never_priced() {
        let (heddle, _repo_dir, repo) = git_rig("disjoint");
        let da = heddle
            .declare_lease(&repo.id, "alice", "main work", vec!["src/**".into()], vec![], None)
            .expect("lease a");
        let db = heddle
            .declare_lease(&repo.id, "bob", "util work", vec!["src/**".into()], vec![], None)
            .expect("lease b");
        assert!(!db.toe_steps.is_empty(), "same glob → warned");
        // …but they self-partition: different files.
        let wa = PathBuf::from(da.thread.worktree.as_ref().unwrap());
        let wb = PathBuf::from(db.thread.worktree.as_ref().unwrap());
        std::fs::write(wa.join("src/main.rs"), "fn main() { /* a */ }\n").unwrap();
        std::fs::write(wb.join("src/util.rs"), "pub fn u() { /* b */ }\n").unwrap();
        heddle.stitch(&da.lease.id).expect("stitch a");
        heddle.stitch(&db.lease.id).expect("stitch b");
        let report = compute(&heddle, &repo);
        let c = &report.counted;
        assert!(c.collisions_at_lease >= 1);
        assert_eq!(c.overlap_pairs, 0, "no same-file edit ever happened");
        assert_eq!(c.warned_pairs_no_overlap_yet, 1, "{c:?}");
        let text = report.render();
        assert!(text.contains("Nothing to price"), "{text}");
        assert!(
            text.contains("cannot honestly measure"),
            "warnings are never monetized: {text}"
        );
        assert_eq!(report.estimate.tokens_avoided, None);
    }

    #[test]
    fn median_is_an_observed_value_never_an_invented_one() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[7]), Some(7));
        assert_eq!(median(&[1, 100]), Some(1), "even size takes the lower middle");
        assert_eq!(median(&[3, 1, 2]), Some(2));
        assert_eq!(median(&[10, 40, 20, 30]), Some(20));
    }

    #[test]
    fn thousands_groups_from_the_right() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
    }

    #[test]
    fn estimate_prefers_recorded_tokens_over_the_lines_assumption() {
        // Recorded tokens exist: measured basis, no assumption in play.
        let e = build_estimate(2, Some(40_000), 3, Some(500), 4);
        assert_eq!(e.redo_cost_tokens, Some(40_000));
        assert_eq!(e.tokens_avoided, Some(80_000));
        assert!(e.assumed_tokens_per_line.is_none());
        assert!(e.redo_cost_basis.contains("measured"), "{}", e.redo_cost_basis);

        // No tokens: lines × the stated assumption, and the assumption is
        // carried on the estimate so renderers can label it.
        let e = build_estimate(3, None, 0, Some(400), 5);
        assert_eq!(
            e.redo_cost_tokens,
            Some(400 * ASSUMED_TOKENS_PER_STITCHED_LINE)
        );
        assert_eq!(
            e.tokens_avoided,
            Some(3 * 400 * ASSUMED_TOKENS_PER_STITCHED_LINE)
        );
        assert_eq!(e.assumed_tokens_per_line, Some(ASSUMED_TOKENS_PER_STITCHED_LINE));
        assert!(e.redo_cost_basis.contains("guess"), "{}", e.redo_cost_basis);

        // No basis at all: no number, and the basis says why.
        let e = build_estimate(1, None, 0, None, 0);
        assert_eq!(e.redo_cost_tokens, None);
        assert_eq!(e.tokens_avoided, None);
        assert!(e.redo_cost_basis.contains("no basis"), "{}", e.redo_cost_basis);
    }

    #[test]
    fn zero_pairs_means_no_tokens_avoided_whatever_the_cost_basis_says() {
        let e = build_estimate(0, Some(50_000), 2, Some(100), 1);
        assert_eq!(e.tokens_avoided, None, "nothing absorbed → nothing priced");
    }
}
