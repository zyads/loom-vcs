// Loom — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! **Loom** — version control for many hands moving at once (see
//! `docs/DESIGN.md` for the full design).
//!
//! Git detects collisions at merge time, hours after the toes were stepped
//! on; leaves crashed agents' work as unowned dirty worktrees; and makes
//! "green main" a snapshot, not an invariant. Loom is the high-frequency
//! collaboration layer that sits above git: agents declare **intent leases**
//! before touching files, checkpoint **stitches** every few seconds, and land
//! on the shared **fabric** only through a **weave gate** that verifies green
//! first. Crashed work becomes an adoptable **orphan**, never a mess.
//!
//! **v1 is single-machine**: multiple local agent sessions collaborating
//! through one shared data directory. Peer-to-peer gossip is out of scope —
//! but every object here carries a stable id and serializes cleanly so the
//! same model can gossip later over a peer transport.
//!
//! **The trust boundary, stated plainly:**
//!
//! * **A lease is knowledge, not a lock.** Declaring a scope that overlaps a
//!   live lease SUCCEEDS — the collision is surfaced the moment it is cheap,
//!   as a recorded `toe_step` warning carrying both goals and a suggested
//!   split. Nothing in Loom ever blocks an agent from working.
//! * **A stitch only READS.** Capturing a stitch walks the leased scope and
//!   snapshots file contents into a content-addressed store under the data
//!   dir. It never writes into the repo.
//! * **The weave gate verifies in a scratch copy, never in the real tree.**
//!   `propose` copies the repo to a scratch dir, overlays the thread's head
//!   stitch, and runs the repo's verify command there. Red never lands — the
//!   failure is recorded and the thread stays active.
//! * **Applying a green weave to the real working tree is an ACTION.** It
//!   requires an explicit human yes, expressed through the
//!   [`consent::WeaveConsent`] trait — an interactive terminal prompt in the
//!   standalone binary, a parked approval when embedded in a host with an
//!   approvals queue. Until that yes, the honest answer is *"verified green;
//!   nothing was applied."* A refusal leaves the working tree untouched with
//!   the reason noted on the thread.
//! * **The git bridge never pushes.** When a repo was registered with
//!   `git_bridge: true`, a landed weave becomes one local commit on the
//!   current branch, message composed from the lease goal + criteria +
//!   verify result. Per-thread draft-branch export is future work.
//!
//! Storage is "boring on purpose": a JSON state file plus an append-only
//! JSONL event log per repo under `<data dir>/<repo_id>/` (default `~/.loom`,
//! overridable via the `LOOM_DATA` env var or [`Loom::at`]), 0o600, bounded,
//! corrupt-line tolerant, and a content-addressed blob store (whole files
//! keyed by sha256; rolling-hash chunking is future work).
//!
//! TODO(federation): gossip logs over a peer transport; sign every object
//! with a per-machine key (a `sig` field slots in via serde default without
//! a format break); rotate the shuttle token for fabric advancement among
//! live peers. None of that is built here — v1 has exactly one peer: this
//! machine.

pub mod bridge;
pub mod consent;
pub mod lease;
pub mod solo;
pub mod store;
pub mod weave;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Bounds — hostile or runaway callers cannot bloat the store
// ---------------------------------------------------------------------------

/// Lease TTL when the caller does not pick one: 30 minutes. Long enough that
/// a CLI or agent session heart-beating every 5 minutes has six chances
/// before the lease orphans; callers can still pick their own within the
/// clamp below.
pub const DEFAULT_TTL_MS: u64 = 30 * 60 * 1000;
/// TTL clamp: a lease lives at least 10 seconds and at most 24 hours.
pub const MIN_TTL_MS: u64 = 10_000;
pub const MAX_TTL_MS: u64 = 24 * 3600 * 1000;

/// Verify command when the repo registration does not pick one.
pub const DEFAULT_VERIFY_CMD: &str = "cargo check";

/// Caps on stored text and collection sizes.
pub const MAX_GOAL_CHARS: usize = 300;
pub const MAX_CRITERIA: usize = 20;
pub const MAX_SCOPE_PATTERNS: usize = 32;
pub const MAX_PATTERN_CHARS: usize = 300;
pub const MAX_REPOS: usize = 20;
pub const MAX_THREADS_PER_REPO: usize = 200;
pub const MAX_STITCHES_PER_THREAD: usize = 200;
pub const MAX_WEAVES_PER_REPO: usize = 500;
pub const MAX_TOE_STEPS: usize = 100;

// ---------------------------------------------------------------------------
// Object model — all objects carry ids and serialize cleanly, so the same
// shapes can gossip between peers later. TODO(federation): a `sig` field
// per object, serde-defaulted, once a per-machine key signs them.
// ---------------------------------------------------------------------------

/// A registered loom repo: a directory the operator pointed Loom at.
/// `id` is a stable hash of the canonical path, so re-registering the same
/// directory is idempotent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RepoConfig {
    pub id: String,
    /// Canonicalized absolute path of the repo directory.
    pub path: String,
    /// Shell command the weave gate runs in the scratch worktree.
    pub verify_cmd: String,
    /// When true, a landed weave becomes one local git commit (never a push).
    #[serde(default)]
    pub git_bridge: bool,
    pub registered_ms: u64,
}

/// An intent lease: "I am about to touch these paths, for this goal."
/// Not a lock — knowledge. Overlap warns (a [`ToeStep`]); it never blocks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    pub id: String,
    pub thread_id: String,
    /// Path globs relative to the repo root (`src/parse/**`, `Cargo.toml`).
    pub scope: Vec<String>,
    pub goal: String,
    /// Acceptance criteria: short sentences a reviewer (human or agent)
    /// can check off.
    #[serde(default)]
    pub criteria: Vec<String>,
    /// Who holds the lease — v1: a free-form holder name (a user, an agent
    /// session). TODO(federation): a peer key.
    pub holder: String,
    pub ttl_ms: u64,
    pub last_heartbeat_ms: u64,
}

impl Lease {
    pub fn expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_heartbeat_ms) > self.ttl_ms
    }

    /// Milliseconds of life left before the lease orphans (0 when expired).
    /// Surfaced as `expires_in_ms` on every API response that carries a
    /// lease, so CLI/MCP callers can self-manage heartbeats.
    pub fn expires_in_ms(&self, now_ms: u64) -> u64 {
        self.ttl_ms
            .saturating_sub(now_ms.saturating_sub(self.last_heartbeat_ms))
    }
}

/// A micro-snapshot of the leased scope: `files` maps repo-relative path →
/// content hash in the blob store. Unchanged files dedup for free (same
/// hash, same blob); an unchanged *manifest* creates no new stitch at all.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stitch {
    pub id: String,
    pub thread_id: String,
    /// Previous stitch on this thread, if any.
    pub parent: Option<String>,
    pub files: BTreeMap<String, String>,
    pub ts_ms: u64,
}

/// Thread lifecycle. `Adopted` has Active semantics (stitch/propose work);
/// it exists as its own status so surfaces can say "this was picked up after
/// its first holder died".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadStatus {
    Active,
    Proposed,
    Woven,
    Orphaned,
    Adopted,
}

impl ThreadStatus {
    /// Statuses in which the thread's lease is live and work may continue.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Active | Self::Adopted | Self::Proposed)
    }
}

/// One agent's work-line: a chain of stitches under one lease and goal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Thread {
    pub id: String,
    pub repo_id: String,
    pub goal: String,
    pub head_stitch: Option<String>,
    pub lease_id: Option<String>,
    pub status: ThreadStatus,
    /// Honest margin note ("verify red: …", "denied by operator", "woven").
    /// Not in the doc's field table; added so deny/red outcomes are visible
    /// on the object itself instead of only in the log.
    #[serde(default)]
    pub note: String,
    /// The parked approval currently gating this thread's green weave, set by
    /// an embedding host's consent layer right after it parks one (the
    /// standalone binary's consent is synchronous and never sets it). Hosts
    /// whose approvals live in memory only — where a restart or timeout kills
    /// them — leave a Proposed thread stuck when its `approval_id` is no
    /// longer pending; [`Loom::reconcile_parked`] returns it to Active.
    /// `None` on a Proposed thread means no approval is waiting (denied /
    /// withdrawn / mid-gate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifyResult {
    Green,
    Red,
}

/// What the weave gate observed: the command it ran and how it ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerifyOutcome {
    pub cmd: String,
    pub result: VerifyResult,
    /// Bounded tail of combined stdout+stderr — enough to see why it's red.
    pub log_tail: String,
}

/// A recorded pass through the weave gate. A green weave is *evidence*, not
/// an application: it lands on the fabric (and the real tree) only after the
/// human yes is given, and only while `fabric_parent` still equals the
/// fabric tip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Weave {
    pub id: String,
    pub thread_id: String,
    /// Fabric tip at record time — the state this verify was measured
    /// against. `None` means the fabric had no tip yet.
    pub fabric_parent: Option<String>,
    pub verify: VerifyOutcome,
    pub ts_ms: u64,
}

/// The shared line. `tip` advances only in [`Loom::land_weave`], whose
/// precondition is a green verify plus an explicit human yes — green by
/// construction, honestly scoped: v1 verifies whole-repo state, not slices.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Fabric {
    #[serde(default)]
    pub repo_id: String,
    #[serde(default)]
    pub tip: Option<String>,
    /// Landed weave ids, oldest first.
    #[serde(default)]
    pub history: Vec<String>,
}

/// A scope collision noticed at declaration time — the moment coordination
/// is still cheap. Warn-only; recorded and returned, never blocking.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToeStep {
    pub id: String,
    pub ts_ms: u64,
    pub lease_a: String,
    pub lease_b: String,
    pub goal_a: String,
    pub goal_b: String,
    pub pattern_a: String,
    pub pattern_b: String,
    /// Human-readable suggestions for a non-overlapping split.
    pub suggested_split: Vec<String>,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Everything Loom knows about one repo. Persisted as
/// `<data dir>/<repo_id>/state.json`; every mutation also appends to the
/// repo's `log.jsonl`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RepoState {
    #[serde(default)]
    pub threads: Vec<Thread>,
    #[serde(default)]
    pub leases: Vec<Lease>,
    #[serde(default)]
    pub stitches: Vec<Stitch>,
    #[serde(default)]
    pub weaves: Vec<Weave>,
    #[serde(default)]
    pub toe_steps: Vec<ToeStep>,
    #[serde(default)]
    pub fabric: Fabric,
    #[serde(default)]
    pub seq: u64,
}

/// The whole in-memory picture: the repo registry plus per-repo state.
#[derive(Clone, Debug, Default)]
pub struct LoomState {
    pub repos: Vec<RepoConfig>,
    pub repo_states: std::collections::HashMap<String, RepoState>,
}

/// The engine. One per process (see [`store`]); tests and embedders build
/// isolated ones with [`Loom::at`]. The std Mutex guards are never held
/// across an await — long work (file walks, verify runs) happens between
/// locked phases.
pub struct Loom {
    base_override: Option<PathBuf>,
    inner: Mutex<Option<LoomState>>,
}

/// The process-wide engine, rooted at the default data dir (`LOOM_DATA` env
/// var when set, else `~/.loom` — resolved fresh on every touch).
pub fn store() -> &'static Loom {
    static S: std::sync::OnceLock<Loom> = std::sync::OnceLock::new();
    S.get_or_init(|| Loom {
        base_override: None,
        inner: Mutex::new(None),
    })
}

/// The default storage root: `$LOOM_DATA` when set and non-empty, else
/// `~/.loom` (via `$HOME`; falls back to `.loom` in the current directory
/// when even `HOME` is unset).
pub fn default_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("LOOM_DATA") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    match std::env::var("HOME") {
        Ok(home) if !home.trim().is_empty() => PathBuf::from(home).join(".loom"),
        _ => PathBuf::from(".loom"),
    }
}

/// What `declare_lease` hands back: the lease, its thread, and any toe-step
/// warnings — declaration succeeded either way.
#[derive(Clone, Debug)]
pub struct DeclareOutcome {
    pub lease: Lease,
    pub thread: Thread,
    pub toe_steps: Vec<ToeStep>,
}

/// What `stitch` hands back. `unchanged` means the manifest was identical to
/// the parent stitch, so no new stitch was created. `lease` is the lease as
/// it stood at capture time, so callers can see how long they have left
/// (`expires_in_ms` on the API) and self-manage heartbeats.
#[derive(Clone, Debug)]
pub struct StitchOutcome {
    pub stitch: Stitch,
    pub unchanged: bool,
    pub skipped: Vec<String>,
    pub lease: Lease,
}

/// What `propose` hands back after the gate ran.
#[derive(Clone, Debug)]
pub struct ProposeOutcome {
    pub weave: Weave,
    pub repo: RepoConfig,
    pub thread: Thread,
    pub green: bool,
}

/// What `land_weave` hands back so the caller can run the git bridge and
/// speak honestly about what changed.
#[derive(Clone, Debug)]
pub struct LandOutcome {
    pub repo: RepoConfig,
    pub thread: Thread,
    pub weave: Weave,
    pub criteria: Vec<String>,
    pub files_applied: usize,
}

impl Loom {
    /// An isolated engine rooted at `base` — tests, and embedders that
    /// manage their own storage root; everyone else uses [`store`].
    pub fn at(base: PathBuf) -> Self {
        Loom {
            base_override: Some(base),
            inner: Mutex::new(None),
        }
    }

    /// Storage root: the override when constructed with [`Loom::at`], else
    /// [`default_data_dir`] resolved fresh each call (so `LOOM_DATA` can be
    /// pointed elsewhere by a test rig between calls).
    pub fn base(&self) -> PathBuf {
        self.base_override.clone().unwrap_or_else(default_data_dir)
    }

    /// Run `f` against the live state, loading on first touch and persisting
    /// whatever it changed. Guards never cross an await; anything slow
    /// (file walks, verify commands) runs OUTSIDE this closure.
    fn with<T>(&self, f: impl FnOnce(&mut LoomState, &PathBuf) -> T) -> T {
        let base = self.base();
        let mut guard = self.inner.lock().expect("loom store lock poisoned");
        if guard.is_none() {
            *guard = Some(store::load(&base));
        }
        let state = guard.as_mut().expect("initialised above");
        let out = f(state, &base);
        prune(state);
        store::persist(&base, state);
        out
    }

    /// Drop cached state (tests, and after the path env changes).
    pub fn reset_cache(&self) {
        *self.inner.lock().expect("loom store lock poisoned") = None;
    }

    // -- repo registration --------------------------------------------------

    /// Register a directory as a loom repo. Idempotent per canonical path:
    /// re-registering updates `verify_cmd`/`git_bridge` in place and keeps
    /// the same `repo_id` (a stable hash of the canonical path).
    pub fn register_repo(
        &self,
        path: &str,
        verify_cmd: Option<String>,
        git_bridge: bool,
    ) -> Result<RepoConfig, String> {
        let canon = std::fs::canonicalize(path.trim())
            .map_err(|e| format!("cannot canonicalize '{}': {e}", path.trim()))?;
        if !canon.is_dir() {
            return Err(format!("{} is not a directory", canon.display()));
        }
        let canon_str = canon.to_string_lossy().to_string();
        let id = repo_id_for(&canon_str);
        let cmd = verify_cmd
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| DEFAULT_VERIFY_CMD.to_string());
        self.with(|s, base| {
            if let Some(existing) = s.repos.iter_mut().find(|r| r.id == id) {
                existing.verify_cmd = cmd.clone();
                existing.git_bridge = git_bridge;
                return Ok(existing.clone());
            }
            if s.repos.len() >= MAX_REPOS {
                return Err(format!("at most {MAX_REPOS} repos can be registered"));
            }
            let repo = RepoConfig {
                id: id.clone(),
                path: canon_str.clone(),
                verify_cmd: cmd.clone(),
                git_bridge,
                registered_ms: now_ms(),
            };
            s.repos.push(repo.clone());
            let mut rs = RepoState::default();
            rs.fabric.repo_id = id.clone();
            s.repo_states.insert(id.clone(), rs);
            store::append_event(
                base,
                &id,
                &serde_json::json!({
                    "ts_ms": now_ms(), "kind": "repo_registered",
                    "path": canon_str, "verify_cmd": cmd, "git_bridge": git_bridge,
                }),
            );
            Ok(repo)
        })
    }

    /// The registered repo containing `path`: the repo whose canonical path
    /// is `path` itself or an ancestor of it (longest match wins, so nested
    /// registrations resolve to the innermost repo). How the CLI and MCP
    /// server turn "the directory I'm in" into a repo id.
    pub fn repo_containing(&self, path: &str) -> Option<RepoConfig> {
        let canon = std::fs::canonicalize(path).ok()?;
        let canon = canon.to_string_lossy().to_string();
        self.with(|s, _| {
            s.repos
                .iter()
                .filter(|r| {
                    canon == r.path || canon.starts_with(&format!("{}/", r.path))
                })
                .max_by_key(|r| r.path.len())
                .cloned()
        })
    }

    // -- leases -------------------------------------------------------------

    /// Declare an intent lease (creates its thread). Scope globs are
    /// validated; overlap with live leases is detected and returned as
    /// toe-step warnings — the declaration still succeeds.
    pub fn declare_lease(
        &self,
        repo_id: &str,
        holder: &str,
        goal: &str,
        scope: Vec<String>,
        criteria: Vec<String>,
        ttl_ms: Option<u64>,
    ) -> Result<DeclareOutcome, String> {
        let goal = cap(goal, MAX_GOAL_CHARS);
        if goal.is_empty() {
            return Err("a lease needs a one-sentence goal".into());
        }
        let scope = lease::validate_scope(&scope)?;
        let criteria: Vec<String> = criteria
            .into_iter()
            .take(MAX_CRITERIA)
            .map(|c| cap(&c, MAX_GOAL_CHARS))
            .filter(|c| !c.is_empty())
            .collect();
        let ttl = ttl_ms
            .unwrap_or(DEFAULT_TTL_MS)
            .clamp(MIN_TTL_MS, MAX_TTL_MS);
        let holder = cap(holder, MAX_GOAL_CHARS);
        let now = now_ms();
        self.with(|s, base| {
            if !s.repos.iter().any(|r| r.id == repo_id) {
                return Err(format!("no registered repo with id {repo_id}"));
            }
            reconcile_repo(s, repo_id, now, base);
            let rs = s.repo_states.entry(repo_id.to_string()).or_default();
            rs.seq += 1;
            let thread_id = format!("thread-{now}-{}", rs.seq);
            let lease_id = format!("lease-{now}-{}", rs.seq);
            let lease = Lease {
                id: lease_id.clone(),
                thread_id: thread_id.clone(),
                scope: scope.clone(),
                goal: goal.clone(),
                criteria,
                holder,
                ttl_ms: ttl,
                last_heartbeat_ms: now,
            };
            let thread = Thread {
                id: thread_id.clone(),
                repo_id: repo_id.to_string(),
                goal: goal.clone(),
                head_stitch: None,
                lease_id: Some(lease_id.clone()),
                status: ThreadStatus::Active,
                note: String::new(),
                approval_id: None,
            };
            // Overlap check against every other live lease, BEFORE inserting
            // ours, so we never compare a lease against itself.
            let toe_steps = lease::detect_toe_steps(rs, &lease, now);
            for t in &toe_steps {
                rs.toe_steps.push(t.clone());
                store::append_event(
                    base,
                    repo_id,
                    &serde_json::json!({
                        "ts_ms": now, "kind": "toe_step",
                        "lease_a": t.lease_a, "lease_b": t.lease_b,
                        "pattern_a": t.pattern_a, "pattern_b": t.pattern_b,
                    }),
                );
            }
            rs.leases.push(lease.clone());
            rs.threads.push(thread.clone());
            store::append_event(
                base,
                repo_id,
                &serde_json::json!({
                    "ts_ms": now, "kind": "lease_declared", "lease": lease.id,
                    "thread": thread.id, "goal": goal, "scope": scope,
                }),
            );
            Ok(DeclareOutcome {
                lease,
                thread,
                toe_steps,
            })
        })
    }

    /// Renew a lease. Refused once the thread is orphaned — adopt instead.
    /// Heartbeats are deliberately NOT logged (they fire every few seconds).
    pub fn heartbeat(&self, lease_id: &str) -> Result<Lease, String> {
        let now = now_ms();
        self.with(|s, base| {
            let repo_id = find_repo_of_lease(s, lease_id)
                .ok_or_else(|| format!("no lease with id {lease_id}"))?;
            reconcile_repo(s, &repo_id, now, base);
            let rs = s.repo_states.get_mut(&repo_id).expect("repo state exists");
            let thread_status = rs
                .threads
                .iter()
                .find(|t| t.lease_id.as_deref() == Some(lease_id))
                .map(|t| t.status);
            if thread_status == Some(ThreadStatus::Orphaned) {
                return Err("this thread is orphaned; adopt it instead of heart-beating".to_string());
            }
            let lease = rs
                .leases
                .iter_mut()
                .find(|l| l.id == lease_id)
                .ok_or_else(|| format!("no lease with id {lease_id}"))?;
            lease.last_heartbeat_ms = now;
            Ok(lease.clone())
        })
    }

    /// Release a lease early (engine-level; no HTTP route in v1 — a lease
    /// also releases implicitly when its weave lands, and expires by TTL).
    pub fn release_lease(&self, lease_id: &str) -> Result<(), String> {
        self.with(|s, base| {
            let repo_id = find_repo_of_lease(s, lease_id)
                .ok_or_else(|| format!("no lease with id {lease_id}"))?;
            let rs = s.repo_states.get_mut(&repo_id).expect("repo state exists");
            rs.leases.retain(|l| l.id != lease_id);
            for t in rs.threads.iter_mut() {
                if t.lease_id.as_deref() == Some(lease_id) && t.status.is_live() {
                    t.lease_id = None;
                    t.status = ThreadStatus::Orphaned;
                    t.note = "lease released by holder".into();
                }
            }
            store::append_event(
                base,
                &repo_id,
                &serde_json::json!({"ts_ms": now_ms(), "kind": "lease_released", "lease": lease_id}),
            );
            Ok(())
        })
    }

    // -- stitches -----------------------------------------------------------

    /// Capture a stitch: snapshot the current content of files matching the
    /// lease scope under the repo root. The server reads the files itself —
    /// callers never upload content. Blocking (file walk + hashing); call
    /// from `spawn_blocking` on async paths.
    pub fn stitch(&self, lease_id: &str) -> Result<StitchOutcome, String> {
        let now = now_ms();
        // Phase 1 (locked): resolve lease → repo path + scope + parent.
        let (repo, lease, thread_id, parent) = self.with(|s, base| {
            let repo_id = find_repo_of_lease(s, lease_id)
                .ok_or_else(|| format!("no lease with id {lease_id}"))?;
            reconcile_repo(s, &repo_id, now, base);
            let rs = s.repo_states.get(&repo_id).expect("repo state exists");
            let lease = rs
                .leases
                .iter()
                .find(|l| l.id == lease_id)
                .ok_or_else(|| format!("no lease with id {lease_id}"))?;
            let thread = rs
                .threads
                .iter()
                .find(|t| t.id == lease.thread_id)
                .ok_or_else(|| format!("lease {lease_id} has no thread"))?;
            if thread.status == ThreadStatus::Orphaned {
                return Err("thread is orphaned; adopt it before stitching".to_string());
            }
            let parent = thread
                .head_stitch
                .as_ref()
                .and_then(|id| rs.stitches.iter().find(|st| st.id == *id))
                .cloned();
            let repo = s
                .repos
                .iter()
                .find(|r| r.id == repo_id)
                .cloned()
                .ok_or_else(|| format!("repo {repo_id} vanished from the registry"))?;
            Ok((repo, lease.clone(), lease.thread_id.clone(), parent))
        })?;
        // Phase 2 (unlocked): walk + hash + write blobs.
        let objects = store::objects_dir(&self.base(), &repo.id);
        let captured =
            weave::capture_scope(std::path::Path::new(&repo.path), &lease.scope, &objects)?;
        // Phase 3 (locked): record the stitch (or report "unchanged").
        self.with(|s, base| {
            let rs = s.repo_states.get_mut(&repo.id).expect("repo state exists");
            if let Some(p) = &parent {
                if p.files == captured.manifest {
                    return Ok(StitchOutcome {
                        stitch: p.clone(),
                        unchanged: true,
                        skipped: captured.skipped,
                        lease: lease.clone(),
                    });
                }
            }
            rs.seq += 1;
            let stitch = Stitch {
                id: format!("stitch-{now}-{}", rs.seq),
                thread_id: thread_id.clone(),
                parent: parent.as_ref().map(|p| p.id.clone()),
                files: captured.manifest.clone(),
                ts_ms: now,
            };
            rs.stitches.push(stitch.clone());
            if let Some(t) = rs.threads.iter_mut().find(|t| t.id == thread_id) {
                t.head_stitch = Some(stitch.id.clone());
            }
            store::append_event(
                base,
                &repo.id,
                &serde_json::json!({
                    "ts_ms": now, "kind": "stitch", "stitch": stitch.id,
                    "thread": thread_id, "files": stitch.files.len(),
                }),
            );
            Ok(StitchOutcome {
                stitch,
                unchanged: false,
                skipped: captured.skipped,
                lease: lease.clone(),
            })
        })
    }

    // -- orphans ------------------------------------------------------------

    /// Lazily expire leases and orphan their threads
    /// (reconcile-on-read — no bespoke timer).
    pub fn reconcile(&self) {
        let now = now_ms();
        self.with(|s, base| {
            let ids: Vec<String> = s.repos.iter().map(|r| r.id.clone()).collect();
            for id in ids {
                reconcile_repo(s, &id, now, base);
            }
        });
    }

    /// Adopt an orphaned thread: the new holder takes over the SAME lease
    /// (fresh TTL and heartbeat), so goal, criteria and scope are preserved
    /// exactly. Thread → Adopted, which has Active semantics.
    pub fn adopt(&self, thread_id: &str, holder: &str) -> Result<(Thread, Lease), String> {
        let now = now_ms();
        let holder = cap(holder, MAX_GOAL_CHARS);
        self.with(|s, base| {
            let repo_id = find_repo_of_thread(s, thread_id)
                .ok_or_else(|| format!("no thread with id {thread_id}"))?;
            reconcile_repo(s, &repo_id, now, base);
            let rs = s.repo_states.get_mut(&repo_id).expect("repo state exists");
            let thread = rs
                .threads
                .iter_mut()
                .find(|t| t.id == thread_id)
                .ok_or_else(|| format!("no thread with id {thread_id}"))?;
            if thread.status != ThreadStatus::Orphaned {
                return Err(format!(
                    "thread {} is {:?}, not orphaned — only orphans can be adopted",
                    thread_id, thread.status
                ));
            }
            let lease_id = thread
                .lease_id
                .clone()
                .ok_or_else(|| format!("orphan {thread_id} lost its lease record"))?;
            thread.status = ThreadStatus::Adopted;
            thread.note = format!("adopted by {holder}");
            let thread = thread.clone();
            let lease = rs
                .leases
                .iter_mut()
                .find(|l| l.id == lease_id)
                .ok_or_else(|| format!("orphan {thread_id} lost lease {lease_id}"))?;
            lease.holder = holder.clone();
            lease.last_heartbeat_ms = now;
            let lease = lease.clone();
            store::append_event(
                base,
                &repo_id,
                &serde_json::json!({
                    "ts_ms": now, "kind": "adopted", "thread": thread_id, "holder": holder,
                }),
            );
            Ok((thread, lease))
        })
    }

    // -- the weave gate -----------------------------------------------------

    /// Run the weave gate for a thread: materialize its head stitch over a
    /// scratch copy of the repo, run the repo's verify command there, and
    /// record the outcome as a [`Weave`]. Green flips the thread to
    /// Proposed; red keeps it Active with the failure noted. NOTHING here
    /// touches the real working tree — landing is [`Loom::land_weave`],
    /// which callers gate behind [`consent::WeaveConsent`] (or a parked
    /// approval, when embedded in a host with an approvals queue).
    ///
    /// Blocking (repo copy + verify command); on async runtimes call it from
    /// a blocking-task helper.
    pub fn propose(&self, thread_id: &str) -> Result<ProposeOutcome, String> {
        let now = now_ms();
        // Phase 1 (locked): validate and flip to Proposed so a second
        // propose of the same thread is refused while the gate runs.
        let (repo, manifest) = self.with(|s, base| {
            let repo_id = find_repo_of_thread(s, thread_id)
                .ok_or_else(|| format!("no thread with id {thread_id}"))?;
            reconcile_repo(s, &repo_id, now, base);
            let repo = s
                .repos
                .iter()
                .find(|r| r.id == repo_id)
                .cloned()
                .ok_or_else(|| format!("repo {repo_id} vanished from the registry"))?;
            let rs = s.repo_states.get_mut(&repo_id).expect("repo state exists");
            let thread = rs
                .threads
                .iter_mut()
                .find(|t| t.id == thread_id)
                .ok_or_else(|| format!("no thread with id {thread_id}"))?;
            match thread.status {
                ThreadStatus::Active | ThreadStatus::Adopted => {}
                ThreadStatus::Proposed => {
                    return Err("already proposed — a weave is in flight for this thread".to_string())
                }
                ThreadStatus::Orphaned => return Err("thread is orphaned; adopt it first".into()),
                ThreadStatus::Woven => return Err("thread already wove onto the fabric".into()),
            }
            let head = thread
                .head_stitch
                .clone()
                .ok_or_else(|| "nothing to weave — capture a stitch first".to_string())?;
            let manifest = rs
                .stitches
                .iter()
                .find(|st| st.id == head)
                .map(|st| st.files.clone())
                .ok_or_else(|| format!("head stitch {head} not found"))?;
            thread.status = ThreadStatus::Proposed;
            thread.note = "verify running".into();
            Ok((repo, manifest))
        })?;
        // Phase 2 (unlocked): scratch worktree + verify. A failure to even
        // materialize is recorded as a red verify, not a silent unwind.
        let objects = store::objects_dir(&self.base(), &repo.id);
        let verify = weave::run_gate(&repo, &manifest, &objects);
        // Phase 3 (locked): record the weave against the CURRENT fabric tip.
        self.with(|s, base| {
            let rs = s.repo_states.get_mut(&repo.id).expect("repo state exists");
            rs.seq += 1;
            let green = verify.result == VerifyResult::Green;
            let weave = Weave {
                id: format!("weave-{}-{}", now_ms(), rs.seq),
                thread_id: thread_id.to_string(),
                fabric_parent: rs.fabric.tip.clone(),
                verify: verify.clone(),
                ts_ms: now_ms(),
            };
            rs.weaves.push(weave.clone());
            let thread = rs
                .threads
                .iter_mut()
                .find(|t| t.id == thread_id)
                .ok_or_else(|| format!("thread {thread_id} vanished mid-propose"))?;
            if green {
                thread.status = ThreadStatus::Proposed;
                thread.note = "verify green — awaiting a human yes to weave".into();
            } else {
                thread.status = ThreadStatus::Active;
                thread.note = format!("verify red ({})", cap(&verify.log_tail, 160));
            }
            // Any previous approval no longer gates this thread — an
            // embedding host records the fresh one (green) via `mark_parked`.
            thread.approval_id = None;
            let thread = thread.clone();
            store::append_event(
                base,
                &repo.id,
                &serde_json::json!({
                    "ts_ms": weave.ts_ms,
                    "kind": if green { "weave_green" } else { "weave_red" },
                    "weave": weave.id, "thread": thread_id, "cmd": verify.cmd,
                }),
            );
            Ok(ProposeOutcome {
                weave,
                repo: repo.clone(),
                thread,
                green,
            })
        })
    }

    /// Land an approved green weave: overlay the stitched files onto the
    /// REAL repo working tree, advance the fabric tip, mark the thread
    /// Woven, release its lease. Precondition-checked: the weave must be
    /// green and its `fabric_parent` must still be the fabric tip — if
    /// another weave landed in between, this refuses and the thread keeps
    /// Proposed with an honest "re-propose" note. Callers reach this only
    /// after an explicit human yes ([`consent::WeaveConsent`] in the
    /// standalone binary, an operator Approve in an embedding host).
    pub fn land_weave(&self, weave_id: &str) -> Result<LandOutcome, String> {
        let now = now_ms();
        self.with(|s, base| {
            let repo_id = s
                .repo_states
                .iter()
                .find(|(_, rs)| rs.weaves.iter().any(|w| w.id == weave_id))
                .map(|(id, _)| id.clone())
                .ok_or_else(|| format!("no weave with id {weave_id}"))?;
            let repo = s
                .repos
                .iter()
                .find(|r| r.id == repo_id)
                .cloned()
                .ok_or_else(|| format!("repo {repo_id} vanished from the registry"))?;
            let rs = s.repo_states.get_mut(&repo_id).expect("repo state exists");
            let weave = rs
                .weaves
                .iter()
                .find(|w| w.id == weave_id)
                .cloned()
                .expect("found above");
            if weave.verify.result != VerifyResult::Green {
                return Err("only a green weave can land".into());
            }
            if rs.fabric.history.iter().any(|w| w == weave_id) {
                return Err("weave already landed".into());
            }
            if rs.fabric.tip != weave.fabric_parent {
                if let Some(t) = rs.threads.iter_mut().find(|t| t.id == weave.thread_id) {
                    t.note = "fabric advanced since this verify — re-propose".into();
                }
                return Err(
                    "fabric advanced since this weave's verify — re-propose the thread".into(),
                );
            }
            let manifest = rs
                .stitches
                .iter()
                .rev()
                .find(|st| st.thread_id == weave.thread_id)
                .map(|st| st.files.clone())
                .ok_or_else(|| "thread's stitches are gone; cannot apply".to_string())?;
            let objects = store::objects_dir(base, &repo_id);
            let applied =
                weave::apply_overlay(std::path::Path::new(&repo.path), &manifest, &objects)?;
            rs.fabric.tip = Some(weave.id.clone());
            rs.fabric.history.push(weave.id.clone());
            let mut criteria = Vec::new();
            let mut lease_to_drop = None;
            let thread = rs
                .threads
                .iter_mut()
                .find(|t| t.id == weave.thread_id)
                .ok_or_else(|| format!("thread {} vanished", weave.thread_id))?;
            thread.status = ThreadStatus::Woven;
            thread.note = "woven".into();
            thread.approval_id = None;
            if let Some(lid) = thread.lease_id.clone() {
                lease_to_drop = Some(lid);
            }
            let thread = thread.clone();
            if let Some(lid) = &lease_to_drop {
                if let Some(l) = rs.leases.iter().find(|l| l.id == *lid) {
                    criteria = l.criteria.clone();
                }
                rs.leases.retain(|l| l.id != *lid);
            }
            store::append_event(
                base,
                &repo_id,
                &serde_json::json!({
                    "ts_ms": now, "kind": "weave_landed", "weave": weave.id,
                    "thread": weave.thread_id, "files": applied,
                }),
            );
            Ok(LandOutcome {
                repo,
                thread,
                weave,
                criteria,
                files_applied: applied,
            })
        })
    }

    /// Record an operator Deny: the thread stays Proposed (its green verify
    /// still stands) with the denial noted, and the parked approval no longer
    /// gates it (`approval_id` clears — the recoverable state [`Loom::withdraw`]
    /// resolves). A thread that already moved on (withdrawn to Active before
    /// the deny landed) is left untouched — the deny arrived late and lost.
    pub fn deny_weave(&self, weave_id: &str, note: &str) -> Result<(), String> {
        self.with(|s, base| {
            let repo_id = s
                .repo_states
                .iter()
                .find(|(_, rs)| rs.weaves.iter().any(|w| w.id == weave_id))
                .map(|(id, _)| id.clone())
                .ok_or_else(|| format!("no weave with id {weave_id}"))?;
            let rs = s.repo_states.get_mut(&repo_id).expect("repo state exists");
            let thread_id = rs
                .weaves
                .iter()
                .find(|w| w.id == weave_id)
                .map(|w| w.thread_id.clone())
                .expect("found above");
            if let Some(t) = rs
                .threads
                .iter_mut()
                .find(|t| t.id == thread_id && t.status == ThreadStatus::Proposed)
            {
                t.note = cap(note, MAX_GOAL_CHARS);
                t.approval_id = None;
            }
            store::append_event(
                base,
                &repo_id,
                &serde_json::json!({
                    "ts_ms": now_ms(), "kind": "weave_denied", "weave": weave_id,
                    "thread": thread_id,
                }),
            );
            Ok(())
        })
    }

    /// Remember which parked approval gates a Proposed thread. Called by an
    /// embedding host's consent layer right after it parks one — the engine
    /// itself never talks to any approvals registry, and the standalone
    /// binary (whose consent is synchronous) never calls this. This is what
    /// makes a lapsed approval *detectable*: see [`Loom::reconcile_parked`].
    pub fn mark_parked(&self, thread_id: &str, approval_id: &str) -> Result<(), String> {
        self.with(|s, base| {
            let repo_id = find_repo_of_thread(s, thread_id)
                .ok_or_else(|| format!("no thread with id {thread_id}"))?;
            let rs = s.repo_states.get_mut(&repo_id).expect("repo state exists");
            let thread = rs
                .threads
                .iter_mut()
                .find(|t| t.id == thread_id)
                .expect("found above");
            if thread.status != ThreadStatus::Proposed {
                return Err(format!(
                    "thread {} is {:?}, not proposed — no approval to record",
                    thread_id, thread.status
                ));
            }
            thread.approval_id = Some(approval_id.to_string());
            store::append_event(
                base,
                &repo_id,
                &serde_json::json!({
                    "ts_ms": now_ms(), "kind": "approval_parked",
                    "thread": thread_id, "approval_id": approval_id,
                }),
            );
            Ok(())
        })
    }

    /// Withdraw a Proposed thread: it returns to Active with an honest note,
    /// so the holder can keep working and re-propose. This is the manual exit
    /// from every stuck-Proposed state — denied, refused at the terminal, or
    /// orphaned by a host restart. Hands back the approval id that was gating
    /// it (if any) so an embedding host can resolve the now-moot approval.
    pub fn withdraw(&self, thread_id: &str, note: &str) -> Result<(Thread, Option<String>), String> {
        let now = now_ms();
        self.with(|s, base| {
            let repo_id = find_repo_of_thread(s, thread_id)
                .ok_or_else(|| format!("no thread with id {thread_id}"))?;
            reconcile_repo(s, &repo_id, now, base);
            let rs = s.repo_states.get_mut(&repo_id).expect("repo state exists");
            let thread = rs
                .threads
                .iter_mut()
                .find(|t| t.id == thread_id)
                .ok_or_else(|| format!("no thread with id {thread_id}"))?;
            if thread.status != ThreadStatus::Proposed {
                return Err(format!(
                    "thread {} is {:?}, not proposed — only a proposed thread can be withdrawn",
                    thread_id, thread.status
                ));
            }
            thread.status = ThreadStatus::Active;
            thread.note = cap(
                if note.trim().is_empty() {
                    "withdrawn — re-propose when ready"
                } else {
                    note
                },
                MAX_GOAL_CHARS,
            );
            let approval_id = thread.approval_id.take();
            let thread = thread.clone();
            store::append_event(
                base,
                &repo_id,
                &serde_json::json!({
                    "ts_ms": now, "kind": "withdrawn", "thread": thread_id,
                    "approval_id": approval_id,
                }),
            );
            Ok((thread, approval_id))
        })
    }

    /// Auto-recover Proposed threads whose parked approval no longer exists.
    /// In hosts whose approvals are in-memory only, a restart drops every
    /// one, and a timeout can resolve them Deny without this engine hearing
    /// about it in all paths. `pending` is the live approval-id set (the
    /// host reads it from its approvals registry); any Proposed thread
    /// pointing at an id not in it returns to Active with an honest note,
    /// ready to re-propose.
    /// Threads with no recorded approval id are left alone — they are either
    /// mid-gate (verify running / park in flight) or already denied, and the
    /// manual [`Loom::withdraw`] covers those.
    pub fn reconcile_parked(&self, pending: &std::collections::HashSet<String>) {
        let now = now_ms();
        self.with(|s, base| {
            for (repo_id, rs) in s.repo_states.iter_mut() {
                for t in rs.threads.iter_mut() {
                    let lapsed = t.status == ThreadStatus::Proposed
                        && t.approval_id.as_ref().is_some_and(|id| !pending.contains(id));
                    if !lapsed {
                        continue;
                    }
                    let gone = t.approval_id.take();
                    t.status = ThreadStatus::Active;
                    t.note = "approval lapsed — re-propose when ready".into();
                    store::append_event(
                        base,
                        repo_id,
                        &serde_json::json!({
                            "ts_ms": now, "kind": "approval_lapsed",
                            "thread": t.id, "approval_id": gone,
                        }),
                    );
                }
            }
        });
    }

    // -- reads --------------------------------------------------------------

    /// A full clone of the registry + per-repo state, for callers to shape
    /// into output. Reconciles orphans first, so what you see is true.
    pub fn snapshot(&self) -> LoomState {
        let now = now_ms();
        self.with(|s, base| {
            let ids: Vec<String> = s.repos.iter().map(|r| r.id.clone()).collect();
            for id in ids {
                reconcile_repo(s, &id, now, base);
            }
            s.clone()
        })
    }

    /// Everything about one thread: its stitch chain, latest manifest, and
    /// a diff summary vs the fabric (v1: vs the real working tree, which IS
    /// the fabric's materialized state — see `docs/DESIGN.md`).
    pub fn thread_detail(&self, thread_id: &str) -> Result<serde_json::Value, String> {
        let now = now_ms();
        let (repo, thread, lease, stitches) = self.with(|s, base| {
            let repo_id = find_repo_of_thread(s, thread_id)
                .ok_or_else(|| format!("no thread with id {thread_id}"))?;
            reconcile_repo(s, &repo_id, now, base);
            let repo = s
                .repos
                .iter()
                .find(|r| r.id == repo_id)
                .cloned()
                .ok_or_else(|| format!("repo {repo_id} vanished from the registry"))?;
            let rs = s.repo_states.get(&repo_id).expect("repo state exists");
            let thread = rs
                .threads
                .iter()
                .find(|t| t.id == thread_id)
                .cloned()
                .expect("found above");
            let lease = thread
                .lease_id
                .as_ref()
                .and_then(|lid| rs.leases.iter().find(|l| l.id == *lid))
                .cloned();
            let stitches: Vec<Stitch> = rs
                .stitches
                .iter()
                .filter(|st| st.thread_id == thread_id)
                .cloned()
                .collect();
            Ok::<_, String>((repo, thread, lease, stitches))
        })?;
        let latest = stitches.last().map(|st| st.files.clone());
        // Diff summary vs the working tree (unlocked; read-only hashing).
        let diff = latest
            .as_ref()
            .map(|m| weave::diff_vs_worktree(std::path::Path::new(&repo.path), m))
            .unwrap_or_default();
        Ok(serde_json::json!({
            "thread": thread,
            "lease": lease,
            "stitches": stitches
                .iter()
                .map(|st| serde_json::json!({
                    "id": st.id, "parent": st.parent, "ts_ms": st.ts_ms,
                    "files": st.files.len(),
                }))
                .collect::<Vec<_>>(),
            "latest_manifest": latest,
            "diff_vs_fabric": diff,
        }))
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Expire leases past TTL and orphan their live threads. Runs inside the
/// store lock; appends an `orphaned` log line per flip.
fn reconcile_repo(s: &mut LoomState, repo_id: &str, now: u64, base: &PathBuf) {
    let Some(rs) = s.repo_states.get_mut(repo_id) else {
        return;
    };
    let expired: Vec<String> = rs
        .leases
        .iter()
        .filter(|l| l.expired(now))
        .map(|l| l.id.clone())
        .collect();
    for lid in expired {
        for t in rs.threads.iter_mut() {
            if t.lease_id.as_deref() == Some(&lid) && t.status.is_live() {
                t.status = ThreadStatus::Orphaned;
                t.note = "lease expired — adoptable".into();
                store::append_event(
                    base,
                    repo_id,
                    &serde_json::json!({
                        "ts_ms": now, "kind": "orphaned", "thread": t.id, "lease": lid,
                    }),
                );
            }
        }
        // The lease record is KEPT (goal/criteria/scope ride along for the
        // adopter); it just no longer counts as live anywhere.
    }
}

fn find_repo_of_lease(s: &LoomState, lease_id: &str) -> Option<String> {
    s.repo_states
        .iter()
        .find(|(_, rs)| rs.leases.iter().any(|l| l.id == lease_id))
        .map(|(id, _)| id.clone())
}

fn find_repo_of_thread(s: &LoomState, thread_id: &str) -> Option<String> {
    s.repo_states
        .iter()
        .find(|(_, rs)| rs.threads.iter().any(|t| t.id == thread_id))
        .map(|(id, _)| id.clone())
}

/// Bounded history, live objects never dropped to make room: terminal
/// (Woven) threads go oldest-first; stitches beyond the per-thread cap drop
/// oldest-first except a thread's head; weaves and toe-steps are rings.
fn prune(s: &mut LoomState) {
    for rs in s.repo_states.values_mut() {
        while rs.threads.len() > MAX_THREADS_PER_REPO {
            let Some(pos) = rs
                .threads
                .iter()
                .position(|t| t.status == ThreadStatus::Woven)
            else {
                break;
            };
            let dead = rs.threads.remove(pos);
            rs.stitches.retain(|st| st.thread_id != dead.id);
            if let Some(lid) = dead.lease_id {
                rs.leases.retain(|l| l.id != lid);
            }
        }
        let thread_ids: Vec<String> = rs.threads.iter().map(|t| t.id.clone()).collect();
        for tid in thread_ids {
            let head = rs
                .threads
                .iter()
                .find(|t| t.id == tid)
                .and_then(|t| t.head_stitch.clone());
            loop {
                let count = rs.stitches.iter().filter(|st| st.thread_id == tid).count();
                if count <= MAX_STITCHES_PER_THREAD {
                    break;
                }
                let Some(pos) = rs
                    .stitches
                    .iter()
                    .position(|st| st.thread_id == tid && Some(&st.id) != head.as_ref())
                else {
                    break;
                };
                rs.stitches.remove(pos);
            }
        }
        while rs.weaves.len() > MAX_WEAVES_PER_REPO {
            rs.weaves.remove(0);
        }
        while rs.toe_steps.len() > MAX_TOE_STEPS {
            rs.toe_steps.remove(0);
        }
    }
}

/// Stable repo id from the canonical path: `repo-<first 16 hex of sha256>`.
pub fn repo_id_for(canonical_path: &str) -> String {
    format!("repo-{}", &store::content_hash(canonical_path.as_bytes())[..16])
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn cap(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    let mut out: String = t.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "loom-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mk scratch");
        p
    }

    fn mk_repo(dir: &PathBuf, files: &[(&str, &str)]) {
        for (rel, body) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            let mut f = std::fs::File::create(&p).unwrap();
            f.write_all(body.as_bytes()).unwrap();
        }
    }

    fn rig(tag: &str) -> (Loom, PathBuf, RepoConfig) {
        let base = scratch(&format!("{tag}-data"));
        let repo_dir = scratch(&format!("{tag}-repo"));
        mk_repo(
            &repo_dir,
            &[("src/main.rs", "fn main() {}\n"), ("README.md", "hi\n")],
        );
        let loom = Loom::at(base.clone());
        let repo = loom
            .register_repo(repo_dir.to_str().unwrap(), Some("true".into()), false)
            .expect("register");
        (loom, repo_dir, repo)
    }

    #[test]
    fn repo_id_is_a_stable_hash_of_the_canonical_path() {
        let a = repo_id_for("/some/where");
        assert_eq!(a, repo_id_for("/some/where"));
        assert_ne!(a, repo_id_for("/some/where/else"));
        assert!(a.starts_with("repo-"));
    }

    #[test]
    fn reregistering_the_same_dir_updates_in_place() {
        let (loom, repo_dir, repo) = rig("rereg");
        let again = loom
            .register_repo(repo_dir.to_str().unwrap(), Some("false".into()), true)
            .expect("re-register");
        assert_eq!(again.id, repo.id);
        assert_eq!(again.verify_cmd, "false");
        assert!(again.git_bridge);
        assert_eq!(loom.snapshot().repos.len(), 1);
    }

    #[test]
    fn stitch_dedups_unchanged_content_and_manifests() {
        let (loom, repo_dir, repo) = rig("dedup");
        let d = loom
            .declare_lease(&repo.id, "tester", "touch src", vec!["src/**".into()], vec![], None)
            .expect("lease");
        let s1 = loom.stitch(&d.lease.id).expect("stitch 1");
        assert!(!s1.unchanged);
        assert_eq!(s1.stitch.files.len(), 1); // src/main.rs only — README out of scope
        // Same content again: NO new stitch.
        let s2 = loom.stitch(&d.lease.id).expect("stitch 2");
        assert!(s2.unchanged);
        assert_eq!(s2.stitch.id, s1.stitch.id);
        // Change the file: new stitch, new hash, parent chained.
        mk_repo(&repo_dir, &[("src/main.rs", "fn main() { /* v2 */ }\n")]);
        let s3 = loom.stitch(&d.lease.id).expect("stitch 3");
        assert!(!s3.unchanged);
        assert_eq!(s3.stitch.parent.as_deref(), Some(s1.stitch.id.as_str()));
        assert_ne!(s3.stitch.files["src/main.rs"], s1.stitch.files["src/main.rs"]);
        // Content-addressing: both blobs exist exactly once each.
        let objects = store::objects_dir(&loom.base(), &repo.id);
        for h in [&s1.stitch.files["src/main.rs"], &s3.stitch.files["src/main.rs"]] {
            assert!(store::read_blob(&objects, h).is_ok(), "blob {h} present");
        }
    }

    #[test]
    fn orphan_expiry_then_adopt_preserves_goal_and_criteria() {
        let (loom, _repo_dir, repo) = rig("orphan");
        let d = loom
            .declare_lease(
                &repo.id,
                "first-holder",
                "migrate the schema",
                vec!["src/**".into()],
                vec!["tests pass".into()],
                Some(MIN_TTL_MS),
            )
            .expect("lease");
        // Not yet expired: still active.
        loom.reconcile();
        let snap = loom.snapshot();
        let rs = &snap.repo_states[&repo.id];
        assert_eq!(rs.threads[0].status, ThreadStatus::Active);
        // Age the heartbeat past TTL by editing state directly (no sleeping
        // in tests), then reconcile.
        loom.with(|s, _| {
            s.repo_states.get_mut(&repo.id).unwrap().leases[0].last_heartbeat_ms = 1;
        });
        loom.reconcile();
        let snap = loom.snapshot();
        let rs = &snap.repo_states[&repo.id];
        assert_eq!(rs.threads[0].status, ThreadStatus::Orphaned);
        // Heartbeat is refused once orphaned.
        assert!(loom.heartbeat(&d.lease.id).is_err());
        // Adopt: same lease id, new holder, fresh heartbeat, criteria intact.
        let (thread, lease) = loom.adopt(&d.thread.id, "second-holder").expect("adopt");
        assert_eq!(thread.status, ThreadStatus::Adopted);
        assert_eq!(lease.id, d.lease.id);
        assert_eq!(lease.holder, "second-holder");
        assert_eq!(lease.goal, "migrate the schema");
        assert_eq!(lease.criteria, vec!["tests pass".to_string()]);
        assert!(!lease.expired(now_ms()));
        // Adopted has Active semantics: heartbeat works again.
        assert!(loom.heartbeat(&d.lease.id).is_ok());
    }

    #[test]
    fn weave_gate_green_lands_only_via_land_weave_and_red_never_does() {
        let (loom, repo_dir, repo) = rig("gate");
        let d = loom
            .declare_lease(&repo.id, "t", "greenify", vec!["src/**".into()], vec![], None)
            .expect("lease");
        loom.stitch(&d.lease.id).expect("stitch");
        // verify_cmd is "true" → green; thread flips to Proposed, fabric
        // untouched until land.
        let out = loom.propose(&d.thread.id).expect("propose");
        assert!(out.green);
        assert_eq!(out.thread.status, ThreadStatus::Proposed);
        assert!(loom.snapshot().repo_states[&repo.id].fabric.tip.is_none());
        // A second propose while one is in flight is refused.
        assert!(loom.propose(&d.thread.id).is_err());
        // Land (callers reach this only after an explicit human yes).
        let landed = loom.land_weave(&out.weave.id).expect("land");
        assert_eq!(landed.thread.status, ThreadStatus::Woven);
        let snap = loom.snapshot();
        let rs = &snap.repo_states[&repo.id];
        assert_eq!(rs.fabric.tip.as_deref(), Some(out.weave.id.as_str()));
        assert_eq!(rs.fabric.history, vec![out.weave.id.clone()]);
        assert!(rs.leases.is_empty(), "lease released on land");
        // Red: new thread, repo re-registered with a failing verify.
        loom.register_repo(repo_dir.to_str().unwrap(), Some("false".into()), false)
            .expect("re-register red");
        let d2 = loom
            .declare_lease(&repo.id, "t", "reddify", vec!["src/**".into()], vec![], None)
            .expect("lease 2");
        loom.stitch(&d2.lease.id).expect("stitch 2");
        let out2 = loom.propose(&d2.thread.id).expect("propose 2");
        assert!(!out2.green);
        assert_eq!(out2.thread.status, ThreadStatus::Active, "red keeps the thread active");
        assert!(out2.thread.note.starts_with("verify red"));
        // Red can never land, even if someone tries.
        assert!(loom.land_weave(&out2.weave.id).is_err());
        let snap = loom.snapshot();
        assert_eq!(
            snap.repo_states[&repo.id].fabric.history.len(),
            1,
            "fabric unchanged by red"
        );
    }

    #[test]
    fn fabric_ordering_a_stale_parent_refuses_to_land_until_reproposed() {
        let (loom, _repo_dir, repo) = rig("order");
        let mk = |goal: &str, scope: &str| {
            let d = loom
                .declare_lease(&repo.id, "t", goal, vec![scope.into()], vec![], None)
                .expect("lease");
            loom.stitch(&d.lease.id).expect("stitch");
            let proposed = loom.propose(&d.thread.id).expect("propose");
            (d, proposed)
        };
        let (_d1, w1) = mk("first", "src/**");
        let (d2, w2) = mk("second", "README.md");
        // Both verified against an empty fabric; first lands fine.
        loom.land_weave(&w1.weave.id).expect("land w1");
        // Second's parent is stale — refused, honestly noted.
        let err = loom.land_weave(&w2.weave.id).unwrap_err();
        assert!(err.contains("re-propose"), "got: {err}");
        // Thread 2 must re-propose... but it is Proposed; deny path resets
        // nothing, so flip via a fresh propose after the note. v1 keeps this
        // manual: adopt/propose guards mean we go through deny first.
        loom.with(|s, _| {
            let rs = s.repo_states.get_mut(&repo.id).unwrap();
            let t = rs.threads.iter_mut().find(|t| t.id == d2.thread.id).unwrap();
            t.status = ThreadStatus::Active; // simulate agent acting on the note
        });
        let w2b = loom.propose(&d2.thread.id).expect("re-propose");
        assert_eq!(w2b.weave.fabric_parent.as_deref(), Some(w1.weave.id.as_str()));
        loom.land_weave(&w2b.weave.id).expect("land w2b");
        let snap = loom.snapshot();
        assert_eq!(
            snap.repo_states[&repo.id].fabric.history,
            vec![w1.weave.id.clone(), w2b.weave.id.clone()],
            "history is orderly, parent-linked"
        );
    }

    #[test]
    fn deny_keeps_the_thread_proposed_with_the_note() {
        let (loom, _repo_dir, repo) = rig("deny");
        let d = loom
            .declare_lease(&repo.id, "t", "goal", vec!["src/**".into()], vec![], None)
            .expect("lease");
        loom.stitch(&d.lease.id).expect("stitch");
        let out = loom.propose(&d.thread.id).expect("propose");
        loom.mark_parked(&d.thread.id, "appr-1").expect("mark parked");
        loom.deny_weave(&out.weave.id, "denied by operator").expect("deny");
        let snap = loom.snapshot();
        let t = &snap.repo_states[&repo.id].threads[0];
        assert_eq!(t.status, ThreadStatus::Proposed);
        assert_eq!(t.note, "denied by operator");
        // The resolved approval no longer gates the thread — this is the
        // recoverable state `withdraw` exists for.
        assert!(t.approval_id.is_none());
        assert!(snap.repo_states[&repo.id].fabric.tip.is_none());
    }

    #[test]
    fn withdraw_returns_a_proposed_thread_to_active_and_allows_repropose() {
        let (loom, _repo_dir, repo) = rig("withdraw");
        let d = loom
            .declare_lease(&repo.id, "t", "goal", vec!["src/**".into()], vec![], None)
            .expect("lease");
        // Withdraw on a non-proposed thread refuses.
        assert!(loom.withdraw(&d.thread.id, "").is_err());
        loom.stitch(&d.lease.id).expect("stitch");
        let out = loom.propose(&d.thread.id).expect("propose");
        assert!(out.green);
        loom.mark_parked(&d.thread.id, "appr-w").expect("mark parked");
        // While Proposed, a second propose is refused — the stuck state.
        assert!(loom.propose(&d.thread.id).is_err());
        // Withdraw: back to Active with the note, approval id handed back so
        // an embedding host can resolve the moot parked approval.
        let (t, aid) = loom.withdraw(&d.thread.id, "").expect("withdraw");
        assert_eq!(t.status, ThreadStatus::Active);
        assert_eq!(t.note, "withdrawn — re-propose when ready");
        assert!(t.approval_id.is_none());
        assert_eq!(aid.as_deref(), Some("appr-w"));
        // A late deny for the old weave must not clobber the Active thread.
        loom.deny_weave(&out.weave.id, "denied by operator").expect("late deny");
        let snap = loom.snapshot();
        let t = &snap.repo_states[&repo.id].threads[0];
        assert_eq!(t.status, ThreadStatus::Active);
        assert_eq!(t.note, "withdrawn — re-propose when ready");
        // And re-propose works.
        let again = loom.propose(&d.thread.id).expect("re-propose");
        assert!(again.green);
        assert_eq!(again.thread.status, ThreadStatus::Proposed);
    }

    #[test]
    fn a_lapsed_parked_approval_reconciles_the_thread_back_to_active() {
        let (loom, _repo_dir, repo) = rig("lapse");
        let d = loom
            .declare_lease(&repo.id, "t", "goal", vec!["src/**".into()], vec![], None)
            .expect("lease");
        loom.stitch(&d.lease.id).expect("stitch");
        loom.propose(&d.thread.id).expect("propose");
        loom.mark_parked(&d.thread.id, "appr-live").expect("mark parked");
        // The approval is still pending → nothing changes.
        let pending: std::collections::HashSet<String> =
            ["appr-live".to_string()].into_iter().collect();
        loom.reconcile_parked(&pending);
        let t = loom.snapshot().repo_states[&repo.id].threads[0].clone();
        assert_eq!(t.status, ThreadStatus::Proposed);
        assert_eq!(t.approval_id.as_deref(), Some("appr-live"));
        // The approval vanished (a host restart or timeout killed it — they
        // are in-memory only) → the thread returns to Active, honestly noted.
        loom.reconcile_parked(&std::collections::HashSet::new());
        let t = loom.snapshot().repo_states[&repo.id].threads[0].clone();
        assert_eq!(t.status, ThreadStatus::Active);
        assert_eq!(t.note, "approval lapsed — re-propose when ready");
        assert!(t.approval_id.is_none());
        // Recovery is real: propose works again.
        assert!(loom.propose(&d.thread.id).expect("re-propose").green);
        // A Proposed thread with NO recorded approval id (mid-gate) is never
        // touched by reconcile — only `withdraw` may move it.
        loom.reconcile_parked(&std::collections::HashSet::new());
        let t = loom.snapshot().repo_states[&repo.id].threads[0].clone();
        assert_eq!(t.status, ThreadStatus::Proposed, "no approval id → left alone");
    }

    #[test]
    fn state_survives_a_cache_drop_and_a_corrupt_log_line() {
        let (loom, _repo_dir, repo) = rig("persist");
        loom.declare_lease(&repo.id, "t", "goal", vec!["src/**".into()], vec![], None)
            .expect("lease");
        // Corrupt the log mid-file; state.json untouched.
        let log = loom.base().join(&repo.id).join("log.jsonl");
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(b"{ this is not json\n").unwrap();
        drop(f);
        loom.reset_cache();
        let snap = loom.snapshot();
        assert_eq!(snap.repos.len(), 1);
        assert_eq!(snap.repo_states[&repo.id].threads.len(), 1);
        // The readable events survive around the corrupt line.
        let events = store::read_events(&loom.base(), &repo.id, 50);
        assert!(events.iter().any(|e| e["kind"] == "lease_declared"));
    }
}
