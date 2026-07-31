// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! **Heddle** — version control for many hands moving at once (see
//! `docs/DESIGN.md` for the full design).
//!
//! Git detects collisions at merge time, hours after the toes were stepped
//! on; leaves crashed agents' work as unowned dirty worktrees; and makes
//! "green main" a snapshot, not an invariant. Heddle is the coordination
//! layer that sits above git: each task runs in its **own git worktree**
//! (isolation — two threads physically cannot clobber each other), declares
//! an **intent lease** before touching files (coordination — collisions
//! warn at declaration time, when they are cheap), checkpoints **stitches**
//! every few seconds (deletions tracked as tombstones), and lands on the
//! shared **fabric** only through a **weave gate** that verifies green in a
//! scratch copy and MERGES at file level — a file changed in both the
//! fabric and the thread refuses the land with an honest "fabric moved
//! under you — rebase" instead of overwriting either side. Crashed work
//! becomes an adoptable **orphan**, never a mess. The [`sync`] module
//! extends all of it across machines over any shared git remote: state
//! rides hidden `refs/heddle/*` refs, and fabric authority is a
//! compare-and-swap ref push — git's atomic ref update is the shuttle
//! token.
//!
//! **The trust boundary, stated plainly:**
//!
//! * **A lease is knowledge, not a lock.** Declaring a scope that overlaps a
//!   live lease SUCCEEDS — the collision is surfaced the moment it is cheap,
//!   as a recorded `toe_step` warning carrying both goals and a suggested
//!   split. Nothing in Heddle ever blocks an agent from working.
//! * **A thread edits its own worktree, not the repo.** Isolated threads
//!   (the default on git repos) get a detached worktree under the heddle data
//!   dir; the repo tree changes only when a weave lands, through the merge
//!   rules above. In-place mode (`--in-place`, or a non-git repo) keeps the
//!   old direct-edit behavior, honestly labeled.
//! * **A stitch only READS.** Capturing a stitch walks the leased scope in
//!   the thread's working dir and snapshots file contents (and deletions)
//!   into a content-addressed store under the data dir.
//! * **The weave gate verifies in a scratch copy, never in the real tree.**
//!   `propose` copies the repo to a scratch dir, overlays the thread's
//!   delta, and runs the repo's verify command there. Red never lands — the
//!   failure is recorded and the thread stays active.
//! * **Applying a green weave to the real working tree is an ACTION.** It
//!   requires an explicit human yes, expressed through the
//!   [`consent::WeaveConsent`] trait — an interactive terminal prompt in the
//!   standalone binary, a parked approval when embedded in a host with an
//!   approvals queue. Until that yes, the honest answer is *"verified green;
//!   nothing was applied."* A refusal leaves the working tree untouched with
//!   the reason noted on the thread.
//! * **The git bridge never pushes.** When a repo was registered with
//!   `git_bridge: true`, a landed weave projects into local git history at
//!   the repo's configured granularity ([`BridgeMode`]): `squash` (one
//!   commit per weave — the default), `stitches` (checkpoint commits on a
//!   per-thread branch, merged with the weave message), or `both` (squash
//!   plus the branch kept unmerged). `heddle export` writes an unlanded
//!   thread's stitch chain to its per-thread branch for human review.
//! * **Sync is explicit and says what it shares.** `heddle sync` publishes
//!   lease/thread metadata AND scoped file blobs to the configured remote —
//!   the same exposure as pushing a branch there; `--auto` is a per-repo
//!   opt-in. Objects are unsigned in this version (machine ids are
//!   identity, not authentication); a `sig` field slots in via serde
//!   default without a format break.
//!
//! Storage is "boring on purpose": a JSON state file plus an append-only
//! JSONL event log per repo under `<data dir>/<repo_id>/` (default `~/.heddle`,
//! overridable via the `HEDDLE_DATA` env var or [`Heddle::at`]), 0o600, bounded,
//! corrupt-line tolerant, and a content-addressed blob store (whole files
//! keyed by sha256; rolling-hash chunking is future work).

pub mod bridge;
pub mod consent;
pub mod lease;
pub mod solo;
pub mod store;
pub mod sync;
pub mod weave;
pub mod worktree;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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

/// The manifest value that records a deletion: a thread that deleted a
/// scoped file carries this instead of a content hash, and applying it
/// removes the file. Deliberately not 64 hex chars, so it can never collide
/// with a real content hash (and `read_blob` refuses it outright).
pub const TOMBSTONE: &str = "deleted";

/// Caps on stored text and collection sizes.
pub const MAX_GOAL_CHARS: usize = 300;
pub const MAX_CRITERIA: usize = 20;
pub const MAX_SCOPE_PATTERNS: usize = 32;
pub const MAX_PATTERN_CHARS: usize = 300;
/// How many repos one person may register. The other caps here exist to stop a
/// runaway agent filling the disk; this one does not — "how many projects do
/// you own" is a fact about the user, not a risk. At 20 it refused the twenty
/// first repo of a real working checkout, which reads as Heddle being broken
/// rather than Heddle protecting anything.
pub const MAX_REPOS: usize = 200;
pub const MAX_THREADS_PER_REPO: usize = 200;
pub const MAX_STITCHES_PER_THREAD: usize = 200;
pub const MAX_WEAVES_PER_REPO: usize = 500;
pub const MAX_TOE_STEPS: usize = 100;

// ---------------------------------------------------------------------------
// Object model — all objects carry ids and serialize cleanly, so the same
// shapes can gossip between peers later. TODO(federation): a `sig` field
// per object, serde-defaulted, once a per-machine key signs them.
// ---------------------------------------------------------------------------

/// How the git bridge projects a landed weave into git history.
/// One lease = one goal = one commit is the intended granularity — scope
/// leases small and `Squash` reads like a clean semantic log. The other
/// modes exist for teams who want checkpoint-level history in git itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BridgeMode {
    /// One commit per weave on the current branch (goal + criteria +
    /// verify). The default.
    #[default]
    Squash,
    /// Replay the thread's stitch chain as individual commits on a
    /// per-thread branch `heddle/<thread-id-short>-<goal-slug>`, then merge
    /// that branch into the current branch with a merge commit carrying the
    /// weave message. History shows every checkpoint AND the semantic
    /// landing.
    Stitches,
    /// Squash commit on the current branch + the per-thread branch
    /// preserved (not merged) for archaeology.
    Both,
}

impl BridgeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Squash => "squash",
            Self::Stitches => "stitches",
            Self::Both => "both",
        }
    }
}

impl std::str::FromStr for BridgeMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "squash" => Ok(Self::Squash),
            "stitches" => Ok(Self::Stitches),
            "both" => Ok(Self::Both),
            other => Err(format!(
                "unknown bridge mode '{other}' — pick squash | stitches | both"
            )),
        }
    }
}

/// A registered heddle repo: a directory the operator pointed Heddle at.
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
    /// Granularity of what the bridge commits ([`BridgeMode`]); serde-default
    /// so state files written before this field existed load as `Squash`.
    #[serde(default)]
    pub bridge_mode: BridgeMode,
    pub registered_ms: u64,
    /// The repo's git root commit, captured at registration — the identity
    /// that survives a `mv` when `path` and `id` (a hash of `path`) do not.
    /// `heddle repair` uses it to rebind moved repos to their existing state
    /// instead of silently stranding it. Serde-default: registries written
    /// before this field existed load with `None` and get backfilled on the
    /// next register or repair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_root_commit: Option<String>,
    /// The git remote `heddle sync` talks to, remembered from the first
    /// `heddle sync --remote <name>`. `None` = this repo has never synced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_remote: Option<String>,
    /// When true (explicit opt-in: `heddle sync --auto`), stitch and propose
    /// also sync — sharing lease/thread metadata AND scoped file blobs with
    /// the remote, the same exposure as pushing a branch there.
    #[serde(default)]
    pub auto_sync: bool,
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
/// content hash in the blob store, or [`TOMBSTONE`] for a file the thread
/// deleted (detected against the previous stitch and the thread's base
/// snapshot — a first stitch with neither reference cannot see deletions).
/// Unchanged files dedup for free (same hash, same blob); an unchanged
/// *manifest* creates no new stitch at all.
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
    /// longer pending; [`Heddle::reconcile_parked`] returns it to Active.
    /// `None` on a Proposed thread means no approval is waiting (denied /
    /// withdrawn / mid-gate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    /// Absolute path of this thread's isolated git worktree — where the
    /// holder edits. `None` means in-place (v0.1 behavior): the thread edits
    /// the repo's own tree directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// The stitch snapshotting the fabric state this thread branched from
    /// (its scope, captured when the worktree was created and refreshed by
    /// `rebase`). Present exactly for isolated threads; the three-way merge
    /// rules at land compare head vs THIS vs the live repo tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_stitch: Option<String>,
}

impl Thread {
    /// Where this thread's work happens: its worktree when isolated, else
    /// the repo root.
    pub fn working_dir<'a>(&'a self, repo_path: &'a str) -> &'a str {
        self.worktree.as_deref().unwrap_or(repo_path)
    }
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
    /// Exactly what landing applied to the tree (path → content hash or
    /// [`TOMBSTONE`]). Empty until the weave lands. This is what `heddle sync`
    /// publishes so other machines can replay the same change.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub applied: BTreeMap<String, String>,
}

/// The shared line. `tip` advances only in [`Heddle::land_weave`], whose
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

/// What one peer machine last published about this repo, cached locally by
/// `heddle sync` so `heddle status` can show the whole team without touching
/// the network. Peers' threads and leases stay in THEIR state — this is a
/// read-only view, refreshed on every sync; cross-machine orphans import
/// into local state only through the claims flow in [`sync`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PeerSnapshot {
    pub machine: String,
    /// When the peer published this state (its clock).
    pub ts_ms: u64,
    /// When we fetched it (our clock).
    #[serde(default)]
    pub fetched_ms: u64,
    #[serde(default)]
    pub threads: Vec<Thread>,
    #[serde(default)]
    pub leases: Vec<Lease>,
}

/// Everything Heddle knows about one repo. Persisted as
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
    pub peers: Vec<PeerSnapshot>,
    #[serde(default)]
    pub seq: u64,
}

/// The whole in-memory picture: the repo registry plus per-repo state.
#[derive(Clone, Debug, Default)]
pub struct HeddleState {
    pub repos: Vec<RepoConfig>,
    pub repo_states: std::collections::HashMap<String, RepoState>,
}

/// The engine. One per process (see [`store`]); tests and embedders build
/// isolated ones with [`Heddle::at`]. The std Mutex guards are never held
/// across an await — long work (file walks, verify runs) happens between
/// locked phases.
pub struct Heddle {
    base_override: Option<PathBuf>,
    inner: Mutex<Option<HeddleState>>,
}

/// Pre-rename names (formerly loom-vcs): compatibility aliases so embedders
/// written against `loom::Loom` keep compiling. New code should say
/// [`Heddle`] / [`HeddleState`].
pub type Loom = Heddle;
#[doc(hidden)]
pub type LoomState = HeddleState;

/// The process-wide engine, rooted at the default data dir (`HEDDLE_DATA` env
/// var when set, else `~/.heddle` — resolved fresh on every touch).
pub fn store() -> &'static Heddle {
    static S: std::sync::OnceLock<Heddle> = std::sync::OnceLock::new();
    S.get_or_init(|| Heddle {
        base_override: None,
        inner: Mutex::new(None),
    })
}

/// The default storage root: `$HEDDLE_DATA` when set and non-empty (the
/// pre-rename `$LOOM_DATA` is honored as a silent fallback), else
/// `~/.heddle` (via `$HOME`; falls back to `.heddle` in the current directory
/// when even `HOME` is unset).
///
/// Rename compatibility: when `~/.heddle` does not exist but a pre-rename
/// `~/.loom` does, the existing `~/.loom` is used — with a one-line notice
/// on stderr, once per process. Data is never moved silently; migrate by
/// renaming the directory yourself (`mv ~/.loom ~/.heddle`).
pub fn default_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("HEDDLE_DATA") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    // Silent fallback for the pre-rename env var (formerly loom-vcs).
    if let Ok(dir) = std::env::var("LOOM_DATA") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    match std::env::var("HOME") {
        Ok(home) if !home.trim().is_empty() => {
            let home = PathBuf::from(home);
            let new = home.join(".heddle");
            let old = home.join(".loom");
            if !new.exists() && old.exists() {
                static NOTICE: std::sync::Once = std::sync::Once::new();
                NOTICE.call_once(|| {
                    eprintln!(
                        "heddle: using existing ~/.loom data dir (pre-rename); \
                         `mv ~/.loom ~/.heddle` to migrate"
                    );
                });
                return old;
            }
            new
        }
        _ => PathBuf::from(".heddle"),
    }
}

/// How a new lease's thread relates to the working tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsolationMode {
    /// Isolated when the repo is a git repo (falling back to in-place, with
    /// the reason noted on the thread, if worktree setup fails); in-place
    /// otherwise. The default.
    Auto,
    /// Require a worktree; the declaration FAILS if one cannot be created.
    Isolated,
    /// v0.1 behavior: the thread edits the repo tree directly.
    InPlace,
}

/// What `declare_lease` hands back: the lease, its thread, and any toe-step
/// warnings — declaration succeeded either way. `working_dir` is where the
/// holder must edit: the thread's own worktree when isolated, else the repo
/// root.
#[derive(Clone, Debug)]
pub struct DeclareOutcome {
    pub lease: Lease,
    pub thread: Thread,
    pub toe_steps: Vec<ToeStep>,
    pub working_dir: String,
}

/// What `rebase_thread` hands back: which files were fast-forwarded from
/// the fabric into the worktree, and which conflicted (changed in BOTH the
/// fabric and the thread — the worktree keeps the thread's version, and the
/// holder is told to review before re-proposing). `approval_id` is any
/// parked approval that was gating the thread, handed back so an embedding
/// host can resolve it.
#[derive(Clone, Debug)]
pub struct RebaseOutcome {
    pub thread: Thread,
    pub fast_forwarded: Vec<String>,
    pub conflicts: Vec<String>,
    pub approval_id: Option<String>,
}

/// What `clean_worktrees` hands back: worktrees removed (thread id, path),
/// and worktrees kept with the honest reason.
#[derive(Clone, Debug, Default)]
pub struct CleanReport {
    pub removed: Vec<(String, String)>,
    pub skipped: Vec<(String, String)>,
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
/// speak honestly about what changed. `stitches` is the thread's stitch
/// chain oldest-first (what `stitches`/`both` bridge modes replay),
/// `base_manifest` the thread's base snapshot (empty for in-place threads),
/// and `objects_dir` the repo's blob store — everything the bridge needs
/// without touching engine state again.
#[derive(Clone, Debug)]
pub struct LandOutcome {
    pub repo: RepoConfig,
    pub thread: Thread,
    pub weave: Weave,
    pub criteria: Vec<String>,
    pub files_applied: usize,
    pub stitches: Vec<Stitch>,
    pub base_manifest: BTreeMap<String, String>,
    pub objects_dir: PathBuf,
}

/// What `export_thread` hands back: the per-thread branch written (or
/// refreshed) and how many stitch commits it carries.
#[derive(Clone, Debug)]
pub struct ExportOutcome {
    pub thread: Thread,
    pub branch: String,
    pub commits: usize,
}

impl Heddle {
    /// An isolated engine rooted at `base` — tests, and embedders that
    /// manage their own storage root; everyone else uses [`store`].
    pub fn at(base: PathBuf) -> Self {
        Heddle {
            base_override: Some(base),
            inner: Mutex::new(None),
        }
    }

    /// Storage root: the override when constructed with [`Heddle::at`], else
    /// [`default_data_dir`] resolved fresh each call (so `HEDDLE_DATA` can be
    /// pointed elsewhere by a test rig between calls).
    pub fn base(&self) -> PathBuf {
        self.base_override.clone().unwrap_or_else(default_data_dir)
    }

    /// Run `f` against the live state, loading on first touch and persisting
    /// whatever it changed. Guards never cross an await; anything slow
    /// (file walks, verify commands) runs OUTSIDE this closure.
    fn with<T>(&self, f: impl FnOnce(&mut HeddleState, &PathBuf) -> T) -> T {
        let base = self.base();
        let mut guard = self.inner.lock().expect("heddle store lock poisoned");
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
        *self.inner.lock().expect("heddle store lock poisoned") = None;
    }

    // -- repo registration --------------------------------------------------

    /// Register a directory as a heddle repo. Idempotent per canonical path:
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
        // Captured outside the state lock — this shells out to git.
        let root_commit = worktree::root_commit(&canon);
        let cmd = verify_cmd
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| DEFAULT_VERIFY_CMD.to_string());
        self.with(|s, base| {
            if let Some(existing) = s.repos.iter_mut().find(|r| r.id == id) {
                existing.verify_cmd = cmd.clone();
                existing.git_bridge = git_bridge;
                // Backfill for registries written before identity was kept.
                if existing.git_root_commit.is_none() {
                    existing.git_root_commit = root_commit.clone();
                }
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
                bridge_mode: BridgeMode::default(),
                registered_ms: now_ms(),
                git_root_commit: root_commit.clone(),
                sync_remote: None,
                auto_sync: false,
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

    /// Set how the git bridge projects landed weaves into git history
    /// (`heddle init --bridge-mode <mode>` / `heddle config --bridge-mode
    /// <mode>`). Re-registering a repo keeps the stored mode.
    pub fn set_bridge_mode(&self, repo_id: &str, mode: BridgeMode) -> Result<RepoConfig, String> {
        self.with(|s, base| {
            let repo = s
                .repos
                .iter_mut()
                .find(|r| r.id == repo_id)
                .ok_or_else(|| format!("no registered repo with id {repo_id}"))?;
            repo.bridge_mode = mode;
            let repo = repo.clone();
            store::append_event(
                base,
                repo_id,
                &serde_json::json!({
                    "ts_ms": now_ms(), "kind": "bridge_mode_set", "mode": mode.as_str(),
                }),
            );
            Ok(repo)
        })
    }

    /// Remember this repo's sync remote and auto-sync opt-in (set by
    /// `heddle sync --remote <name>` / `--auto`). Auto-sync is explicit
    /// consent to share lease/thread metadata AND scoped file blobs with
    /// that remote on every stitch/propose — the same exposure as pushing a
    /// branch there.
    pub fn set_sync(
        &self,
        repo_id: &str,
        remote: Option<String>,
        auto: Option<bool>,
    ) -> Result<RepoConfig, String> {
        self.with(|s, base| {
            let repo = s
                .repos
                .iter_mut()
                .find(|r| r.id == repo_id)
                .ok_or_else(|| format!("no registered repo with id {repo_id}"))?;
            if let Some(r) = remote {
                repo.sync_remote = Some(r);
            }
            if let Some(a) = auto {
                repo.auto_sync = a;
            }
            let repo = repo.clone();
            store::append_event(
                base,
                repo_id,
                &serde_json::json!({
                    "ts_ms": now_ms(), "kind": "sync_configured",
                    "remote": repo.sync_remote, "auto": repo.auto_sync,
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

    /// The registered repo and thread whose isolated WORKTREE contains
    /// `path` (the path itself or anything under it). Worktrees live under
    /// the heddle data dir — outside every registered repo tree — so
    /// [`Self::repo_containing`] cannot see them; this is how "I am standing
    /// in this thread's worktree" becomes "I mean THIS thread". A worktree
    /// that no longer exists on disk (woven, cleaned) never matches.
    pub fn thread_containing(&self, path: &str) -> Option<(RepoConfig, Thread)> {
        let canon = std::fs::canonicalize(path).ok()?;
        let snap = self.snapshot();
        for repo in &snap.repos {
            let Some(rs) = snap.repo_states.get(&repo.id) else {
                continue;
            };
            for t in &rs.threads {
                let Some(wt) = &t.worktree else { continue };
                let Ok(wt_canon) = std::fs::canonicalize(wt) else {
                    continue;
                };
                if canon.starts_with(&wt_canon) {
                    return Some((repo.clone(), t.clone()));
                }
            }
        }
        None
    }

    /// Bare-verb targeting: the ONE thread a flag-less `stitch` / `propose` /
    /// `rebase` / `export` means, resolved from `cwd`. The rule, in order:
    ///
    /// 1. Standing inside a thread's isolated worktree names THAT thread —
    ///    cwd is unambiguous intent.
    /// 2. Otherwise, a repo with exactly ONE live thread is unambiguous.
    /// 3. Otherwise REFUSE with the list of live threads, never guess.
    ///
    /// Rule 3 is why the shared solo pointer is NOT consulted here: several
    /// agents share one data dir, so its last-lease-wins slot may name a
    /// DIFFERENT agent's thread — guessing from it is exactly how stitches
    /// and exports land on a stranger's work-line.
    pub fn resolve_bare_target(&self, cwd: &str) -> Result<(RepoConfig, Thread), String> {
        if let Some(found) = self.thread_containing(cwd) {
            return Ok(found);
        }
        let repo = self.repo_containing(cwd).ok_or_else(|| {
            "no registered repo contains this directory — run `heddle init` here first"
                .to_string()
        })?;
        let snap = self.snapshot();
        let rs = snap.repo_states.get(&repo.id).cloned().unwrap_or_default();
        let live: Vec<&Thread> = rs.threads.iter().filter(|t| t.status.is_live()).collect();
        match live.len() {
            1 => Ok((repo, live[0].clone())),
            0 => Err(
                "no live thread in this repo — `heddle lease \"<goal>\" <scope...>` first \
                 (or `heddle adopt <thread-id>`)"
                    .to_string(),
            ),
            n => {
                let mut msg = format!(
                    "{n} live threads in this repo — refusing to guess which one you mean \
                     (a wrong guess writes onto another agent's work):\n"
                );
                for t in &live {
                    msg.push_str(&format!(
                        "  --lease {}  thread {} [{:?}] — {}\n",
                        t.lease_id.as_deref().unwrap_or("(none)"),
                        t.id,
                        t.status,
                        t.goal
                    ));
                }
                msg.push_str(
                    "say which: pass --lease <id> (or --thread <id>; lease_id/thread_id over \
                     MCP), or run from inside the thread's worktree",
                );
                Err(msg)
            }
        }
    }

    // -- repair (moved repos) -----------------------------------------------

    /// Rebind registered repos whose directory has MOVED — Heddle's answer to
    /// `git worktree repair`.
    ///
    /// A repo's id is a hash of its canonical path, so `mv` used to strand
    /// everything: the registry pointed at a path that no longer existed, a
    /// re-`init` at the new path minted a fresh empty id, and the old state
    /// (threads, stitches, blobs) plus its git worktree registrations were
    /// left behind with nothing able to reach them.
    ///
    /// Repair looks for each missing repo under `scan_roots` and rebinds it:
    /// the state dir is renamed to the new id, thread/fabric back-references
    /// and worktree paths are rewritten, and `git worktree repair` re-points
    /// git's own metadata. Matching is by **root commit** (exact) and falls
    /// back to a unique **basename** match, which the report labels as such —
    /// an ambiguous or unmatched repo is reported, never guessed at.
    ///
    /// `dry_run` computes the whole plan and touches nothing.
    pub fn repair_repos(&self, scan_roots: &[String], dry_run: bool) -> RepairReport {
        let mut report = RepairReport::default();

        // Phase 1 (unlocked): who is missing, and what are the candidates?
        // Everything here is filesystem + git work, so it stays out of the
        // state lock.
        let missing: Vec<RepoConfig> = self.with(|s, _| {
            s.repos
                .iter()
                .filter(|r| !Path::new(&r.path).is_dir())
                .cloned()
                .collect()
        });
        if missing.is_empty() {
            return report;
        }
        let known: Vec<String> = self.with(|s, _| s.repos.iter().map(|r| r.path.clone()).collect());

        // Default search roots: the nearest existing ancestor of each missing
        // path. A repo moved into a subfolder of where it used to live (the
        // common tidy-up) is found without the caller naming a root.
        let mut roots: Vec<PathBuf> = scan_roots.iter().map(PathBuf::from).collect();
        if roots.is_empty() {
            for r in &missing {
                let mut p = Path::new(&r.path);
                while let Some(parent) = p.parent() {
                    if parent.is_dir() {
                        if !roots.contains(&parent.to_path_buf()) {
                            roots.push(parent.to_path_buf());
                        }
                        break;
                    }
                    p = parent;
                }
            }
        }
        let candidates = scan_for_git_repos(&roots, REPAIR_SCAN_DEPTH, &known);

        let mut plans: Vec<(RepoConfig, String, &'static str)> = Vec::new();
        let mut claimed: Vec<String> = Vec::new();
        for r in &missing {
            let old_base = Path::new(&r.path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            // Exact: same root commit. Only ever one repo can match.
            let by_commit: Vec<&Candidate> = match &r.git_root_commit {
                Some(rc) => candidates
                    .iter()
                    .filter(|c| c.root_commit.as_deref() == Some(rc.as_str()))
                    .collect(),
                None => Vec::new(),
            };
            let (hits, how): (Vec<&Candidate>, &'static str) = if !by_commit.is_empty() {
                // Several repos can share a root commit (a clone, or a fork
                // that never rewrote history). Identity narrows the field;
                // the name settles it — and if it doesn't, we say so below
                // rather than pick one.
                let named: Vec<&Candidate> = by_commit
                    .iter()
                    .copied()
                    .filter(|c| c.basename == old_base)
                    .collect();
                if by_commit.len() > 1 && named.len() == 1 {
                    (named, "root commit")
                } else {
                    (by_commit, "root commit")
                }
            } else {
                (
                    candidates
                        .iter()
                        .filter(|c| c.basename == old_base)
                        .collect(),
                    "name",
                )
            };
            let hits: Vec<&&Candidate> = hits
                .iter()
                .filter(|c| !claimed.contains(&c.path))
                .collect();
            match hits.len() {
                0 => report
                    .unmatched
                    .push((r.path.clone(), "no candidate found under the search roots".into())),
                1 => {
                    let dest = hits[0].path.clone();
                    claimed.push(dest.clone());
                    plans.push((r.clone(), dest, how));
                }
                n => report.unmatched.push((
                    r.path.clone(),
                    format!("{n} candidates matched by {how} — rerun with an explicit search root"),
                )),
            }
        }

        if dry_run {
            for (r, dest, how) in plans {
                report.rebound.push(Rebind {
                    old_path: r.path,
                    new_path: dest,
                    old_id: r.id,
                    new_id: String::new(),
                    matched_by: how.to_string(),
                });
            }
            return report;
        }

        // Phase 2: rebind each plan — state dir rename, then state rewrite.
        for (repo, dest, how) in plans {
            let new_id = repo_id_for(&dest);
            if new_id != repo.id {
                let base = self.base();
                let from = base.join(&repo.id);
                let to = base.join(&new_id);
                if from.is_dir() {
                    if to.exists() {
                        report.unmatched.push((
                            repo.path.clone(),
                            format!("{} already has heddle state — refusing to overwrite", dest),
                        ));
                        continue;
                    }
                    if let Err(e) = std::fs::rename(&from, &to) {
                        report
                            .unmatched
                            .push((repo.path.clone(), format!("cannot move state dir: {e}")));
                        continue;
                    }
                }
            }
            let root_commit = worktree::root_commit(Path::new(&dest));
            let old_wt_dir = store::worktrees_dir(&self.base(), &repo.id)
                .to_string_lossy()
                .to_string();
            let new_wt_dir = store::worktrees_dir(&self.base(), &new_id)
                .to_string_lossy()
                .to_string();

            self.with(|s, base| {
                if let Some(rc) = s.repos.iter_mut().find(|r| r.id == repo.id) {
                    rc.id = new_id.clone();
                    rc.path = dest.clone();
                    if rc.git_root_commit.is_none() {
                        rc.git_root_commit = root_commit.clone();
                    }
                }
                if let Some(mut rs) = s.repo_states.remove(&repo.id) {
                    rs.fabric.repo_id = new_id.clone();
                    for t in rs.threads.iter_mut() {
                        t.repo_id = new_id.clone();
                        if let Some(wt) = t.worktree.as_mut() {
                            if let Some(rest) = wt.strip_prefix(&old_wt_dir) {
                                *wt = format!("{new_wt_dir}{rest}");
                            }
                        }
                    }
                    s.repo_states.insert(new_id.clone(), rs);
                }
                store::append_event(
                    base,
                    &new_id,
                    &serde_json::json!({
                        "ts_ms": now_ms(), "kind": "repo_repaired",
                        "from": repo.path, "to": dest, "matched_by": how,
                    }),
                );
            });

            // Git's own metadata last: re-point every worktree that moved with
            // the state dir, THEN forget the ones whose directory is truly
            // gone. Order matters — pruning first (or repairing without the
            // new paths) deletes the administrative entry of a worktree whose
            // checkout is sitting right there, intact.
            let moved_wts = worktree::dirs_in(Path::new(&new_wt_dir));
            let _ = worktree::repair(Path::new(&dest), &moved_wts);
            let _ = worktree::prune(Path::new(&dest));

            report.rebound.push(Rebind {
                old_path: repo.path,
                new_path: dest,
                old_id: repo.id,
                new_id,
                matched_by: how.to_string(),
            });
        }
        report
    }

    // -- leases -------------------------------------------------------------

    /// Declare an intent lease (creates its thread). Scope globs are
    /// validated; overlap with live leases is detected and returned as
    /// toe-step warnings — the declaration still succeeds.
    ///
    /// Isolation is [`IsolationMode::Auto`]: when the repo is a git repo the
    /// thread gets its own worktree (edit THERE — `working_dir` on the
    /// outcome); otherwise it works in place. Use [`Heddle::declare_lease_mode`]
    /// to pick explicitly.
    pub fn declare_lease(
        &self,
        repo_id: &str,
        holder: &str,
        goal: &str,
        scope: Vec<String>,
        criteria: Vec<String>,
        ttl_ms: Option<u64>,
    ) -> Result<DeclareOutcome, String> {
        self.declare_lease_mode(repo_id, holder, goal, scope, criteria, ttl_ms, IsolationMode::Auto)
    }

    /// [`Heddle::declare_lease`] with the isolation mode explicit. Blocking
    /// when isolation applies (a `git worktree add` plus a scope walk to
    /// snapshot the thread's base); call from a blocking-task helper on
    /// async paths.
    #[allow(clippy::too_many_arguments)]
    pub fn declare_lease_mode(
        &self,
        repo_id: &str,
        holder: &str,
        goal: &str,
        scope: Vec<String>,
        criteria: Vec<String>,
        ttl_ms: Option<u64>,
        mode: IsolationMode,
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
        // Phase 1 (locked): validate, detect toe-steps, insert lease+thread.
        let (mut out, repo) = self.with(|s, base| {
            let repo = s
                .repos
                .iter()
                .find(|r| r.id == repo_id)
                .cloned()
                .ok_or_else(|| format!("no registered repo with id {repo_id}"))?;
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
                worktree: None,
                base_stitch: None,
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
            let working_dir = repo.path.clone();
            Ok::<_, String>((
                DeclareOutcome {
                    lease,
                    thread,
                    toe_steps,
                    working_dir,
                },
                repo,
            ))
        })?;
        let isolate = match mode {
            IsolationMode::InPlace => false,
            IsolationMode::Isolated => true,
            IsolationMode::Auto => worktree::is_git_repo(std::path::Path::new(&repo.path)),
        };
        if !isolate {
            return Ok(out);
        }
        // Phase 2 (unlocked): add the worktree (detached at git HEAD), then
        // ALIGN its scope to the repo's live tree — the fabric may be ahead
        // of HEAD (landed weaves sit in the working tree until committed).
        // What the repo has right now becomes the thread's base: the state
        // the merge rules at land compare against.
        let wt_dir = store::worktrees_dir(&self.base(), &repo.id).join(&out.thread.id);
        let objects = store::objects_dir(&self.base(), &repo.id);
        let setup = worktree::add(std::path::Path::new(&repo.path), &wt_dir).and_then(|()| {
            weave::align_tree(
                std::path::Path::new(&repo.path),
                &wt_dir,
                &out.lease.scope,
                &objects,
            )
        });
        // Phase 3 (locked): record isolation — or fall back / roll back.
        match setup {
            Ok(captured) => {
                let wt_str = wt_dir.to_string_lossy().to_string();
                let updated = self.with(|s, base| {
                    let rs = s.repo_states.get_mut(repo_id)?;
                    rs.seq += 1;
                    let base_stitch = Stitch {
                        id: format!("stitch-{}-{}", now_ms(), rs.seq),
                        thread_id: out.thread.id.clone(),
                        parent: None,
                        files: captured.manifest.clone(),
                        ts_ms: now_ms(),
                    };
                    rs.stitches.push(base_stitch.clone());
                    let t = rs.threads.iter_mut().find(|t| t.id == out.thread.id)?;
                    t.worktree = Some(wt_str.clone());
                    t.base_stitch = Some(base_stitch.id.clone());
                    store::append_event(
                        base,
                        repo_id,
                        &serde_json::json!({
                            "ts_ms": now_ms(), "kind": "worktree_created",
                            "thread": t.id, "path": wt_str, "base_stitch": base_stitch.id,
                        }),
                    );
                    Some(t.clone())
                });
                if let Some(t) = updated {
                    out.thread = t;
                    out.working_dir = wt_dir.to_string_lossy().to_string();
                }
                Ok(out)
            }
            Err(e) => {
                let _ = worktree::remove(std::path::Path::new(&repo.path), &wt_dir);
                if mode == IsolationMode::Isolated {
                    // The caller demanded isolation: undo the declaration.
                    self.with(|s, base| {
                        if let Some(rs) = s.repo_states.get_mut(repo_id) {
                            rs.leases.retain(|l| l.id != out.lease.id);
                            rs.threads.retain(|t| t.id != out.thread.id);
                            rs.toe_steps.retain(|t| t.lease_a != out.lease.id);
                        }
                        store::append_event(
                            base,
                            repo_id,
                            &serde_json::json!({
                                "ts_ms": now_ms(), "kind": "lease_rolled_back",
                                "lease": out.lease.id, "reason": e,
                            }),
                        );
                    });
                    return Err(format!(
                        "isolated lease refused — worktree setup failed: {e}"
                    ));
                }
                // Auto: degrade honestly to in-place, reason on the thread.
                let note = format!("isolation unavailable ({}) — working in place", cap(&e, 160));
                self.with(|s, _| {
                    if let Some(rs) = s.repo_states.get_mut(repo_id) {
                        if let Some(t) = rs.threads.iter_mut().find(|t| t.id == out.thread.id) {
                            t.note = note.clone();
                        }
                    }
                });
                out.thread.note = note;
                Ok(out)
            }
        }
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
    /// lease scope under the thread's working directory — its isolated
    /// worktree when it has one, else the repo root. The server reads the
    /// files itself — callers never upload content. Files present in the
    /// previous stitch (or the thread's base) but gone from disk are
    /// recorded as [`TOMBSTONE`] deletions. Blocking (file walk + hashing);
    /// call from `spawn_blocking` on async paths.
    pub fn stitch(&self, lease_id: &str) -> Result<StitchOutcome, String> {
        let now = now_ms();
        // Phase 1 (locked): resolve lease → capture root + scope + parent.
        let (repo, lease, thread_id, parent, root, base_manifest) = self.with(|s, base| {
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
            let base_manifest = thread
                .base_stitch
                .as_ref()
                .and_then(|id| rs.stitches.iter().find(|st| st.id == *id))
                .map(|st| st.files.clone());
            let repo = s
                .repos
                .iter()
                .find(|r| r.id == repo_id)
                .cloned()
                .ok_or_else(|| format!("repo {repo_id} vanished from the registry"))?;
            let root = thread.working_dir(&repo.path).to_string();
            if thread.worktree.is_some() && !std::path::Path::new(&root).is_dir() {
                return Err(format!(
                    "thread {}'s worktree is missing ({root}) — it was removed outside heddle; \
                     re-lease to start a fresh one",
                    thread.id
                ));
            }
            Ok((repo, lease.clone(), lease.thread_id.clone(), parent, root, base_manifest))
        })?;
        // Phase 2 (unlocked): walk + hash + write blobs.
        let objects = store::objects_dir(&self.base(), &repo.id);
        let captured = weave::capture_scope(std::path::Path::new(&root), &lease.scope, &objects)?;
        // Deletions: anything the previous stitch or the base knew about
        // that no longer exists on disk becomes a tombstone. A parent's
        // tombstone carries forward until the file reappears.
        let mut manifest = captured.manifest.clone();
        let reference = parent
            .as_ref()
            .map(|p| p.files.clone())
            .or_else(|| base_manifest.clone());
        if let Some(reference) = &reference {
            for rel in reference.keys() {
                manifest
                    .entry(rel.clone())
                    .or_insert_with(|| TOMBSTONE.to_string());
            }
        }
        // Phase 3 (locked): record the stitch (or report "unchanged").
        self.with(|s, base| {
            let rs = s.repo_states.get_mut(&repo.id).expect("repo state exists");
            // Unchanged vs the parent — or, for a first stitch of an
            // isolated thread, vs its base snapshot: no new stitch.
            if let Some(p) = &parent {
                if p.files == manifest {
                    return Ok(StitchOutcome {
                        stitch: p.clone(),
                        unchanged: true,
                        skipped: captured.skipped,
                        lease: lease.clone(),
                    });
                }
            } else if let Some(b) = &base_manifest {
                if *b == manifest {
                    let base_id = rs
                        .threads
                        .iter()
                        .find(|t| t.id == thread_id)
                        .and_then(|t| t.base_stitch.clone())
                        .and_then(|id| rs.stitches.iter().find(|st| st.id == id))
                        .cloned();
                    if let Some(bs) = base_id {
                        return Ok(StitchOutcome {
                            stitch: bs,
                            unchanged: true,
                            skipped: captured.skipped,
                            lease: lease.clone(),
                        });
                    }
                }
            }
            rs.seq += 1;
            let stitch = Stitch {
                id: format!("stitch-{now}-{}", rs.seq),
                thread_id: thread_id.clone(),
                parent: parent.as_ref().map(|p| p.id.clone()),
                files: manifest.clone(),
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
    ///
    /// The dead holder's worktree is handed over as-is (its path rides on
    /// the returned thread). An orphan WITHOUT a worktree — an in-place
    /// thread, or one imported from another machine — gets a fresh one on a
    /// git repo: worktree at HEAD, base snapshotted, the orphan's head
    /// stitch overlaid, so the adopter continues the work isolated. Falls
    /// back to in-place (noted) when that setup fails.
    pub fn adopt(&self, thread_id: &str, holder: &str) -> Result<(Thread, Lease), String> {
        let (thread, lease) = self.adopt_locked(thread_id, holder)?;
        if thread.worktree.is_some() {
            return Ok((thread, lease));
        }
        // Post-step (unlocked): try to give the adopter isolation.
        let repo = self.with(|s, _| {
            s.repos
                .iter()
                .find(|r| r.id == thread.repo_id)
                .cloned()
        });
        let Some(repo) = repo else { return Ok((thread, lease)) };
        if !worktree::is_git_repo(std::path::Path::new(&repo.path)) {
            return Ok((thread, lease));
        }
        let wt_dir = store::worktrees_dir(&self.base(), &repo.id).join(&thread.id);
        let objects = store::objects_dir(&self.base(), &repo.id);
        let head_manifest = self.with(|s, _| {
            s.repo_states.get(&repo.id).and_then(|rs| {
                thread
                    .head_stitch
                    .as_ref()
                    .and_then(|id| rs.stitches.iter().find(|st| st.id == *id))
                    .map(|st| st.files.clone())
            })
        });
        let setup = worktree::add(std::path::Path::new(&repo.path), &wt_dir)
            .and_then(|()| {
                // Base = the repo's LIVE tree (the fabric), not git HEAD.
                weave::align_tree(std::path::Path::new(&repo.path), &wt_dir, &lease.scope, &objects)
            })
            .and_then(|base_cap| {
                if let Some(m) = &head_manifest {
                    weave::apply_overlay(&wt_dir, m, &objects)?;
                }
                Ok(base_cap)
            });
        match setup {
            Ok(base_cap) => {
                let wt_str = wt_dir.to_string_lossy().to_string();
                let updated = self.with(|s, base| {
                    let rs = s.repo_states.get_mut(&repo.id)?;
                    rs.seq += 1;
                    let base_stitch = Stitch {
                        id: format!("stitch-{}-{}", now_ms(), rs.seq),
                        thread_id: thread.id.clone(),
                        parent: None,
                        files: base_cap.manifest.clone(),
                        ts_ms: now_ms(),
                    };
                    rs.stitches.push(base_stitch.clone());
                    let t = rs.threads.iter_mut().find(|t| t.id == thread.id)?;
                    t.worktree = Some(wt_str.clone());
                    t.base_stitch = Some(base_stitch.id.clone());
                    store::append_event(
                        base,
                        &repo.id,
                        &serde_json::json!({
                            "ts_ms": now_ms(), "kind": "worktree_created",
                            "thread": t.id, "path": wt_str, "on": "adopt",
                        }),
                    );
                    Some(t.clone())
                });
                Ok((updated.unwrap_or(thread), lease))
            }
            Err(e) => {
                let _ = worktree::remove(std::path::Path::new(&repo.path), &wt_dir);
                let note =
                    format!("isolation unavailable ({}) — working in place", cap(&e, 160));
                let updated = self.with(|s, _| {
                    s.repo_states.get_mut(&repo.id).and_then(|rs| {
                        let t = rs.threads.iter_mut().find(|t| t.id == thread.id)?;
                        t.note = note.clone();
                        Some(t.clone())
                    })
                });
                Ok((updated.unwrap_or(thread), lease))
            }
        }
    }

    fn adopt_locked(&self, thread_id: &str, holder: &str) -> Result<(Thread, Lease), String> {
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
    /// touches the real working tree — landing is [`Heddle::land_weave`],
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
            let base_stitch = thread.base_stitch.clone();
            let manifest = rs
                .stitches
                .iter()
                .find(|st| st.id == head)
                .map(|st| st.files.clone())
                .ok_or_else(|| format!("head stitch {head} not found"))?;
            thread.status = ThreadStatus::Proposed;
            thread.note = "verify running".into();
            // Isolated threads verify their DELTA vs base overlaid on the
            // live repo tree (the fabric's materialization) — exactly the
            // state a land would produce. In-place threads overlay the whole
            // manifest (v0.1 behavior; their tree IS the repo tree).
            let manifest = match base_stitch
                .and_then(|id| rs.stitches.iter().find(|st| st.id == id))
                .map(|st| &st.files)
            {
                Some(base) => manifest
                    .into_iter()
                    .filter(|(rel, h)| base.get(rel) != Some(h))
                    .collect(),
                None => manifest,
            };
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
                applied: BTreeMap::new(),
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
    /// Land a weave, then retire the thread's worktree.
    ///
    /// A woven thread is finished, so its isolated worktree is garbage the
    /// moment the weave lands. Leaving that to a manual `heddle clean` meant
    /// worktrees piled up for as long as nobody remembered to run it, each
    /// one a full checkout on disk and a live registration in the repo's git
    /// metadata. Cleanup rides the event that makes it correct.
    ///
    /// The sweep is best-effort and never fails a landed weave: `clean_worktrees`
    /// keeps anything with uncaptured divergence, and a removal error is
    /// reported by `heddle clean`, not raised here — the weave HAS landed and
    /// saying otherwise would be a lie.
    pub fn land_weave(&self, weave_id: &str) -> Result<LandOutcome, String> {
        let out = self.land_weave_inner(weave_id)?;
        let _ = self.clean_worktrees(&out.repo.id);
        Ok(out)
    }

    fn land_weave_inner(&self, weave_id: &str) -> Result<LandOutcome, String> {
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
            let (head_id, base_id) = rs
                .threads
                .iter()
                .find(|t| t.id == weave.thread_id)
                .map(|t| (t.head_stitch.clone(), t.base_stitch.clone()))
                .ok_or_else(|| format!("thread {} vanished", weave.thread_id))?;
            let manifest = head_id
                .as_ref()
                .and_then(|id| rs.stitches.iter().find(|st| st.id == *id))
                .map(|st| st.files.clone())
                .ok_or_else(|| "thread's stitches are gone; cannot apply".to_string())?;
            let base_manifest = base_id
                .and_then(|id| rs.stitches.iter().find(|st| st.id == id))
                .map(|st| st.files.clone());
            // The stitch chain oldest-first, for bridge modes that replay
            // checkpoints. Pruned parents just end the walk — honest partial
            // history beats none.
            let stitch_chain = stitch_chain(rs, head_id.as_ref());
            // Isolated threads MERGE at file level: only files the thread
            // actually changed vs its base are applied, and a file that
            // moved in BOTH the fabric and the thread refuses the land —
            // never silently overwriting either side. In-place threads
            // (no base) apply the whole manifest, v0.1 behavior.
            let (apply_manifest, conflicts) = weave::merge_plan(
                std::path::Path::new(&repo.path),
                &manifest,
                base_manifest.as_ref(),
            );
            if !conflicts.is_empty() {
                let list = cap(&conflicts.join(", "), MAX_GOAL_CHARS);
                let note = format!(
                    "fabric moved under you on {list} — `heddle rebase`, then re-propose"
                );
                if let Some(t) = rs.threads.iter_mut().find(|t| t.id == weave.thread_id) {
                    t.note = note.clone();
                }
                store::append_event(
                    base,
                    &repo_id,
                    &serde_json::json!({
                        "ts_ms": now, "kind": "weave_conflict", "weave": weave.id,
                        "thread": weave.thread_id, "files": conflicts,
                    }),
                );
                return Err(format!(
                    "fabric moved under you on {list} — rebase the thread \
                     (`heddle rebase`) and re-propose"
                ));
            }
            let objects = store::objects_dir(base, &repo_id);
            let applied =
                weave::apply_overlay(std::path::Path::new(&repo.path), &apply_manifest, &objects)?;
            let mut weave = weave;
            weave.applied = apply_manifest;
            if let Some(w) = rs.weaves.iter_mut().find(|w| w.id == weave_id) {
                w.applied = weave.applied.clone();
            }
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
                stitches: stitch_chain,
                base_manifest: base_manifest.unwrap_or_default(),
                objects_dir: objects,
            })
        })
    }

    /// Record an operator Deny: the thread stays Proposed (its green verify
    /// still stands) with the denial noted, and the parked approval no longer
    /// gates it (`approval_id` clears — the recoverable state [`Heddle::withdraw`]
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
    /// makes a lapsed approval *detectable*: see [`Heddle::reconcile_parked`].
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

    // -- rebase & worktree hygiene ------------------------------------------

    /// Refresh an isolated thread's worktree from the fabric tip (the live
    /// repo tree). File-level, three-way against the thread's base:
    ///
    /// * **fabric-only** changes are fast-forwarded into the worktree
    ///   (copies — and deletions, when the fabric deleted a file the thread
    ///   never touched);
    /// * **thread-only** changes are kept;
    /// * files changed in **both** keep the THREAD's version and come back
    ///   as `conflicts` — the holder is told to review them against the
    ///   repo tree before re-proposing. Nothing merges silently.
    ///
    /// The base is re-snapshotted to the fabric's current state and the head
    /// stitch re-captured, so the next propose/land measures purely against
    /// the new base. A Proposed thread returns to Active (its old verify is
    /// moot); any parked approval id is handed back for the host to resolve.
    /// Blocking (two scope walks); use a blocking-task helper on async paths.
    pub fn rebase_thread(&self, thread_id: &str) -> Result<RebaseOutcome, String> {
        let now = now_ms();
        // Phase 1 (locked): resolve thread → worktree, scope, base manifest.
        let (repo, wt_path, scope, base_manifest, old_head) = self.with(|s, base| {
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
                .ok_or_else(|| format!("no thread with id {thread_id}"))?;
            if !thread.status.is_live() {
                return Err(format!(
                    "thread {} is {:?} — only a live thread can rebase (adopt an orphan first)",
                    thread_id, thread.status
                ));
            }
            let wt = thread.worktree.clone().ok_or_else(|| {
                "this thread works in place — it follows the fabric directly; nothing to rebase"
                    .to_string()
            })?;
            if !std::path::Path::new(&wt).is_dir() {
                return Err(format!("worktree {wt} is missing — re-lease to start fresh"));
            }
            let scope = thread
                .lease_id
                .as_ref()
                .and_then(|lid| rs.leases.iter().find(|l| l.id == *lid))
                .map(|l| l.scope.clone())
                .ok_or_else(|| format!("thread {thread_id} lost its lease record"))?;
            let base_manifest = thread
                .base_stitch
                .as_ref()
                .and_then(|id| rs.stitches.iter().find(|st| st.id == *id))
                .map(|st| st.files.clone())
                .unwrap_or_default();
            let old_head = thread
                .head_stitch
                .as_ref()
                .and_then(|id| rs.stitches.iter().find(|st| st.id == *id))
                .cloned();
            Ok((repo, PathBuf::from(&wt), scope, base_manifest, old_head))
        })?;
        // Phase 2 (unlocked): snapshot both trees, then walk the union.
        let objects = store::objects_dir(&self.base(), &repo.id);
        let wt_cap = weave::capture_scope(&wt_path, &scope, &objects)?;
        let repo_cap =
            weave::capture_scope(std::path::Path::new(&repo.path), &scope, &objects)?;
        // Effective value per rel: the captured hash, or a tombstone when
        // the base knew the file and it is gone now.
        let eff = |m: &BTreeMap<String, String>, rel: &str| -> Option<String> {
            m.get(rel).cloned().or_else(|| {
                base_manifest
                    .contains_key(rel)
                    .then(|| TOMBSTONE.to_string())
            })
        };
        let mut rels: Vec<String> = base_manifest
            .keys()
            .chain(wt_cap.manifest.keys())
            .chain(repo_cap.manifest.keys())
            .cloned()
            .collect();
        rels.sort();
        rels.dedup();
        // The next head manifest starts as "worktree now + tombstones for
        // base files the thread deleted", then fast-forwards adjust it.
        let mut final_head = wt_cap.manifest.clone();
        for rel in base_manifest.keys() {
            final_head
                .entry(rel.clone())
                .or_insert_with(|| TOMBSTONE.to_string());
        }
        let mut fast_forwarded = Vec::new();
        let mut conflicts = Vec::new();
        for rel in &rels {
            let wt_h = eff(&wt_cap.manifest, rel);
            let repo_h = eff(&repo_cap.manifest, rel);
            let base_h = base_manifest.get(rel).cloned();
            let thread_changed = wt_h != base_h;
            let fabric_changed = repo_h != base_h;
            if !thread_changed && fabric_changed {
                match repo_cap.manifest.get(rel) {
                    Some(h) => {
                        // Fabric edited/added a file the thread never
                        // touched: copy it into the worktree.
                        let bytes = store::read_blob(&objects, h)?;
                        let dest = weave::safe_join(&wt_path, rel)?;
                        if let Some(dir) = dest.parent() {
                            std::fs::create_dir_all(dir)
                                .map_err(|e| format!("rebase mkdir {rel}: {e}"))?;
                        }
                        std::fs::write(&dest, bytes)
                            .map_err(|e| format!("rebase write {rel}: {e}"))?;
                        final_head.insert(rel.clone(), h.clone());
                    }
                    None => {
                        // Fabric deleted it: mirror the deletion.
                        if let Ok(dest) = weave::safe_join(&wt_path, rel) {
                            let _ = std::fs::remove_file(dest);
                        }
                        final_head.remove(rel);
                    }
                }
                fast_forwarded.push(rel.clone());
            } else if thread_changed && fabric_changed && wt_h != repo_h {
                conflicts.push(rel.clone());
            }
        }
        // Drop tombstones for files the new base does not have either — a
        // deletion of something already gone is not a change.
        final_head.retain(|rel, h| h != TOMBSTONE || repo_cap.manifest.contains_key(rel));
        // Phase 3 (locked): new base, refreshed head, honest note.
        self.with(|s, base| {
            let rs = s.repo_states.get_mut(&repo.id).expect("repo state exists");
            rs.seq += 1;
            let base_stitch = Stitch {
                id: format!("stitch-{}-{}", now_ms(), rs.seq),
                thread_id: thread_id.to_string(),
                parent: None,
                files: repo_cap.manifest.clone(),
                ts_ms: now_ms(),
            };
            rs.stitches.push(base_stitch.clone());
            let new_head = match &old_head {
                Some(h) if h.files == final_head => None, // unchanged
                None => None, // never stitched — nothing to re-head
                Some(h) => {
                    rs.seq += 1;
                    let st = Stitch {
                        id: format!("stitch-{}-{}", now_ms(), rs.seq),
                        thread_id: thread_id.to_string(),
                        parent: Some(h.id.clone()),
                        files: final_head.clone(),
                        ts_ms: now_ms(),
                    };
                    rs.stitches.push(st.clone());
                    Some(st)
                }
            };
            let thread = rs
                .threads
                .iter_mut()
                .find(|t| t.id == thread_id)
                .ok_or_else(|| format!("thread {thread_id} vanished mid-rebase"))?;
            thread.base_stitch = Some(base_stitch.id.clone());
            if let Some(st) = &new_head {
                thread.head_stitch = Some(st.id.clone());
            }
            if thread.status == ThreadStatus::Proposed {
                thread.status = ThreadStatus::Active;
            }
            thread.note = if conflicts.is_empty() {
                "rebased onto the fabric — clean".to_string()
            } else {
                cap(
                    &format!(
                        "rebased — BOTH sides had changed {}; your version was kept in the \
                         worktree. Review against the repo tree, then re-propose",
                        conflicts.join(", ")
                    ),
                    MAX_GOAL_CHARS,
                )
            };
            let approval_id = thread.approval_id.take();
            let thread = thread.clone();
            store::append_event(
                base,
                &repo.id,
                &serde_json::json!({
                    "ts_ms": now_ms(), "kind": "rebased", "thread": thread_id,
                    "fast_forwarded": fast_forwarded.len(), "conflicts": conflicts,
                }),
            );
            Ok(RebaseOutcome {
                thread,
                fast_forwarded: fast_forwarded.clone(),
                conflicts: conflicts.clone(),
                approval_id,
            })
        })
    }

    /// Remove worktrees that are DONE: a Woven thread's worktree goes once
    /// every file it captured still matches its last stitch (nothing
    /// uncaptured would be lost). Anything else is kept with an honest
    /// reason — live threads are in use, orphans stay adoptable, and a
    /// worktree with uncaptured divergence is never deleted.
    pub fn clean_worktrees(&self, repo_id: &str) -> Result<CleanReport, String> {
        // Phase 1 (locked): collect candidates.
        let (repo, candidates) = self.with(|s, base| {
            let repo = s
                .repos
                .iter()
                .find(|r| r.id == repo_id)
                .cloned()
                .ok_or_else(|| format!("no registered repo with id {repo_id}"))?;
            reconcile_repo(s, repo_id, now_ms(), base);
            let rs = s.repo_states.get(repo_id).expect("repo state exists");
            let cands: Vec<(Thread, BTreeMap<String, String>)> = rs
                .threads
                .iter()
                .filter(|t| t.worktree.is_some())
                .map(|t| {
                    // What the worktree should contain: the head stitch,
                    // falling back to the base for never-stitched threads.
                    let expected = t
                        .head_stitch
                        .as_ref()
                        .or(t.base_stitch.as_ref())
                        .and_then(|id| rs.stitches.iter().find(|st| st.id == *id))
                        .map(|st| st.files.clone())
                        .unwrap_or_default();
                    (t.clone(), expected)
                })
                .collect();
            Ok::<_, String>((repo, cands))
        })?;
        // Phase 2 (unlocked): check + remove.
        let mut report = CleanReport::default();
        let mut cleared: Vec<String> = Vec::new();
        for (t, expected) in candidates {
            let wt = t.worktree.clone().expect("filtered above");
            let wt_path = std::path::Path::new(&wt);
            if !wt_path.is_dir() {
                cleared.push(t.id.clone());
                report
                    .removed
                    .push((t.id.clone(), format!("{wt} (already gone)")));
                continue;
            }
            if t.status != ThreadStatus::Woven {
                report.skipped.push((
                    t.id.clone(),
                    format!("thread is {:?} — worktree still in use", t.status),
                ));
                continue;
            }
            let mut divergent: Vec<String> = Vec::new();
            for (rel, h) in &expected {
                let on_disk = weave::hash_on_disk(wt_path, rel);
                let matches = match (h.as_str(), on_disk) {
                    (TOMBSTONE, None) => true,
                    (want, Some(have)) => want == have,
                    (_, None) => false,
                };
                if !matches {
                    divergent.push(rel.clone());
                }
            }
            if !divergent.is_empty() {
                report.skipped.push((
                    t.id.clone(),
                    cap(
                        &format!(
                            "uncaptured changes in {} — refusing to delete; stitch or \
                             inspect the worktree first",
                            divergent.join(", ")
                        ),
                        MAX_GOAL_CHARS,
                    ),
                ));
                continue;
            }
            match worktree::remove(std::path::Path::new(&repo.path), wt_path) {
                Ok(()) => {
                    cleared.push(t.id.clone());
                    report.removed.push((t.id.clone(), wt.clone()));
                }
                Err(e) => report.skipped.push((t.id.clone(), e)),
            }
        }
        // Phase 3 (locked): forget removed worktrees.
        if !cleared.is_empty() {
            self.with(|s, base| {
                if let Some(rs) = s.repo_states.get_mut(repo_id) {
                    for t in rs.threads.iter_mut() {
                        if cleared.contains(&t.id) {
                            t.worktree = None;
                        }
                    }
                }
                store::append_event(
                    base,
                    repo_id,
                    &serde_json::json!({
                        "ts_ms": now_ms(), "kind": "worktrees_cleaned",
                        "threads": cleared,
                    }),
                );
            });
        }
        Ok(report)
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
    /// manual [`Heddle::withdraw`] covers those.
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

    // -- sync support (called by the `sync` module; engine stays git-free) --

    /// Apply weaves another machine landed on the shared fabric: overlay
    /// each weave's `applied` manifest onto the repo tree (blobs must
    /// already be in the local object store), record the weave, advance the
    /// tip. The caller (sync) verified these ids extend our history —
    /// weaves already present are skipped.
    pub fn import_fabric_weaves(
        &self,
        repo_id: &str,
        weaves: Vec<Weave>,
        origin: &str,
    ) -> Result<usize, String> {
        self.with(|s, base| {
            let repo = s
                .repos
                .iter()
                .find(|r| r.id == repo_id)
                .cloned()
                .ok_or_else(|| format!("no registered repo with id {repo_id}"))?;
            let rs = s.repo_states.get_mut(repo_id).expect("repo state exists");
            let objects = store::objects_dir(base, repo_id);
            let mut applied_count = 0;
            for w in weaves {
                if rs.fabric.history.iter().any(|id| *id == w.id) {
                    continue;
                }
                weave::apply_overlay(std::path::Path::new(&repo.path), &w.applied, &objects)?;
                rs.fabric.tip = Some(w.id.clone());
                rs.fabric.history.push(w.id.clone());
                store::append_event(
                    base,
                    repo_id,
                    &serde_json::json!({
                        "ts_ms": now_ms(), "kind": "weave_synced_in", "weave": w.id,
                        "from": origin, "files": w.applied.len(),
                    }),
                );
                rs.weaves.push(w);
                applied_count += 1;
            }
            Ok(applied_count)
        })
    }

    /// Replace the cached peer view (what other machines last published)
    /// and record cross-machine toe-steps: our live leases vs each peer's,
    /// one warning per lease pair, deduplicated against what is already
    /// recorded.
    pub fn update_peers(
        &self,
        repo_id: &str,
        peers: Vec<PeerSnapshot>,
    ) -> Result<Vec<ToeStep>, String> {
        let now = now_ms();
        self.with(|s, base| {
            if !s.repos.iter().any(|r| r.id == repo_id) {
                return Err(format!("no registered repo with id {repo_id}"));
            }
            reconcile_repo(s, repo_id, now, base);
            let rs = s.repo_states.get_mut(repo_id).expect("repo state exists");
            let mut fresh = Vec::new();
            for peer in &peers {
                for pl in &peer.leases {
                    // Only the peer's live leases can be stepped on.
                    let live = !pl.expired(now)
                        && peer.threads.iter().any(|t| {
                            t.lease_id.as_deref() == Some(&pl.id) && t.status.is_live()
                        });
                    if !live {
                        continue;
                    }
                    for ours in rs.leases.clone() {
                        let ours_live = !ours.expired(now)
                            && rs.threads.iter().any(|t| {
                                t.lease_id.as_deref() == Some(&ours.id) && t.status.is_live()
                            });
                        if !ours_live {
                            continue;
                        }
                        let already = rs
                            .toe_steps
                            .iter()
                            .any(|t| t.lease_a == ours.id && t.lease_b == pl.id);
                        if already {
                            continue;
                        }
                        let hit = ours.scope.iter().find_map(|a| {
                            pl.scope
                                .iter()
                                .find(|b| lease::patterns_may_overlap(a, b))
                                .map(|b| (a.clone(), b.clone()))
                        });
                        if let Some((pat_a, pat_b)) = hit {
                            let step = ToeStep {
                                id: format!("toe-{now}-{}", rs.toe_steps.len() + fresh.len() + 1),
                                ts_ms: now,
                                lease_a: ours.id.clone(),
                                lease_b: pl.id.clone(),
                                goal_a: ours.goal.clone(),
                                goal_b: format!("{} [on {}]", pl.goal, peer.machine),
                                pattern_a: pat_a.clone(),
                                pattern_b: pat_b.clone(),
                                suggested_split: lease::suggest_split(&pat_a, &pat_b),
                            };
                            rs.toe_steps.push(step.clone());
                            fresh.push(step.clone());
                            store::append_event(
                                base,
                                repo_id,
                                &serde_json::json!({
                                    "ts_ms": now, "kind": "toe_step_cross_machine",
                                    "lease_a": step.lease_a, "lease_b": step.lease_b,
                                    "peer": peer.machine,
                                }),
                            );
                        }
                    }
                }
            }
            rs.peers = peers;
            Ok(fresh)
        })
    }

    /// Import a thread claimed from another machine (the sync module won its
    /// claim CAS first): thread + lease + stitches enter local state, the
    /// thread lands as Orphaned so the normal [`Heddle::adopt`] flow — fresh
    /// worktree, head stitch materialized — takes it from there. Blobs for
    /// the stitch manifests must already be in the local object store.
    pub fn import_thread(
        &self,
        repo_id: &str,
        mut thread: Thread,
        lease: Lease,
        stitches: Vec<Stitch>,
        origin: &str,
    ) -> Result<Thread, String> {
        self.with(|s, base| {
            if !s.repos.iter().any(|r| r.id == repo_id) {
                return Err(format!("no registered repo with id {repo_id}"));
            }
            let rs = s.repo_states.get_mut(repo_id).expect("repo state exists");
            if rs.threads.iter().any(|t| t.id == thread.id) {
                return Err(format!("thread {} already exists locally", thread.id));
            }
            thread.repo_id = repo_id.to_string();
            thread.status = ThreadStatus::Orphaned;
            thread.worktree = None; // the dead machine's path means nothing here
            thread.base_stitch = None; // rebased against OUR tree on adopt
            thread.approval_id = None;
            thread.lease_id = Some(lease.id.clone());
            thread.note = format!("orphan imported from {origin}");
            for st in stitches {
                if !rs.stitches.iter().any(|x| x.id == st.id) {
                    rs.stitches.push(st);
                }
            }
            rs.leases.retain(|l| l.id != lease.id);
            rs.leases.push(lease);
            rs.threads.push(thread.clone());
            store::append_event(
                base,
                repo_id,
                &serde_json::json!({
                    "ts_ms": now_ms(), "kind": "thread_imported",
                    "thread": thread.id, "from": origin,
                }),
            );
            Ok(thread)
        })
    }

    // -- reads --------------------------------------------------------------

    /// A full clone of the registry + per-repo state, for callers to shape
    /// into output. Reconciles orphans first, so what you see is true.
    pub fn snapshot(&self) -> HeddleState {
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

    // -- draft-branch export -------------------------------------------------

    /// Write a thread's stitch chain to its per-thread git branch
    /// (`heddle/<thread-id-short>-<goal-slug>`) WITHOUT landing anything —
    /// draft-branch export for human review of in-flight (Active/Proposed)
    /// work; woven and orphaned threads export too, for archaeology.
    /// Rebuilds the branch from the current branch tip on every call —
    /// export is a projection, not state. Pure git plumbing: the working
    /// tree, index and current branch are never touched. Blocking (git
    /// subprocesses); call from a blocking-task helper on async paths.
    pub fn export_thread(&self, thread_id: &str) -> Result<ExportOutcome, String> {
        // Phase 1 (locked): gather the chain and base.
        let (repo, thread, stitches, base_manifest) = self.with(|s, base| {
            let repo_id = find_repo_of_thread(s, thread_id)
                .ok_or_else(|| format!("no thread with id {thread_id}"))?;
            reconcile_repo(s, &repo_id, now_ms(), base);
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
            let chain = stitch_chain(rs, thread.head_stitch.as_ref());
            if chain.is_empty() {
                return Err("nothing to export — capture a stitch first".to_string());
            }
            let base_manifest = thread
                .base_stitch
                .as_ref()
                .and_then(|id| rs.stitches.iter().find(|st| st.id == *id))
                .map(|st| st.files.clone())
                .unwrap_or_default();
            Ok((repo, thread, chain, base_manifest))
        })?;
        if !worktree::is_git_repo(std::path::Path::new(&repo.path)) {
            return Err(format!("{} is not a git repo — nothing to export to", repo.path));
        }
        // Phase 2 (unlocked): replay the chain onto the branch, plumbing only.
        let repo_dir = std::path::Path::new(&repo.path);
        let objects = store::objects_dir(&self.base(), &repo.id);
        let base_commit = bridge::head_commit(repo_dir)?;
        let branch = bridge::thread_branch_name(&thread.id, &thread.goal);
        let commits = bridge::build_thread_branch(
            repo_dir,
            &base_commit,
            &branch,
            &thread.goal,
            &stitches,
            &base_manifest,
            &objects,
        )?;
        if commits == 0 {
            return Err(
                "every stitch in the chain was an empty diff — nothing to export".to_string(),
            );
        }
        // Phase 3 (locked): log it.
        self.with(|s, base| {
            let _ = s; // state unchanged — export is a projection
            store::append_event(
                base,
                &repo.id,
                &serde_json::json!({
                    "ts_ms": now_ms(), "kind": "exported", "thread": thread_id,
                    "branch": branch, "commits": commits,
                }),
            );
        });
        Ok(ExportOutcome {
            thread,
            branch,
            commits,
        })
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Expire leases past TTL and orphan their live threads. Runs inside the
/// store lock; appends an `orphaned` log line per flip.
fn reconcile_repo(s: &mut HeddleState, repo_id: &str, now: u64, base: &PathBuf) {
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

/// A thread's stitch chain, oldest first, walked from `head` through
/// `parent` links. A pruned parent ends the walk. The thread's base stitch
/// (worktree snapshot) is never in this chain — it has no parent link from
/// the first real stitch.
fn stitch_chain(rs: &RepoState, head: Option<&String>) -> Vec<Stitch> {
    let mut chain = Vec::new();
    let mut cur = head.cloned();
    while let Some(id) = cur {
        let Some(st) = rs.stitches.iter().find(|s| s.id == id) else {
            break;
        };
        chain.push(st.clone());
        cur = st.parent.clone();
    }
    chain.reverse();
    chain
}

fn find_repo_of_lease(s: &HeddleState, lease_id: &str) -> Option<String> {
    s.repo_states
        .iter()
        .find(|(_, rs)| rs.leases.iter().any(|l| l.id == lease_id))
        .map(|(id, _)| id.clone())
}

fn find_repo_of_thread(s: &HeddleState, thread_id: &str) -> Option<String> {
    s.repo_states
        .iter()
        .find(|(_, rs)| rs.threads.iter().any(|t| t.id == thread_id))
        .map(|(id, _)| id.clone())
}

/// Bounded history, live objects never dropped to make room: terminal
/// (Woven) threads go oldest-first; stitches beyond the per-thread cap drop
/// oldest-first except a thread's head; weaves and toe-steps are rings.
fn prune(s: &mut HeddleState) {
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
            let (head, base) = rs
                .threads
                .iter()
                .find(|t| t.id == tid)
                .map(|t| (t.head_stitch.clone(), t.base_stitch.clone()))
                .unwrap_or((None, None));
            loop {
                let count = rs.stitches.iter().filter(|st| st.thread_id == tid).count();
                if count <= MAX_STITCHES_PER_THREAD {
                    break;
                }
                let Some(pos) = rs.stitches.iter().position(|st| {
                    st.thread_id == tid
                        && Some(&st.id) != head.as_ref()
                        && Some(&st.id) != base.as_ref()
                }) else {
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

/// How deep under a search root `heddle repair` looks for a moved repo.
/// Three levels covers the realistic tidy-up (`work/AETHER` →
/// `work/aether/AETHER`) without walking an entire home directory.
const REPAIR_SCAN_DEPTH: usize = 3;

/// One relocation `heddle repair` performed (or would, under `--dry-run`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rebind {
    pub old_path: String,
    pub new_path: String,
    pub old_id: String,
    /// Empty under `--dry-run`: nothing was minted.
    pub new_id: String,
    /// `"root commit"` (exact) or `"name"` (unique basename match).
    pub matched_by: String,
}

/// What `repair_repos` hands back: what moved, and what it could not place.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RepairReport {
    pub rebound: Vec<Rebind>,
    /// (old path, honest reason) for every repo left alone.
    pub unmatched: Vec<(String, String)>,
}

/// A git repo found under a search root, with the identity used to match it.
struct Candidate {
    path: String,
    basename: String,
    root_commit: Option<String>,
}

/// Walk `roots` to `depth`, collecting git repos that are not already bound
/// to a registered repo. Skips dotted directories (`.git`, `.venv`, caches)
/// and never follows symlinks — a scan must not wander out of the tree it
/// was pointed at.
fn scan_for_git_repos(roots: &[PathBuf], depth: usize, known: &[String]) -> Vec<Candidate> {
    let mut found: Vec<Candidate> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut frontier: Vec<(PathBuf, usize)> = roots.iter().map(|r| (r.clone(), 0)).collect();
    while let Some((dir, d)) = frontier.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_dir() || p.is_symlink() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let canon = match std::fs::canonicalize(&p) {
                Ok(c) => c.to_string_lossy().to_string(),
                Err(_) => continue,
            };
            if seen.contains(&canon) {
                continue;
            }
            seen.push(canon.clone());
            if worktree::is_git_repo(&p) {
                if !known.contains(&canon) {
                    found.push(Candidate {
                        basename: name,
                        root_commit: worktree::root_commit(&p),
                        path: canon,
                    });
                }
                // A git repo is a leaf: heddle repos don't nest inside one.
                continue;
            }
            if d + 1 < depth {
                frontier.push((p, d + 1));
            }
        }
    }
    found
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
            "heddle-{tag}-{}-{}",
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

    fn rig(tag: &str) -> (Heddle, PathBuf, RepoConfig) {
        let base = scratch(&format!("{tag}-data"));
        let repo_dir = scratch(&format!("{tag}-repo"));
        mk_repo(
            &repo_dir,
            &[("src/main.rs", "fn main() {}\n"), ("README.md", "hi\n")],
        );
        let heddle = Heddle::at(base.clone());
        let repo = heddle
            .register_repo(repo_dir.to_str().unwrap(), Some("true".into()), false)
            .expect("register");
        (heddle, repo_dir, repo)
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
        let (heddle, repo_dir, repo) = rig("rereg");
        let again = heddle
            .register_repo(repo_dir.to_str().unwrap(), Some("false".into()), true)
            .expect("re-register");
        assert_eq!(again.id, repo.id);
        assert_eq!(again.verify_cmd, "false");
        assert!(again.git_bridge);
        assert_eq!(heddle.snapshot().repos.len(), 1);
    }

    #[test]
    fn stitch_dedups_unchanged_content_and_manifests() {
        let (heddle, repo_dir, repo) = rig("dedup");
        let d = heddle
            .declare_lease(&repo.id, "tester", "touch src", vec!["src/**".into()], vec![], None)
            .expect("lease");
        let s1 = heddle.stitch(&d.lease.id).expect("stitch 1");
        assert!(!s1.unchanged);
        assert_eq!(s1.stitch.files.len(), 1); // src/main.rs only — README out of scope
        // Same content again: NO new stitch.
        let s2 = heddle.stitch(&d.lease.id).expect("stitch 2");
        assert!(s2.unchanged);
        assert_eq!(s2.stitch.id, s1.stitch.id);
        // Change the file: new stitch, new hash, parent chained.
        mk_repo(&repo_dir, &[("src/main.rs", "fn main() { /* v2 */ }\n")]);
        let s3 = heddle.stitch(&d.lease.id).expect("stitch 3");
        assert!(!s3.unchanged);
        assert_eq!(s3.stitch.parent.as_deref(), Some(s1.stitch.id.as_str()));
        assert_ne!(s3.stitch.files["src/main.rs"], s1.stitch.files["src/main.rs"]);
        // Content-addressing: both blobs exist exactly once each.
        let objects = store::objects_dir(&heddle.base(), &repo.id);
        for h in [&s1.stitch.files["src/main.rs"], &s3.stitch.files["src/main.rs"]] {
            assert!(store::read_blob(&objects, h).is_ok(), "blob {h} present");
        }
    }

    #[test]
    fn orphan_expiry_then_adopt_preserves_goal_and_criteria() {
        let (heddle, _repo_dir, repo) = rig("orphan");
        let d = heddle
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
        heddle.reconcile();
        let snap = heddle.snapshot();
        let rs = &snap.repo_states[&repo.id];
        assert_eq!(rs.threads[0].status, ThreadStatus::Active);
        // Age the heartbeat past TTL by editing state directly (no sleeping
        // in tests), then reconcile.
        heddle.with(|s, _| {
            s.repo_states.get_mut(&repo.id).unwrap().leases[0].last_heartbeat_ms = 1;
        });
        heddle.reconcile();
        let snap = heddle.snapshot();
        let rs = &snap.repo_states[&repo.id];
        assert_eq!(rs.threads[0].status, ThreadStatus::Orphaned);
        // Heartbeat is refused once orphaned.
        assert!(heddle.heartbeat(&d.lease.id).is_err());
        // Adopt: same lease id, new holder, fresh heartbeat, criteria intact.
        let (thread, lease) = heddle.adopt(&d.thread.id, "second-holder").expect("adopt");
        assert_eq!(thread.status, ThreadStatus::Adopted);
        assert_eq!(lease.id, d.lease.id);
        assert_eq!(lease.holder, "second-holder");
        assert_eq!(lease.goal, "migrate the schema");
        assert_eq!(lease.criteria, vec!["tests pass".to_string()]);
        assert!(!lease.expired(now_ms()));
        // Adopted has Active semantics: heartbeat works again.
        assert!(heddle.heartbeat(&d.lease.id).is_ok());
    }

    #[test]
    fn weave_gate_green_lands_only_via_land_weave_and_red_never_does() {
        let (heddle, repo_dir, repo) = rig("gate");
        let d = heddle
            .declare_lease(&repo.id, "t", "greenify", vec!["src/**".into()], vec![], None)
            .expect("lease");
        heddle.stitch(&d.lease.id).expect("stitch");
        // verify_cmd is "true" → green; thread flips to Proposed, fabric
        // untouched until land.
        let out = heddle.propose(&d.thread.id).expect("propose");
        assert!(out.green);
        assert_eq!(out.thread.status, ThreadStatus::Proposed);
        assert!(heddle.snapshot().repo_states[&repo.id].fabric.tip.is_none());
        // A second propose while one is in flight is refused.
        assert!(heddle.propose(&d.thread.id).is_err());
        // Land (callers reach this only after an explicit human yes).
        let landed = heddle.land_weave(&out.weave.id).expect("land");
        assert_eq!(landed.thread.status, ThreadStatus::Woven);
        let snap = heddle.snapshot();
        let rs = &snap.repo_states[&repo.id];
        assert_eq!(rs.fabric.tip.as_deref(), Some(out.weave.id.as_str()));
        assert_eq!(rs.fabric.history, vec![out.weave.id.clone()]);
        assert!(rs.leases.is_empty(), "lease released on land");
        // Red: new thread, repo re-registered with a failing verify.
        heddle.register_repo(repo_dir.to_str().unwrap(), Some("false".into()), false)
            .expect("re-register red");
        let d2 = heddle
            .declare_lease(&repo.id, "t", "reddify", vec!["src/**".into()], vec![], None)
            .expect("lease 2");
        heddle.stitch(&d2.lease.id).expect("stitch 2");
        let out2 = heddle.propose(&d2.thread.id).expect("propose 2");
        assert!(!out2.green);
        assert_eq!(out2.thread.status, ThreadStatus::Active, "red keeps the thread active");
        assert!(out2.thread.note.starts_with("verify red"));
        // Red can never land, even if someone tries.
        assert!(heddle.land_weave(&out2.weave.id).is_err());
        let snap = heddle.snapshot();
        assert_eq!(
            snap.repo_states[&repo.id].fabric.history.len(),
            1,
            "fabric unchanged by red"
        );
    }

    #[test]
    fn fabric_ordering_a_stale_parent_refuses_to_land_until_reproposed() {
        let (heddle, _repo_dir, repo) = rig("order");
        let mk = |goal: &str, scope: &str| {
            let d = heddle
                .declare_lease(&repo.id, "t", goal, vec![scope.into()], vec![], None)
                .expect("lease");
            heddle.stitch(&d.lease.id).expect("stitch");
            let proposed = heddle.propose(&d.thread.id).expect("propose");
            (d, proposed)
        };
        let (_d1, w1) = mk("first", "src/**");
        let (d2, w2) = mk("second", "README.md");
        // Both verified against an empty fabric; first lands fine.
        heddle.land_weave(&w1.weave.id).expect("land w1");
        // Second's parent is stale — refused, honestly noted.
        let err = heddle.land_weave(&w2.weave.id).unwrap_err();
        assert!(err.contains("re-propose"), "got: {err}");
        // Thread 2 must re-propose... but it is Proposed; deny path resets
        // nothing, so flip via a fresh propose after the note. v1 keeps this
        // manual: adopt/propose guards mean we go through deny first.
        heddle.with(|s, _| {
            let rs = s.repo_states.get_mut(&repo.id).unwrap();
            let t = rs.threads.iter_mut().find(|t| t.id == d2.thread.id).unwrap();
            t.status = ThreadStatus::Active; // simulate agent acting on the note
        });
        let w2b = heddle.propose(&d2.thread.id).expect("re-propose");
        assert_eq!(w2b.weave.fabric_parent.as_deref(), Some(w1.weave.id.as_str()));
        heddle.land_weave(&w2b.weave.id).expect("land w2b");
        let snap = heddle.snapshot();
        assert_eq!(
            snap.repo_states[&repo.id].fabric.history,
            vec![w1.weave.id.clone(), w2b.weave.id.clone()],
            "history is orderly, parent-linked"
        );
    }

    #[test]
    fn deny_keeps_the_thread_proposed_with_the_note() {
        let (heddle, _repo_dir, repo) = rig("deny");
        let d = heddle
            .declare_lease(&repo.id, "t", "goal", vec!["src/**".into()], vec![], None)
            .expect("lease");
        heddle.stitch(&d.lease.id).expect("stitch");
        let out = heddle.propose(&d.thread.id).expect("propose");
        heddle.mark_parked(&d.thread.id, "appr-1").expect("mark parked");
        heddle.deny_weave(&out.weave.id, "denied by operator").expect("deny");
        let snap = heddle.snapshot();
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
        let (heddle, _repo_dir, repo) = rig("withdraw");
        let d = heddle
            .declare_lease(&repo.id, "t", "goal", vec!["src/**".into()], vec![], None)
            .expect("lease");
        // Withdraw on a non-proposed thread refuses.
        assert!(heddle.withdraw(&d.thread.id, "").is_err());
        heddle.stitch(&d.lease.id).expect("stitch");
        let out = heddle.propose(&d.thread.id).expect("propose");
        assert!(out.green);
        heddle.mark_parked(&d.thread.id, "appr-w").expect("mark parked");
        // While Proposed, a second propose is refused — the stuck state.
        assert!(heddle.propose(&d.thread.id).is_err());
        // Withdraw: back to Active with the note, approval id handed back so
        // an embedding host can resolve the moot parked approval.
        let (t, aid) = heddle.withdraw(&d.thread.id, "").expect("withdraw");
        assert_eq!(t.status, ThreadStatus::Active);
        assert_eq!(t.note, "withdrawn — re-propose when ready");
        assert!(t.approval_id.is_none());
        assert_eq!(aid.as_deref(), Some("appr-w"));
        // A late deny for the old weave must not clobber the Active thread.
        heddle.deny_weave(&out.weave.id, "denied by operator").expect("late deny");
        let snap = heddle.snapshot();
        let t = &snap.repo_states[&repo.id].threads[0];
        assert_eq!(t.status, ThreadStatus::Active);
        assert_eq!(t.note, "withdrawn — re-propose when ready");
        // And re-propose works.
        let again = heddle.propose(&d.thread.id).expect("re-propose");
        assert!(again.green);
        assert_eq!(again.thread.status, ThreadStatus::Proposed);
    }

    #[test]
    fn a_lapsed_parked_approval_reconciles_the_thread_back_to_active() {
        let (heddle, _repo_dir, repo) = rig("lapse");
        let d = heddle
            .declare_lease(&repo.id, "t", "goal", vec!["src/**".into()], vec![], None)
            .expect("lease");
        heddle.stitch(&d.lease.id).expect("stitch");
        heddle.propose(&d.thread.id).expect("propose");
        heddle.mark_parked(&d.thread.id, "appr-live").expect("mark parked");
        // The approval is still pending → nothing changes.
        let pending: std::collections::HashSet<String> =
            ["appr-live".to_string()].into_iter().collect();
        heddle.reconcile_parked(&pending);
        let t = heddle.snapshot().repo_states[&repo.id].threads[0].clone();
        assert_eq!(t.status, ThreadStatus::Proposed);
        assert_eq!(t.approval_id.as_deref(), Some("appr-live"));
        // The approval vanished (a host restart or timeout killed it — they
        // are in-memory only) → the thread returns to Active, honestly noted.
        heddle.reconcile_parked(&std::collections::HashSet::new());
        let t = heddle.snapshot().repo_states[&repo.id].threads[0].clone();
        assert_eq!(t.status, ThreadStatus::Active);
        assert_eq!(t.note, "approval lapsed — re-propose when ready");
        assert!(t.approval_id.is_none());
        // Recovery is real: propose works again.
        assert!(heddle.propose(&d.thread.id).expect("re-propose").green);
        // A Proposed thread with NO recorded approval id (mid-gate) is never
        // touched by reconcile — only `withdraw` may move it.
        heddle.reconcile_parked(&std::collections::HashSet::new());
        let t = heddle.snapshot().repo_states[&repo.id].threads[0].clone();
        assert_eq!(t.status, ThreadStatus::Proposed, "no approval id → left alone");
    }

    // -- worktree isolation --------------------------------------------------

    fn git_ok(dir: &PathBuf, args: &[&str]) {
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

    /// A rig whose repo is a real git repo with one commit — isolation
    /// activates under Auto.
    fn git_rig(tag: &str) -> (Heddle, PathBuf, RepoConfig) {
        let base = scratch(&format!("{tag}-data"));
        let repo_dir = scratch(&format!("{tag}-repo"));
        mk_repo(
            &repo_dir,
            &[
                ("src/main.rs", "fn main() {}\n"),
                ("src/util.rs", "pub fn u() {}\n"),
            ],
        );
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

    fn read(p: &std::path::Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn landing_a_weave_retires_the_threads_worktree() {
        let (heddle, _repo_dir, repo) = git_rig("autoclean");
        let d = heddle
            .declare_lease(&repo.id, "t", "edit", vec!["src/**".into()], vec![], None)
            .expect("lease");
        let wt = PathBuf::from(d.thread.worktree.as_ref().expect("isolated"));
        std::fs::write(wt.join("src/main.rs"), "fn main() { /* v2 */ }\n").unwrap();
        heddle.stitch(&d.lease.id).expect("stitch");
        let p = heddle.propose(&d.thread.id).expect("propose");
        assert!(p.green);
        heddle.land_weave(&p.weave.id).expect("land");
        assert!(
            !wt.exists(),
            "a woven thread's worktree is gone without anyone running `heddle clean`"
        );
    }

    #[test]
    fn repair_rebinds_a_moved_repo_to_its_existing_state() {
        let (heddle, repo_dir, repo) = git_rig("moved");
        let d = heddle
            .declare_lease(&repo.id, "t", "in flight", vec!["src/**".into()], vec![], None)
            .expect("lease");
        let thread_id = d.thread.id.clone();

        // The tidy-up: the repo moves into a subfolder.
        let nest = repo_dir.parent().unwrap().join("moved-nest");
        std::fs::create_dir_all(&nest).unwrap();
        let dest = nest.join(repo_dir.file_name().unwrap());
        std::fs::rename(&repo_dir, &dest).unwrap();
        heddle.reset_cache();

        // Before repair the repo is unreachable from its new location.
        assert!(
            heddle.repo_containing(dest.to_str().unwrap()).is_none(),
            "a moved repo is stranded until repaired"
        );

        let plan = heddle.repair_repos(&[], true);
        assert_eq!(plan.rebound.len(), 1, "dry run plans the rebind: {plan:?}");
        assert!(
            heddle.repo_containing(dest.to_str().unwrap()).is_none(),
            "--dry-run changed nothing"
        );

        let report = heddle.repair_repos(&[], false);
        assert_eq!(report.rebound.len(), 1, "one repo rebound: {report:?}");
        assert_eq!(report.rebound[0].matched_by, "root commit");

        let found = heddle
            .repo_containing(dest.to_str().unwrap())
            .expect("moved repo is reachable again");
        assert_eq!(found.path, dest.canonicalize().unwrap().to_string_lossy());
        assert_ne!(found.id, repo.id, "new path means a new id");

        // The state came WITH it — thread, and a worktree path that resolves.
        let snap = heddle.snapshot();
        let rs = snap
            .repo_states
            .get(&found.id)
            .expect("state moved to the new id");
        let t = rs
            .threads
            .iter()
            .find(|t| t.id == thread_id)
            .expect("thread survived the move");
        assert_eq!(t.repo_id, found.id, "back-reference rewritten");
        let wt = PathBuf::from(t.worktree.as_ref().expect("still isolated"));
        assert!(wt.exists(), "worktree path points at the relocated state dir");

        // ...and git still KNOWS it. The worktrees move with the state dir, so
        // repair has to name their new paths; repairing blind and then pruning
        // deletes the registration of a checkout that is sitting right there.
        let listed = std::process::Command::new("git")
            .arg("-C")
            .arg(&dest)
            .args(["worktree", "list"])
            .output()
            .expect("git worktree list");
        let listed = String::from_utf8_lossy(&listed.stdout).to_string();
        assert!(
            listed.contains(&wt.to_string_lossy().to_string()),
            "the relocated worktree is still a registered git worktree:\n{listed}"
        );
        // And it is a working checkout, not an orphaned directory.
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["status", "--porcelain"])
            .output()
            .expect("git status in worktree");
        assert!(st.status.success(), "git works inside the relocated worktree");
    }

    #[test]
    fn repair_leaves_an_ambiguous_match_alone_rather_than_guessing() {
        let (heddle, repo_dir, _repo) = git_rig("ambig");
        // Two candidates with the same basename and NO shared identity: the
        // registered repo's root commit exists in neither.
        let nest = repo_dir.parent().unwrap().join("ambig-nest");
        let name = repo_dir.file_name().unwrap();
        for side in ["a", "b"] {
            let dir = nest.join(side).join(name);
            mk_repo(&dir, &[("src/main.rs", "fn main() {}\n")]);
            git_ok(&dir, &["init", "-q"]);
            git_ok(&dir, &["config", "user.email", "heddle@test"]);
            git_ok(&dir, &["config", "user.name", "heddle test"]);
            git_ok(&dir, &["add", "-A"]);
            git_ok(&dir, &["commit", "-q", "-m", format!("base {side}").as_str()]);
        }
        std::fs::remove_dir_all(&repo_dir).unwrap();
        heddle.reset_cache();

        let report = heddle.repair_repos(&[nest.to_string_lossy().to_string()], false);
        assert!(report.rebound.is_empty(), "nothing guessed: {report:?}");
        assert_eq!(report.unmatched.len(), 1);
        assert!(
            report.unmatched[0].1.contains("candidates matched"),
            "and it says why: {}",
            report.unmatched[0].1
        );
    }

    #[test]
    fn isolated_lease_gets_a_worktree_and_edits_there_never_touch_the_repo() {
        let (heddle, repo_dir, repo) = git_rig("iso");
        let d = heddle
            .declare_lease(&repo.id, "t", "solo work", vec!["src/**".into()], vec![], None)
            .expect("lease");
        let wt = d.thread.worktree.clone().expect("isolated by default on a git repo");
        assert_eq!(d.working_dir, wt);
        assert!(d.thread.base_stitch.is_some(), "base snapshotted");
        let wt = PathBuf::from(&wt);
        assert!(wt.join("src/main.rs").exists(), "worktree has HEAD's files");
        // No edits yet: stitch reports unchanged vs the base, head stays None.
        let s0 = heddle.stitch(&d.lease.id).expect("stitch 0");
        assert!(s0.unchanged);
        // Edit IN THE WORKTREE.
        std::fs::write(wt.join("src/main.rs"), "fn main() { /* wt */ }\n").unwrap();
        let s1 = heddle.stitch(&d.lease.id).expect("stitch 1");
        assert!(!s1.unchanged);
        // The repo tree is untouched by leasing, editing, stitching.
        assert_eq!(read(&repo_dir.join("src/main.rs")), "fn main() {}\n");
        // Green weave + land applies ONLY the thread's delta to the repo.
        let out = heddle.propose(&d.thread.id).expect("propose");
        assert!(out.green);
        let landed = heddle.land_weave(&out.weave.id).expect("land");
        assert_eq!(landed.files_applied, 1, "only the changed file lands");
        assert_eq!(read(&repo_dir.join("src/main.rs")), "fn main() { /* wt */ }\n");
    }

    #[test]
    fn two_threads_overlap_first_lands_second_refused_then_rebase_then_lands() {
        let (heddle, repo_dir, repo) = git_rig("pair");
        let da = heddle
            .declare_lease(&repo.id, "alice", "restyle main", vec!["src/**".into()], vec![], None)
            .expect("lease a");
        let db = heddle
            .declare_lease(&repo.id, "bob", "rework main too", vec!["src/**".into()], vec![], None)
            .expect("lease b");
        assert!(!db.toe_steps.is_empty(), "overlap warned at declaration");
        let wa = PathBuf::from(da.thread.worktree.as_ref().unwrap());
        let wb = PathBuf::from(db.thread.worktree.as_ref().unwrap());
        assert_ne!(wa, wb, "each thread has its OWN tree");
        // Overlapping edits to the same file, in separate worktrees; Alice
        // also touches util.rs (Bob never does).
        std::fs::write(wa.join("src/main.rs"), "fn main() { /* alice */ }\n").unwrap();
        std::fs::write(wa.join("src/util.rs"), "pub fn u() { /* alice */ }\n").unwrap();
        std::fs::write(wb.join("src/main.rs"), "fn main() { /* bob */ }\n").unwrap();
        heddle.stitch(&da.lease.id).expect("stitch a");
        heddle.stitch(&db.lease.id).expect("stitch b");
        // No clobbering happened: both worktrees hold their own versions.
        assert!(read(&wa.join("src/main.rs")).contains("alice"));
        assert!(read(&wb.join("src/main.rs")).contains("bob"));
        // Alice lands first.
        let pa = heddle.propose(&da.thread.id).expect("propose a");
        heddle.land_weave(&pa.weave.id).expect("land a");
        assert!(read(&repo_dir.join("src/main.rs")).contains("alice"));
        // Bob's verify is green — but landing refuses honestly: the fabric
        // moved under him on the shared file.
        let pb = heddle.propose(&db.thread.id).expect("propose b");
        assert!(pb.green);
        let err = heddle.land_weave(&pb.weave.id).unwrap_err();
        assert!(err.contains("fabric moved under you"), "{err}");
        assert!(err.contains("src/main.rs"), "{err}");
        assert!(err.contains("rebase"), "{err}");
        // Alice's version is still intact — nothing was clobbered.
        assert!(read(&repo_dir.join("src/main.rs")).contains("alice"));
        // Rebase: util.rs (fabric-only) fast-forwards, main.rs conflicts
        // and keeps Bob's version.
        let rb = heddle.rebase_thread(&db.thread.id).expect("rebase b");
        assert_eq!(rb.conflicts, vec!["src/main.rs".to_string()]);
        assert!(rb.fast_forwarded.contains(&"src/util.rs".to_string()));
        assert!(read(&wb.join("src/util.rs")).contains("alice"), "fabric ff'd in");
        assert!(read(&wb.join("src/main.rs")).contains("bob"), "thread's kept");
        assert_eq!(rb.thread.status, ThreadStatus::Active);
        // Bob reconciles by hand, re-stitches, re-proposes — and lands.
        std::fs::write(wb.join("src/main.rs"), "fn main() { /* alice+bob */ }\n").unwrap();
        heddle.stitch(&db.lease.id).expect("stitch b2");
        let pb2 = heddle.propose(&db.thread.id).expect("propose b2");
        assert!(pb2.green);
        heddle.land_weave(&pb2.weave.id).expect("land b2");
        assert!(read(&repo_dir.join("src/main.rs")).contains("alice+bob"));
        // util.rs kept Alice's version — Bob's stale base never overwrote it.
        assert!(read(&repo_dir.join("src/util.rs")).contains("alice"));
    }

    #[test]
    fn bare_target_inside_a_worktree_means_that_thread_even_among_many() {
        let (heddle, repo_dir, repo) = git_rig("bare-wt");
        let da = heddle
            .declare_lease(&repo.id, "alice", "restyle main", vec!["src/**".into()], vec![], None)
            .expect("lease a");
        let db = heddle
            .declare_lease(&repo.id, "bob", "rework util", vec!["src/**".into()], vec![], None)
            .expect("lease b");
        let wa = PathBuf::from(da.thread.worktree.as_ref().expect("isolated"));
        let wb = PathBuf::from(db.thread.worktree.as_ref().expect("isolated"));
        // Standing in a worktree — even a subdirectory of it — names that
        // thread, no matter how many others are live.
        let (r, t) = heddle
            .resolve_bare_target(wa.to_str().unwrap())
            .expect("A's worktree resolves");
        assert_eq!((r.id.as_str(), t.id.as_str()), (repo.id.as_str(), da.thread.id.as_str()));
        let (_, t) = heddle
            .resolve_bare_target(wb.join("src").to_str().unwrap())
            .expect("a subdir of B's worktree resolves");
        assert_eq!(t.id, db.thread.id, "B's worktree never means A's thread");
        // The same inference names the repo for `thread_containing` callers.
        let (r, t) = heddle.thread_containing(wb.to_str().unwrap()).expect("containing");
        assert_eq!((r.id, t.id), (repo.id.clone(), db.thread.id.clone()));
        // Outside any worktree the repo root stays ambiguous (see the
        // refusal test) — but never resolves to the WRONG thread.
        let err = heddle.resolve_bare_target(repo_dir.to_str().unwrap()).unwrap_err();
        assert!(err.contains(&da.thread.id) && err.contains(&db.thread.id), "{err}");
    }

    #[test]
    fn bare_target_with_one_live_thread_is_unambiguous() {
        let (heddle, repo_dir, repo) = rig("bare-one");
        let d = heddle
            .declare_lease(&repo.id, "solo", "only work-line", vec!["src/**".into()], vec![], None)
            .expect("lease");
        let (r, t) = heddle
            .resolve_bare_target(repo_dir.to_str().unwrap())
            .expect("one live thread resolves bare");
        assert_eq!(r.id, repo.id);
        assert_eq!(t.id, d.thread.id);
    }

    #[test]
    fn bare_target_refuses_to_guess_between_live_threads() {
        let (heddle, repo_dir, repo) = rig("bare-many");
        let da = heddle
            .declare_lease(&repo.id, "alice", "restyle main", vec!["src/**".into()], vec![], None)
            .expect("lease a");
        let db = heddle
            .declare_lease(&repo.id, "bob", "reword readme", vec!["README.md".into()], vec![], None)
            .expect("lease b");
        // Two live threads, no flag, not in a worktree: REFUSED — loudly,
        // with ids, goals and the way out. This is the exact scenario where
        // trusting the shared solo pointer stitched onto a stranger's thread.
        let cwd = repo_dir.to_str().unwrap();
        let err = heddle.resolve_bare_target(cwd).unwrap_err();
        for needle in [
            da.thread.id.as_str(),
            db.thread.id.as_str(),
            da.lease.id.as_str(),
            db.lease.id.as_str(),
            "restyle main",
            "reword readme",
            "--lease",
        ] {
            assert!(err.contains(needle), "refusal must name '{needle}':\n{err}");
        }
        // Alice weaves; Bob is then the only live thread — bare resolves
        // again (woven threads never count toward ambiguity).
        heddle.stitch(&da.lease.id).expect("stitch a");
        let p = heddle.propose(&da.thread.id).expect("propose a");
        heddle.land_weave(&p.weave.id).expect("land a");
        let (_, t) = heddle.resolve_bare_target(cwd).expect("one live thread left");
        assert_eq!(t.id, db.thread.id);
    }

    #[test]
    fn bare_target_with_no_live_threads_says_lease_first() {
        let (heddle, repo_dir, _repo) = rig("bare-none");
        let err = heddle.resolve_bare_target(repo_dir.to_str().unwrap()).unwrap_err();
        assert!(err.contains("no live thread"), "{err}");
        assert!(err.contains("heddle lease"), "{err}");
        // And an unregistered directory is its own honest error.
        let stray = scratch("bare-stray");
        let err = heddle.resolve_bare_target(stray.to_str().unwrap()).unwrap_err();
        assert!(err.contains("no registered repo"), "{err}");
    }

    #[test]
    fn isolation_modes_in_place_flag_plain_dirs_and_isolated_refusal() {
        // A git repo with --in-place behaves like v0.1: no worktree.
        let (heddle, _repo_dir, repo) = git_rig("modes");
        let d = heddle
            .declare_lease_mode(
                &repo.id, "t", "old style", vec!["src/**".into()], vec![], None,
                IsolationMode::InPlace,
            )
            .expect("in-place lease");
        assert!(d.thread.worktree.is_none());
        assert!(d.thread.base_stitch.is_none());
        assert_eq!(d.working_dir, repo.path);
        // A plain directory under Auto works in place too.
        let (heddle2, _dir2, repo2) = rig("modes-plain");
        let d2 = heddle2
            .declare_lease(&repo2.id, "t", "plain", vec!["src/**".into()], vec![], None)
            .expect("plain lease");
        assert!(d2.thread.worktree.is_none());
        // Demanding isolation on a plain directory fails AND rolls back.
        let err = heddle2
            .declare_lease_mode(
                &repo2.id, "t", "must isolate", vec!["src/**".into()], vec![], None,
                IsolationMode::Isolated,
            )
            .unwrap_err();
        assert!(err.contains("worktree setup failed"), "{err}");
        let snap = heddle2.snapshot();
        assert_eq!(
            snap.repo_states[&repo2.id].threads.len(),
            1,
            "the refused lease left nothing behind"
        );
    }

    #[test]
    fn deletions_are_tombstoned_applied_on_land_and_conflict_on_fabric_edit() {
        let (heddle, repo_dir, repo) = git_rig("del");
        // Thread 1 deletes util.rs in its worktree.
        let d = heddle
            .declare_lease(&repo.id, "t", "drop util", vec!["src/**".into()], vec![], None)
            .expect("lease");
        let wt = PathBuf::from(d.thread.worktree.as_ref().unwrap());
        std::fs::remove_file(wt.join("src/util.rs")).unwrap();
        let s = heddle.stitch(&d.lease.id).expect("stitch");
        assert_eq!(
            s.stitch.files.get("src/util.rs").map(String::as_str),
            Some(TOMBSTONE),
            "deletion recorded as a tombstone"
        );
        let p = heddle.propose(&d.thread.id).expect("propose");
        assert!(p.green);
        heddle.land_weave(&p.weave.id).expect("land");
        assert!(!repo_dir.join("src/util.rs").exists(), "weave applied the deletion");
        // Thread 2 deletes main.rs — but the fabric edits it meanwhile:
        // delete-vs-edit is a conflict, refused honestly.
        let d2 = heddle
            .declare_lease(&repo.id, "t", "drop main", vec!["src/**".into()], vec![], None)
            .expect("lease 2");
        let wt2 = PathBuf::from(d2.thread.worktree.as_ref().unwrap());
        std::fs::remove_file(wt2.join("src/main.rs")).unwrap();
        heddle.stitch(&d2.lease.id).expect("stitch 2");
        std::fs::write(repo_dir.join("src/main.rs"), "fn main() { /* moved */ }\n").unwrap();
        let p2 = heddle.propose(&d2.thread.id).expect("propose 2");
        assert!(p2.green);
        let err = heddle.land_weave(&p2.weave.id).unwrap_err();
        assert!(err.contains("fabric moved under you"), "{err}");
        assert!(repo_dir.join("src/main.rs").exists(), "nothing was deleted");
        // In-place threads track deletions vs their previous stitch too.
        let (heddle3, dir3, repo3) = rig("del-inplace");
        let d3 = heddle3
            .declare_lease(&repo3.id, "t", "prune", vec!["src/**".into()], vec![], None)
            .expect("lease 3");
        heddle3.stitch(&d3.lease.id).expect("stitch 3a");
        std::fs::remove_file(dir3.join("src/main.rs")).unwrap();
        let s3 = heddle3.stitch(&d3.lease.id).expect("stitch 3b");
        assert_eq!(
            s3.stitch.files.get("src/main.rs").map(String::as_str),
            Some(TOMBSTONE)
        );
    }

    #[test]
    fn adopt_hands_over_the_worktree_or_materializes_one() {
        let (heddle, _repo_dir, repo) = git_rig("adopt-wt");
        let d = heddle
            .declare_lease(
                &repo.id, "first", "half-done work", vec!["src/**".into()], vec![],
                Some(MIN_TTL_MS),
            )
            .expect("lease");
        let wt = d.thread.worktree.clone().expect("isolated");
        std::fs::write(
            PathBuf::from(&wt).join("src/main.rs"),
            "fn main() { /* half */ }\n",
        )
        .unwrap();
        heddle.stitch(&d.lease.id).expect("stitch");
        // The holder dies (heartbeat ages out); the thread orphans.
        heddle.with(|s, _| {
            s.repo_states.get_mut(&repo.id).unwrap().leases[0].last_heartbeat_ms = 1;
        });
        heddle.reconcile();
        // Adoption hands over the SAME worktree, work intact.
        let (thread, _lease) = heddle.adopt(&d.thread.id, "second").expect("adopt");
        assert_eq!(thread.worktree.as_deref(), Some(wt.as_str()));
        assert!(read(&PathBuf::from(&wt).join("src/main.rs")).contains("half"));
        // An orphan WITHOUT a worktree (imported from another machine)
        // gets one materialized at its head stitch.
        let imported = Thread {
            id: "thread-import-1".into(),
            repo_id: repo.id.clone(),
            goal: "imported work".into(),
            head_stitch: Some(
                heddle.snapshot().repo_states[&repo.id]
                    .stitches
                    .iter()
                    .rev()
                    .find(|st| st.thread_id == d.thread.id && st.files.values().any(|h| h != TOMBSTONE))
                    .unwrap()
                    .id
                    .clone(),
            ),
            lease_id: None,
            status: ThreadStatus::Orphaned,
            note: String::new(),
            approval_id: None,
            worktree: None,
            base_stitch: None,
        };
        // Re-point the head stitch at the imported thread id and give it a
        // lease record, the way sync's import_thread does.
        let lease = Lease {
            id: "lease-import-1".into(),
            thread_id: imported.id.clone(),
            scope: vec!["src/**".into()],
            goal: imported.goal.clone(),
            criteria: vec![],
            holder: "dead machine".into(),
            ttl_ms: MIN_TTL_MS,
            last_heartbeat_ms: 1,
        };
        let head_id = imported.head_stitch.clone().unwrap();
        heddle.with(|s, _| {
            let rs = s.repo_states.get_mut(&repo.id).unwrap();
            let mut st = rs.stitches.iter().find(|st| st.id == head_id).unwrap().clone();
            st.id = "stitch-import-1".into();
            st.thread_id = imported.id.clone();
            st.parent = None;
            rs.stitches.push(st);
        });
        let mut imported = imported;
        imported.head_stitch = Some("stitch-import-1".into());
        heddle.import_thread(&repo.id, imported, lease, vec![], "m-elsewhere")
            .expect("import");
        let (t2, _l2) = heddle.adopt("thread-import-1", "adopter").expect("adopt imported");
        let wt2 = t2.worktree.expect("worktree materialized");
        assert!(read(&PathBuf::from(&wt2).join("src/main.rs")).contains("half"));
        assert!(t2.base_stitch.is_some(), "based against OUR tree");
    }

    #[test]
    fn clean_removes_only_captured_woven_worktrees() {
        let (heddle, _repo_dir, repo) = git_rig("clean");
        let d = heddle
            .declare_lease(&repo.id, "t", "finish and clean", vec!["src/**".into()], vec![], None)
            .expect("lease");
        let wt = PathBuf::from(d.thread.worktree.as_ref().unwrap());
        std::fs::write(wt.join("src/main.rs"), "fn main() { /* done */ }\n").unwrap();
        heddle.stitch(&d.lease.id).expect("stitch");
        let p = heddle.propose(&d.thread.id).expect("propose");
        // Diverge BEFORE the weave lands: uncaptured bytes mean even the
        // automatic post-land sweep has to leave this worktree standing.
        std::fs::write(wt.join("src/main.rs"), "fn main() { /* uncaptured */ }\n").unwrap();
        heddle.land_weave(&p.weave.id).expect("land");
        assert!(wt.exists(), "auto-clean kept a worktree with uncaptured work");
        // A live thread's worktree is never cleaned.
        let d2 = heddle
            .declare_lease(&repo.id, "t", "still working", vec!["src/**".into()], vec![], None)
            .expect("lease 2");
        let report = heddle.clean_worktrees(&repo.id).expect("clean 1");
        assert!(report.removed.is_empty());
        assert!(report
            .skipped
            .iter()
            .any(|(tid, why)| tid == &d.thread.id && why.contains("uncaptured")));
        assert!(report
            .skipped
            .iter()
            .any(|(tid, why)| tid == &d2.thread.id && why.contains("in use")));
        // Restore the captured content: now the woven worktree goes.
        std::fs::write(wt.join("src/main.rs"), "fn main() { /* done */ }\n").unwrap();
        let report = heddle.clean_worktrees(&repo.id).expect("clean 2");
        assert!(report.removed.iter().any(|(tid, _)| tid == &d.thread.id));
        assert!(!wt.exists());
        let snap = heddle.snapshot();
        let t = snap.repo_states[&repo.id]
            .threads
            .iter()
            .find(|t| t.id == d.thread.id)
            .unwrap()
            .clone();
        assert!(t.worktree.is_none(), "forgotten after removal");
        // The live thread's worktree survived.
        assert!(PathBuf::from(d2.thread.worktree.as_ref().unwrap()).exists());
    }

    #[test]
    fn state_survives_a_cache_drop_and_a_corrupt_log_line() {
        let (heddle, _repo_dir, repo) = rig("persist");
        heddle.declare_lease(&repo.id, "t", "goal", vec!["src/**".into()], vec![], None)
            .expect("lease");
        // Corrupt the log mid-file; state.json untouched.
        let log = heddle.base().join(&repo.id).join("log.jsonl");
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(b"{ this is not json\n").unwrap();
        drop(f);
        heddle.reset_cache();
        let snap = heddle.snapshot();
        assert_eq!(snap.repos.len(), 1);
        assert_eq!(snap.repo_states[&repo.id].threads.len(), 1);
        // The readable events survive around the corrupt line.
        let events = store::read_events(&heddle.base(), &repo.id, 50);
        assert!(events.iter().any(|e| e["kind"] == "lease_declared"));
    }
}
