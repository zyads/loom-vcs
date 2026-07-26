// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! Multi-machine sync over any git remote — no server, no daemon, no new
//! protocol. If two machines can push to the same private git repo, they
//! can share a heddle.
//!
//! **The transport is git refs in a hidden namespace** (they never touch
//! branches, tags, or anyone's checkout):
//!
//! ```text
//! refs/heddle/<machine-id>/state    this machine's published heddle state:
//!                                 a commit whose tree is
//!                                   state.json          threads/leases/stitches
//!                                   objects/<sha256>    scoped file blobs
//! refs/heddle/fabric                THE shared fabric: fabric.json (ordered
//!                                 landed-weave entries, each with its apply
//!                                 manifest) + objects/ blobs
//! refs/heddle/claims/<thread-id>    orphan-adoption claims (first push wins)
//! refs/heddle/<machine-id>/mail/*   opaque mailbox payloads (see below)
//! ```
//!
//! **Fabric authority is a compare-and-swap ref push.** `refs/heddle/fabric`
//! advances only via `git push --force-with-lease=<ref>:<expected-sha>` —
//! the push succeeds only if the remote still has the value this machine
//! last fetched. Git's atomic ref update IS the shuttle token: no election,
//! no coordinator, and a lost race degrades into the same honest flow as a
//! local collision — "fabric moved — rebase and re-propose". Orphan claims
//! use the same primitive with an expected value of "absent": the earliest
//! claim wins deterministically, the loser is told who won.
//!
//! **Consent and visibility, stated plainly:** `heddle sync` shares this
//! repo's heddle metadata (goals, scopes, holders, thread status) AND the
//! scoped file content of stitches with the remote — the same exposure as
//! pushing a branch there. It runs only when you run it; `--auto` (sync
//! after every stitch/propose) is a per-repo opt-in flag you set yourself.
//! Syncing never touches branches and never lands anything: fabric entries
//! fetched from peers are applied to your tree because they ARE the shared
//! fabric your own weaves are measured against — the same rule as one
//! machine, now spanning several.
//!
//! **The mailbox** (`refs/heddle/<machine-id>/mail/<id>`) carries opaque
//! payloads: a `kind` string plus bytes Heddle never interprets or verifies —
//! sign them yourself if you need authenticity. It exists so higher layers
//! (e.g. an embedding host's team/consent envelopes) can ride the same
//! remote without teaching Heddle their formats. Nothing in this crate reads
//! mail content.
//!
//! Everything here shells out to the repo's own `git`; the engine in
//! `lib.rs` stays git-free and single-machine-correct without this module.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::{
    now_ms, store, worktree, Lease, Heddle, PeerSnapshot, RepoConfig, Stitch, Thread, ToeStep,
    VerifyOutcome, VerifyResult, Weave, TOMBSTONE,
};

/// The single shared fabric ref.
pub const FABRIC_REF: &str = "refs/heddle/fabric";

// ---------------------------------------------------------------------------
// Machine identity
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct MachineFile {
    id: String,
}

/// This data dir's stable machine id (`m-<12 hex>`), created on first use
/// and persisted in `<data>/machine.json`. Identity, not authentication —
/// signatures are future work and the docs say so.
pub fn machine_id(base: &Path) -> String {
    let path = base.join("machine.json");
    if let Some(m) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|b| serde_json::from_str::<MachineFile>(&b).ok())
    {
        if !m.id.trim().is_empty() {
            return m.id;
        }
    }
    let seed = format!("{}-{}-{:?}", std::process::id(), now_ms(), base);
    let id = format!("m-{}", &store::content_hash(seed.as_bytes())[..12]);
    let _ = std::fs::create_dir_all(base);
    let _ = std::fs::write(
        &path,
        serde_json::to_string_pretty(&MachineFile { id: id.clone() }).unwrap_or_default(),
    );
    id
}

// ---------------------------------------------------------------------------
// Published shapes
// ---------------------------------------------------------------------------

/// What one machine publishes about a repo (its `state.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PublishedState {
    pub machine: String,
    pub ts_ms: u64,
    #[serde(default)]
    pub threads: Vec<Thread>,
    #[serde(default)]
    pub leases: Vec<Lease>,
    #[serde(default)]
    pub stitches: Vec<Stitch>,
}

/// One landed weave on the shared fabric, with everything a peer needs to
/// replay it: the apply manifest (blobs ride in the same commit's tree).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FabricEntry {
    pub weave_id: String,
    pub thread_id: String,
    pub goal: String,
    pub machine: String,
    pub ts_ms: u64,
    pub verify_cmd: String,
    #[serde(default)]
    pub applied: BTreeMap<String, String>,
}

/// The shared fabric's `fabric.json`: landed entries, oldest first.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SharedFabric {
    pub entries: Vec<FabricEntry>,
}

/// What one `heddle sync` pass did — every field an honest count or note.
#[derive(Clone, Debug, Default)]
pub struct SyncOutcome {
    pub machine: String,
    pub remote: String,
    /// Weaves fetched from the shared fabric and applied to this tree.
    pub fabric_pulled: usize,
    /// Local weaves published to the shared fabric (CAS succeeded).
    pub fabric_pushed: usize,
    /// Set when the fabric CAS was refused (someone advanced it first):
    /// the honest "fabric moved" note. Run `heddle sync` again after
    /// rebasing/re-proposing local work.
    pub cas_refused: Option<String>,
    /// Peer machines whose state was fetched this pass.
    pub peers: Vec<String>,
    /// Cross-machine toe-steps recorded this pass.
    pub toe_steps: Vec<ToeStep>,
    /// Peer orphans visible after this pass (thread id, goal, machine).
    pub remote_orphans: Vec<(String, String, String)>,
}

// ---------------------------------------------------------------------------
// git plumbing (Command-based; output captured, stderr surfaced on error)
// ---------------------------------------------------------------------------

fn run_git(repo: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        // A deterministic identity for plumbing commits, without touching
        // the user's git config.
        .arg("-c")
        .arg("user.name=heddle-sync")
        .arg("-c")
        .arg("user.email=heddle@localhost")
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("cannot run git: {e}"))?;
    if let Some(bytes) = stdin {
        use std::io::Write;
        let mut pipe = child.stdin.take().expect("piped above");
        pipe.write_all(bytes).map_err(|e| format!("git stdin: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("git wait: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&"?"),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn git_str(repo: &Path, args: &[&str]) -> Result<String, String> {
    run_git(repo, args, None).map(|b| String::from_utf8_lossy(&b).trim().to_string())
}

fn ref_sha(repo: &Path, name: &str) -> Option<String> {
    git_str(repo, &["rev-parse", "--verify", "--quiet", name])
        .ok()
        .filter(|s| !s.is_empty())
}

/// Fetch every heddle ref from the remote (pruning ones deleted there).
/// A remote with no heddle refs yet fetches cleanly to nothing.
fn fetch_heddle_refs(repo: &Path, remote: &str) -> Result<(), String> {
    git_str(
        repo,
        &["fetch", "--prune", "--quiet", remote, "+refs/heddle/*:refs/heddle/*"],
    )
    .map(|_| ())
}

/// Read one file out of a heddle ref's commit tree.
fn read_at(repo: &Path, refname: &str, path: &str) -> Result<Vec<u8>, String> {
    run_git(repo, &["cat-file", "blob", &format!("{refname}:{path}")], None)
}

/// Build a heddle commit: `state_name` (a JSON blob) at the tree root plus an
/// `objects/` subtree holding the given content-addressed blobs (read from
/// the local heddle object store). Returns the commit sha.
fn write_heddle_commit(
    repo: &Path,
    file_name: &str,
    json: &[u8],
    objects_dir: &Path,
    blob_hashes: &BTreeSet<String>,
    msg: &str,
) -> Result<String, String> {
    let json_oid = String::from_utf8_lossy(&run_git(
        repo,
        &["hash-object", "-w", "--stdin"],
        Some(json),
    )?)
    .trim()
    .to_string();
    // Store every referenced blob as a git blob in ONE spawn.
    let mut paths = String::new();
    let mut present: Vec<&String> = Vec::new();
    for h in blob_hashes {
        if h == TOMBSTONE {
            continue;
        }
        let p = objects_dir.join(&h[..2]).join(h);
        if p.exists() {
            paths.push_str(&p.to_string_lossy());
            paths.push('\n');
            present.push(h);
        }
    }
    let mut tree_lines = String::new();
    if !present.is_empty() {
        let oids = run_git(
            repo,
            &["hash-object", "-w", "--stdin-paths"],
            Some(paths.as_bytes()),
        )?;
        let oids = String::from_utf8_lossy(&oids);
        let mut sub = String::new();
        for (h, oid) in present.iter().zip(oids.lines()) {
            sub.push_str(&format!("100644 blob {oid}\t{h}\n"));
        }
        let sub_oid = String::from_utf8_lossy(&run_git(
            repo,
            &["mktree"],
            Some(sub.as_bytes()),
        )?)
        .trim()
        .to_string();
        tree_lines.push_str(&format!("040000 tree {sub_oid}\tobjects\n"));
    }
    tree_lines.push_str(&format!("100644 blob {json_oid}\t{file_name}\n"));
    let tree_oid = String::from_utf8_lossy(&run_git(
        repo,
        &["mktree"],
        Some(tree_lines.as_bytes()),
    )?)
    .trim()
    .to_string();
    git_str(repo, &["commit-tree", &tree_oid, "-m", msg])
}

/// Push `sha` to `refname` on the remote. `expect: None` = overwrite freely
/// (our own namespace); `Some(sha)` = compare-and-swap against that value;
/// `Some("")` = the ref must not exist yet (claims).
fn push_ref(
    repo: &Path,
    remote: &str,
    sha: &str,
    refname: &str,
    expect: Option<&str>,
) -> Result<(), String> {
    let spec = format!("{sha}:{refname}");
    match expect {
        None => git_str(repo, &["push", "--quiet", remote, &format!("+{spec}")]).map(|_| ()),
        Some(old) => {
            let lease = format!("--force-with-lease={refname}:{old}");
            git_str(repo, &["push", "--quiet", &lease, remote, &spec]).map(|_| ())
        }
    }
}

// ---------------------------------------------------------------------------
// The sync pass
// ---------------------------------------------------------------------------

/// One full sync pass for a repo: fetch heddle refs, reconcile the shared
/// fabric (pull peers' landed weaves / publish ours via CAS), publish this
/// machine's state, refresh the peer view and cross-machine toe-steps.
///
/// `remote`/`auto` when given are persisted on the repo config first, so
/// `heddle sync --remote origin` once is enough. Blocking (git network I/O).
pub fn sync(
    engine: &Heddle,
    repo_id: &str,
    remote: Option<&str>,
    auto: Option<bool>,
) -> Result<SyncOutcome, String> {
    sync_opts(engine, repo_id, remote, auto, false)
}

/// [`sync`], plus the escape hatch for publishing to a remote anyone can read.
pub fn sync_opts(
    engine: &Heddle,
    repo_id: &str,
    remote: Option<&str>,
    auto: Option<bool>,
    allow_public: bool,
) -> Result<SyncOutcome, String> {
    let repo = if remote.is_some() || auto.is_some() {
        engine.set_sync(repo_id, remote.map(String::from), auto)?
    } else {
        engine
            .snapshot()
            .repos
            .into_iter()
            .find(|r| r.id == repo_id)
            .ok_or_else(|| format!("no registered repo with id {repo_id}"))?
    };
    let remote = repo
        .sync_remote
        .clone()
        .ok_or_else(|| "no sync remote configured — run `heddle sync --remote <name>` once".to_string())?;
    let repo_path = PathBuf::from(&repo.path);
    if !worktree::is_git_repo(&repo_path) {
        return Err("sync needs a git repo (the remote rides ordinary git refs)".into());
    }
    // Sync publishes the *content* of leased files so another machine can see
    // work in progress. On a repo the world can read, that is unfinished
    // source in public. Refuse rather than surprise someone.
    if !allow_public && repo.auto_sync && remote_is_world_readable(&repo_path, &remote) == Some(true) {
        return Err(format!(
            "'{remote}' can be read by anyone without signing in, and sync publishes the \
             contents of leased files there — that would put work in progress in public. \
             Turn it off with `heddle sync --auto off`, point sync at a private remote, or \
             pass --anyway if you really mean to publish it."
        ));
    }
    let machine = machine_id(&engine.base());
    let mut out = SyncOutcome {
        machine: machine.clone(),
        remote: remote.clone(),
        ..Default::default()
    };
    // 1. Fetch everything heddle on the remote.
    fetch_heddle_refs(&repo_path, &remote)?;
    // 2. Reconcile the shared fabric.
    reconcile_fabric(engine, &repo, &repo_path, &remote, &machine, &mut out)?;
    // 3. Publish this machine's state.
    publish_state(engine, &repo, &repo_path, &remote, &machine)?;
    // 4. Refresh the peer view.
    import_peers(engine, &repo, &repo_path, &machine, &mut out)?;
    Ok(out)
}

fn read_shared_fabric(repo_path: &Path) -> SharedFabric {
    read_at(repo_path, FABRIC_REF, "fabric.json")
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn reconcile_fabric(
    engine: &Heddle,
    repo: &RepoConfig,
    repo_path: &Path,
    remote: &str,
    machine: &str,
    out: &mut SyncOutcome,
) -> Result<(), String> {
    let remote_sha = ref_sha(repo_path, FABRIC_REF);
    let shared = read_shared_fabric(repo_path);
    let remote_ids: Vec<String> = shared.entries.iter().map(|e| e.weave_id.clone()).collect();
    let snap = engine.snapshot();
    let rs = snap.repo_states.get(&repo.id).cloned().unwrap_or_default();
    let local_ids = rs.fabric.history.clone();
    let objects = store::objects_dir(&engine.base(), &repo.id);

    if remote_ids.starts_with(&local_ids) && remote_ids.len() > local_ids.len() {
        // Peers landed weaves we haven't seen: materialize their blobs from
        // the fabric commit, then replay them onto this tree.
        let fresh = &shared.entries[local_ids.len()..];
        let mut weaves = Vec::new();
        for e in fresh {
            for h in e.applied.values().filter(|h| *h != TOMBSTONE) {
                let bytes = read_at(repo_path, FABRIC_REF, &format!("objects/{h}"))
                    .map_err(|err| format!("fabric blob {h} unavailable: {err}"))?;
                store::put_blob(&objects, &bytes)?;
            }
            weaves.push(Weave {
                id: e.weave_id.clone(),
                thread_id: e.thread_id.clone(),
                fabric_parent: None,
                verify: VerifyOutcome {
                    cmd: e.verify_cmd.clone(),
                    result: VerifyResult::Green,
                    log_tail: format!("verified green on {} before landing", e.machine),
                },
                ts_ms: e.ts_ms,
                applied: e.applied.clone(),
            });
        }
        out.fabric_pulled = engine.import_fabric_weaves(&repo.id, weaves, "sync")?;
        return Ok(());
    }
    if local_ids.starts_with(&remote_ids) && local_ids.len() > remote_ids.len() {
        // We are ahead: publish our unshared weaves behind the CAS.
        let mut entries = shared.entries.clone();
        let mut blob_set: BTreeSet<String> = entries
            .iter()
            .flat_map(|e| e.applied.values().cloned())
            .collect();
        for wid in &local_ids[remote_ids.len()..] {
            let w = rs
                .weaves
                .iter()
                .find(|w| w.id == *wid)
                .ok_or_else(|| format!("weave {wid} was pruned locally; cannot publish it"))?;
            let goal = rs
                .threads
                .iter()
                .find(|t| t.id == w.thread_id)
                .map(|t| t.goal.clone())
                .unwrap_or_default();
            blob_set.extend(w.applied.values().cloned());
            entries.push(FabricEntry {
                weave_id: w.id.clone(),
                thread_id: w.thread_id.clone(),
                goal,
                machine: machine.to_string(),
                ts_ms: w.ts_ms,
                verify_cmd: w.verify.cmd.clone(),
                applied: w.applied.clone(),
            });
        }
        let pushed = local_ids.len() - remote_ids.len();
        let json = serde_json::to_vec_pretty(&SharedFabric { entries })
            .map_err(|e| format!("fabric json: {e}"))?;
        let commit = write_heddle_commit(
            repo_path,
            "fabric.json",
            &json,
            &objects,
            &blob_set,
            &format!("heddle fabric: {} weaves", local_ids.len()),
        )?;
        let expect = remote_sha.as_deref().unwrap_or("");
        match push_ref(repo_path, remote, &commit, FABRIC_REF, Some(expect)) {
            Ok(()) => out.fabric_pushed = pushed,
            Err(e) => {
                // Lost the CAS race: fetch what won and say so honestly.
                let _ = fetch_heddle_refs(repo_path, remote);
                out.cas_refused = Some(format!(
                    "the shared fabric moved before this machine's weaves could land on it \
                     — synced the newer fabric instead; rebase open threads and re-propose \
                     ({e})"
                ));
            }
        }
        return Ok(());
    }
    if local_ids == remote_ids {
        return Ok(()); // in step — nothing to move either way
    }
    Err(format!(
        "this machine's fabric and the shared fabric have DIVERGED (local has {} weaves, \
         shared has {}, and neither extends the other). Heddle will not guess: land no more \
         weaves here, `heddle sync` from the machine that owns the missing history, or \
         re-register this repo to start its local fabric from the shared one.",
        local_ids.len(),
        remote_ids.len()
    ))
}

fn publish_state(
    engine: &Heddle,
    repo: &RepoConfig,
    repo_path: &Path,
    remote: &str,
    machine: &str,
) -> Result<(), String> {
    let snap = engine.snapshot();
    let rs = snap.repo_states.get(&repo.id).cloned().unwrap_or_default();
    let state = PublishedState {
        machine: machine.to_string(),
        ts_ms: now_ms(),
        threads: rs.threads.clone(),
        leases: rs.leases.clone(),
        stitches: rs.stitches.clone(),
    };
    let blob_set: BTreeSet<String> = rs
        .stitches
        .iter()
        .flat_map(|st| st.files.values().cloned())
        .collect();
    let json = serde_json::to_vec_pretty(&state).map_err(|e| format!("state json: {e}"))?;
    let objects = store::objects_dir(&engine.base(), &repo.id);
    let commit = write_heddle_commit(
        repo_path,
        "state.json",
        &json,
        &objects,
        &blob_set,
        &format!("heddle state from {machine}"),
    )?;
    push_ref(
        repo_path,
        remote,
        &commit,
        &format!("refs/heddle/{machine}/state"),
        None,
    )
}

fn peer_state_refs(repo_path: &Path, machine: &str) -> Vec<(String, String)> {
    let Ok(body) = git_str(
        repo_path,
        &["for-each-ref", "refs/heddle", "--format=%(refname)"],
    ) else {
        return Vec::new();
    };
    body.lines()
        .filter_map(|r| {
            let segs: Vec<&str> = r.split('/').collect();
            // refs/heddle/<machine>/state
            if segs.len() == 4 && segs[3] == "state" && segs[2] != machine {
                Some((segs[2].to_string(), r.to_string()))
            } else {
                None
            }
        })
        .collect()
}

fn import_peers(
    engine: &Heddle,
    repo: &RepoConfig,
    repo_path: &Path,
    machine: &str,
    out: &mut SyncOutcome,
) -> Result<(), String> {
    let mut snapshots = Vec::new();
    let now = now_ms();
    for (peer_machine, refname) in peer_state_refs(repo_path, machine) {
        let Ok(bytes) = read_at(repo_path, &refname, "state.json") else {
            continue;
        };
        let Ok(state) = serde_json::from_slice::<PublishedState>(&bytes) else {
            continue;
        };
        for t in &state.threads {
            let lease_dead = t
                .lease_id
                .as_ref()
                .and_then(|lid| state.leases.iter().find(|l| l.id == *lid))
                .map(|l| l.expired(now))
                .unwrap_or(true);
            let adoptable = t.status == crate::ThreadStatus::Orphaned
                || (t.status.is_live() && lease_dead);
            if adoptable {
                out.remote_orphans
                    .push((t.id.clone(), t.goal.clone(), peer_machine.clone()));
            }
        }
        out.peers.push(peer_machine.clone());
        snapshots.push(PeerSnapshot {
            machine: peer_machine,
            ts_ms: state.ts_ms,
            fetched_ms: now,
            threads: state.threads,
            leases: state.leases,
        });
    }
    out.toe_steps = engine.update_peers(&repo.id, snapshots)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Cross-machine orphan adoption (claims CAS)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct Claim {
    thread: String,
    machine: String,
    holder: String,
    ts_ms: u64,
}

/// Adopt an orphan that lives on ANOTHER machine's published state: claim
/// it with a first-push-wins CAS on `refs/heddle/claims/<thread-id>`, then
/// import thread + lease + stitches (blobs included) and run the normal
/// local adoption — fresh worktree, head stitch materialized.
///
/// Losing the claim race is an honest error naming the winner. The dead
/// machine's own copy of the thread is untouched (it will see the claim on
/// its next sync; v1 leaves acting on that to the humans involved).
pub fn adopt_remote(
    engine: &Heddle,
    repo_id: &str,
    thread_id: &str,
    holder: &str,
) -> Result<(Thread, Lease), String> {
    let repo = engine
        .snapshot()
        .repos
        .into_iter()
        .find(|r| r.id == repo_id)
        .ok_or_else(|| format!("no registered repo with id {repo_id}"))?;
    let remote = repo
        .sync_remote
        .clone()
        .ok_or_else(|| "no sync remote configured — run `heddle sync --remote <name>` first".to_string())?;
    let repo_path = PathBuf::from(&repo.path);
    let machine = machine_id(&engine.base());
    fetch_heddle_refs(&repo_path, &remote)?;
    // Find which peer published this thread.
    let mut found: Option<(String, String, PublishedState)> = None;
    for (peer_machine, refname) in peer_state_refs(&repo_path, &machine) {
        if let Ok(bytes) = read_at(&repo_path, &refname, "state.json") {
            if let Ok(state) = serde_json::from_slice::<PublishedState>(&bytes) {
                if state.threads.iter().any(|t| t.id == thread_id) {
                    found = Some((peer_machine, refname, state));
                    break;
                }
            }
        }
    }
    let (peer_machine, refname, state) = found.ok_or_else(|| {
        format!("no peer machine has published a thread {thread_id} — `heddle sync` first?")
    })?;
    let thread = state
        .threads
        .iter()
        .find(|t| t.id == thread_id)
        .cloned()
        .expect("found above");
    let lease = thread
        .lease_id
        .as_ref()
        .and_then(|lid| state.leases.iter().find(|l| l.id == *lid))
        .cloned()
        .ok_or_else(|| format!("peer state for {thread_id} carries no lease record"))?;
    let now = now_ms();
    if thread.status != crate::ThreadStatus::Orphaned
        && !(thread.status.is_live() && lease.expired(now))
    {
        return Err(format!(
            "thread {thread_id} on {peer_machine} is {:?} with a live lease — not adoptable",
            thread.status
        ));
    }
    // The claim: first CAS push wins.
    let claim = Claim {
        thread: thread_id.to_string(),
        machine: machine.clone(),
        holder: holder.to_string(),
        ts_ms: now,
    };
    let json = serde_json::to_vec_pretty(&claim).map_err(|e| format!("claim json: {e}"))?;
    let objects = store::objects_dir(&engine.base(), &repo.id);
    let commit = write_heddle_commit(
        &repo_path,
        "claim.json",
        &json,
        &objects,
        &BTreeSet::new(),
        &format!("heddle claim: {thread_id} by {machine}"),
    )?;
    let claim_ref = format!("refs/heddle/claims/{thread_id}");
    if let Err(e) = push_ref(&repo_path, &remote, &commit, &claim_ref, Some("")) {
        let _ = fetch_heddle_refs(&repo_path, &remote);
        let winner = read_at(&repo_path, &claim_ref, "claim.json")
            .ok()
            .and_then(|b| serde_json::from_slice::<Claim>(&b).ok())
            .map(|c| format!("{} (holder {})", c.machine, c.holder))
            .unwrap_or_else(|| "another machine".to_string());
        return Err(format!(
            "thread {thread_id} was already claimed by {winner} — the claim ref decided \
             the race; pick another orphan ({e})"
        ));
    }
    // We own it: bring the stitches (and their blobs) home.
    let stitches: Vec<Stitch> = state
        .stitches
        .iter()
        .filter(|st| st.thread_id == thread_id)
        .cloned()
        .collect();
    for st in &stitches {
        for h in st.files.values().filter(|h| *h != TOMBSTONE) {
            let bytes = read_at(&repo_path, &refname, &format!("objects/{h}"))
                .map_err(|e| format!("peer blob {h} unavailable: {e}"))?;
            store::put_blob(&objects, &bytes)?;
        }
    }
    engine.import_thread(repo_id, thread, lease, stitches, &peer_machine)?;
    engine.adopt(thread_id, holder)
}

// ---------------------------------------------------------------------------
// Mailbox — opaque payloads for higher layers
// ---------------------------------------------------------------------------

/// One mailbox item, payload untouched and unverified by Heddle.
#[derive(Clone, Debug)]
pub struct MailItem {
    pub machine: String,
    pub kind: String,
    pub ts_ms: u64,
    pub payload: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct MailMeta {
    kind: String,
    ts_ms: u64,
}

/// Publish an opaque payload under this machine's mail namespace. Heddle
/// never reads, interprets or signs it — callers wanting authenticity sign
/// the bytes themselves.
pub fn mail_send(
    engine: &Heddle,
    repo_id: &str,
    kind: &str,
    payload: &[u8],
) -> Result<String, String> {
    let repo = engine
        .snapshot()
        .repos
        .into_iter()
        .find(|r| r.id == repo_id)
        .ok_or_else(|| format!("no registered repo with id {repo_id}"))?;
    let remote = repo
        .sync_remote
        .clone()
        .ok_or_else(|| "no sync remote configured".to_string())?;
    let repo_path = PathBuf::from(&repo.path);
    let machine = machine_id(&engine.base());
    let now = now_ms();
    let meta = serde_json::to_vec_pretty(&MailMeta {
        kind: kind.to_string(),
        ts_ms: now,
    })
    .map_err(|e| format!("mail meta: {e}"))?;
    // A tiny two-file tree: mail.json + payload.
    let meta_oid = String::from_utf8_lossy(&run_git(
        &repo_path,
        &["hash-object", "-w", "--stdin"],
        Some(&meta),
    )?)
    .trim()
    .to_string();
    let payload_oid = String::from_utf8_lossy(&run_git(
        &repo_path,
        &["hash-object", "-w", "--stdin"],
        Some(payload),
    )?)
    .trim()
    .to_string();
    let tree = format!("100644 blob {meta_oid}\tmail.json\n100644 blob {payload_oid}\tpayload\n");
    let tree_oid = String::from_utf8_lossy(&run_git(
        &repo_path,
        &["mktree"],
        Some(tree.as_bytes()),
    )?)
    .trim()
    .to_string();
    let commit = git_str(
        &repo_path,
        &["commit-tree", &tree_oid, "-m", &format!("heddle mail: {kind}")],
    )?;
    let refname = format!("refs/heddle/{machine}/mail/{now}");
    push_ref(&repo_path, &remote, &commit, &refname, None)?;
    Ok(refname)
}

/// Fetch and list every peer's mailbox items (payloads included). Read-only.
pub fn mail_list(engine: &Heddle, repo_id: &str) -> Result<Vec<MailItem>, String> {
    let repo = engine
        .snapshot()
        .repos
        .into_iter()
        .find(|r| r.id == repo_id)
        .ok_or_else(|| format!("no registered repo with id {repo_id}"))?;
    let remote = repo
        .sync_remote
        .clone()
        .ok_or_else(|| "no sync remote configured".to_string())?;
    let repo_path = PathBuf::from(&repo.path);
    fetch_heddle_refs(&repo_path, &remote)?;
    let Ok(body) = git_str(
        &repo_path,
        &["for-each-ref", "refs/heddle", "--format=%(refname)"],
    ) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for r in body.lines() {
        let segs: Vec<&str> = r.split('/').collect();
        // refs/heddle/<machine>/mail/<id>
        if segs.len() == 5 && segs[3] == "mail" {
            let Ok(meta_bytes) = read_at(&repo_path, r, "mail.json") else {
                continue;
            };
            let Ok(meta) = serde_json::from_slice::<MailMeta>(&meta_bytes) else {
                continue;
            };
            let payload = read_at(&repo_path, r, "payload").unwrap_or_default();
            out.push(MailItem {
                machine: segs[2].to_string(),
                kind: meta.kind,
                ts_ms: meta.ts_ms,
                payload,
            });
        }
    }
    out.sort_by_key(|m| m.ts_ms);
    Ok(out)
}

/// Run a sync when the repo opted into `--auto`; quiet no-op otherwise.
/// Failures are returned for the caller to PRINT, never to abort the local
/// operation that triggered them — local work must not hostage on a remote.
pub fn maybe_autosync(engine: &Heddle, repo: &RepoConfig) -> Option<Result<SyncOutcome, String>> {
    if repo.auto_sync && repo.sync_remote.is_some() {
        Some(sync(engine, &repo.id, None, None))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IsolationMode, ThreadStatus, MIN_TTL_MS};

    fn scratch(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "heddle-sync-{tag}-{}-{}-{}",
            std::process::id(),
            now_ms(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mk scratch");
        p
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
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

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    /// One clone of the shared bare remote, with its own heddle data dir.
    fn machine(tag: &str, bare: &Path) -> (Heddle, PathBuf, RepoConfig) {
        let dir = scratch(&format!("{tag}-repo"));
        git_ok(
            dir.parent().unwrap(),
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                dir.to_str().unwrap(),
            ],
        );
        git_ok(&dir, &["config", "user.email", "heddle@test"]);
        git_ok(&dir, &["config", "user.name", "heddle test"]);
        let engine = Heddle::at(scratch(&format!("{tag}-data")));
        let repo = engine
            .register_repo(dir.to_str().unwrap(), Some("true".into()), false)
            .expect("register");
        (engine, dir, repo)
    }

    /// A bare remote seeded with one commit (f.txt + extra.txt).
    fn bare_remote(tag: &str) -> PathBuf {
        let src = scratch(&format!("{tag}-src"));
        std::fs::write(src.join("f.txt"), "base\n").unwrap();
        std::fs::write(src.join("extra.txt"), "base-extra\n").unwrap();
        git_ok(&src, &["init", "-q"]);
        git_ok(&src, &["config", "user.email", "heddle@test"]);
        git_ok(&src, &["config", "user.name", "heddle test"]);
        git_ok(&src, &["add", "-A"]);
        git_ok(&src, &["commit", "-q", "-m", "base"]);
        let bare = scratch(&format!("{tag}-bare"));
        let _ = std::fs::remove_dir_all(&bare);
        git_ok(
            src.parent().unwrap(),
            &[
                "clone",
                "--bare",
                "-q",
                src.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        bare
    }

    #[test]
    fn two_machines_share_fabric_leases_and_the_honest_rebase_flow() {
        let bare = bare_remote("pair");
        let (a, a_dir, ra) = machine("pair-a", &bare);
        let (b, b_dir, rb) = machine("pair-b", &bare);

        // A starts work on f.txt, plus a second broad lease that stays live
        // (so B can see a cross-machine toe-step later).
        let da = a
            .declare_lease(&ra.id, "alice", "rework f", vec!["f.txt".into()], vec![], None)
            .expect("lease a");
        let wa = PathBuf::from(da.thread.worktree.as_ref().expect("isolated"));
        std::fs::write(wa.join("f.txt"), "alice\n").unwrap();
        a.stitch(&da.lease.id).expect("stitch a");
        a.declare_lease(&ra.id, "alice", "audit everything", vec!["**".into()], vec![], None)
            .expect("lease a2");

        // B leases the SAME file before ever syncing — its base predates
        // everything A is about to land.
        let db = b
            .declare_lease(&rb.id, "bob", "restyle f", vec!["f.txt".into()], vec![], None)
            .expect("lease b");
        let wb = PathBuf::from(db.thread.worktree.as_ref().expect("isolated"));
        std::fs::write(wb.join("f.txt"), "bob\n").unwrap();
        b.stitch(&db.lease.id).expect("stitch b");

        // A lands and syncs: one weave onto the shared fabric (CAS from
        // "absent"), state published.
        let pa = a.propose(&da.thread.id).expect("propose a");
        a.land_weave(&pa.weave.id).expect("land a");
        let sa = sync(&a, &ra.id, Some("origin"), None).expect("sync a");
        assert_eq!(sa.fabric_pushed, 1);
        assert!(sa.cas_refused.is_none());

        // B syncs: pulls A's weave into ITS tree, sees A as a peer, and is
        // warned that A's live lease overlaps B's.
        let sb = sync(&b, &rb.id, Some("origin"), None).expect("sync b");
        assert_eq!(sb.fabric_pulled, 1);
        assert_eq!(read(&b_dir.join("f.txt")), "alice\n", "A's land replayed on B");
        assert_eq!(sb.peers, vec![sa.machine.clone()]);
        assert!(
            sb.toe_steps.iter().any(|t| t.goal_b.contains("audit everything")),
            "cross-machine toe-step names the peer's goal: {:?}",
            sb.toe_steps
        );
        // B's worktree is untouched by the pull — isolation holds.
        assert_eq!(read(&wb.join("f.txt")), "bob\n");

        // B proposes green, but landing refuses: the fabric moved under it.
        let pb = b.propose(&db.thread.id).expect("propose b");
        assert!(pb.green);
        let err = b.land_weave(&pb.weave.id).unwrap_err();
        assert!(err.contains("fabric moved under you"), "{err}");
        // Rebase, reconcile by hand, land, sync — the CAS accepts (nothing
        // else moved).
        let rbo = b.rebase_thread(&db.thread.id).expect("rebase b");
        assert_eq!(rbo.conflicts, vec!["f.txt".to_string()]);
        std::fs::write(wb.join("f.txt"), "alice+bob\n").unwrap();
        b.stitch(&db.lease.id).expect("stitch b2");
        let pb2 = b.propose(&db.thread.id).expect("propose b2");
        b.land_weave(&pb2.weave.id).expect("land b2");
        let sb2 = sync(&b, &rb.id, None, None).expect("sync b2");
        assert_eq!(sb2.fabric_pushed, 1);
        assert!(sb2.cas_refused.is_none());

        // A syncs: B's weave replays onto A's tree.
        let sa2 = sync(&a, &ra.id, None, None).expect("sync a2");
        assert_eq!(sa2.fabric_pulled, 1);
        assert_eq!(read(&a_dir.join("f.txt")), "alice+bob\n");

        // The CAS itself: advancing the fabric ref with a stale expected
        // value is refused by git — this is the shuttle token. (The new
        // commit reuses the current tree; only the ref update matters.)
        let current = ref_sha(&a_dir, FABRIC_REF).expect("fabric ref exists locally");
        let tree = git_str(&a_dir, &["rev-parse", &format!("{current}^{{tree}}")]).unwrap();
        let next = git_str(&a_dir, &["commit-tree", &tree, "-m", "cas probe"]).unwrap();
        let stale = "0000000000000000000000000000000000000001";
        let e = push_ref(&a_dir, "origin", &next, FABRIC_REF, Some(stale)).unwrap_err();
        assert!(e.contains("failed"), "stale CAS refused: {e}");
        // And with the TRUE expected value the same update succeeds.
        push_ref(&a_dir, "origin", &next, FABRIC_REF, Some(&current)).expect("CAS ok");
    }

    #[test]
    fn cross_machine_orphan_adoption_first_claim_wins_the_race() {
        let bare = bare_remote("orphan");
        let (a, _a_dir, ra) = machine("orphan-a", &bare);
        let (b, _b_dir, rb) = machine("orphan-b", &bare);
        let (c, _c_dir, rc) = machine("orphan-c", &bare);

        // A starts work, stitches once, then dies (heartbeat ages out).
        let da = a
            .declare_lease(
                &ra.id,
                "alice",
                "migrate the config",
                vec!["f.txt".into()],
                vec!["keys renamed".into()],
                Some(MIN_TTL_MS),
            )
            .expect("lease a");
        let wa = PathBuf::from(da.thread.worktree.as_ref().unwrap());
        std::fs::write(wa.join("f.txt"), "half-done\n").unwrap();
        a.stitch(&da.lease.id).expect("stitch a");
        a.with(|s, _| {
            s.repo_states.get_mut(&ra.id).unwrap().leases[0].last_heartbeat_ms = 1;
        });
        sync(&a, &ra.id, Some("origin"), None).expect("sync a (publishes the orphan)");

        // B and C both learn about it.
        let sb = sync(&b, &rb.id, Some("origin"), None).expect("sync b");
        sync(&c, &rc.id, Some("origin"), None).expect("sync c");
        assert!(
            sb.remote_orphans
                .iter()
                .any(|(tid, goal, _)| tid == &da.thread.id && goal.contains("migrate")),
            "orphan visible with its goal: {:?}",
            sb.remote_orphans
        );

        // B claims first and wins: thread imported, adopted, the last
        // stitch materialized into a fresh local worktree.
        let (tb, lb) = adopt_remote(&b, &rb.id, &da.thread.id, "bee").expect("b adopts");
        assert_eq!(tb.status, ThreadStatus::Adopted);
        assert_eq!(lb.goal, "migrate the config");
        assert_eq!(lb.criteria, vec!["keys renamed".to_string()]);
        assert_eq!(lb.holder, "bee");
        let wtb = tb.worktree.expect("worktree materialized on adopt");
        assert_eq!(read(&PathBuf::from(&wtb).join("f.txt")), "half-done\n");

        // C's claim loses the CAS deterministically and is told who won.
        let e = adopt_remote(&c, &rc.id, &da.thread.id, "cee").unwrap_err();
        assert!(e.contains("already claimed"), "{e}");
        assert!(e.contains("bee"), "the loser learns the winner: {e}");

        // B continues the work to done: stitch → propose → land → sync.
        std::fs::write(PathBuf::from(&wtb).join("f.txt"), "done\n").unwrap();
        b.stitch(&lb.id).expect("stitch b");
        let pb = b.propose(&tb.id).expect("propose b");
        assert!(pb.green);
        b.land_weave(&pb.weave.id).expect("land b");
        let sb2 = sync(&b, &rb.id, None, None).expect("sync b2");
        assert_eq!(sb2.fabric_pushed, 1);
        // In-place lease on a machine, isolated modes etc. are covered in
        // lib tests; here the point is the claim decided the race.
        let _ = IsolationMode::Auto;
    }
}

// ---------------------------------------------------------------------------
// Is this remote readable by the whole world?
// ---------------------------------------------------------------------------

/// Turn a remote URL into the address an anonymous stranger would use, or
/// `None` when we cannot tell (a local path, an unfamiliar scheme).
///
/// `git@github.com:owner/repo.git` and `https://user@github.com/owner/repo`
/// both become `https://github.com/owner/repo.git`.
pub fn anonymous_url(url: &str) -> Option<String> {
    let u = url.trim();
    if u.is_empty() || u.starts_with('/') || u.starts_with('.') || u.starts_with("file:") {
        return None;
    }
    // scp-like: [user@]host:path
    if !u.contains("://") {
        let (hostpart, path) = u.split_once(':')?;
        let host = hostpart.rsplit('@').next()?;
        if host.is_empty() || path.is_empty() {
            return None;
        }
        return Some(format!("https://{host}/{}", path.trim_start_matches('/')));
    }
    let (scheme, rest) = u.split_once("://")?;
    if !matches!(scheme, "https" | "http" | "ssh" | "git") {
        return None;
    }
    let (hostpart, path) = rest.split_once('/')?;
    let host = hostpart.rsplit('@').next()?; // drop any userinfo
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("https://{host}/{path}"))
}

/// Ask the remote **with every credential path shut off**. If it still answers,
/// anybody can read it — which means anybody can read the file content sync
/// publishes there.
///
/// `None` means "could not tell" (offline, unfamiliar URL). A caller must treat
/// that as unknown rather than as safe.
pub fn remote_is_world_readable(repo: &Path, remote_name: &str) -> Option<bool> {
    let url = String::from_utf8(
        run_git(repo, &["remote", "get-url", remote_name], None).ok()?,
    )
    .ok()?;
    let anon = anonymous_url(url.trim())?;

    let out = Command::new("git")
        .arg("-c")
        .arg("credential.helper=")
        // No `-h`: that restricts to branch heads, HEAD is not one, and the
        // empty match read as "could not tell" — so the guard never fired.
        .args(["ls-remote", "--exit-code", &anon, "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    if out.status.success() {
        return Some(true);
    }
    let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
    // A refusal we recognise means it is genuinely gated. Anything else
    // (no network, DNS failure) is "could not tell".
    if err.contains("authentication")
        || err.contains("could not read username")
        || err.contains("terminal prompts disabled")
        || err.contains("not found")
        || err.contains("403")
        || err.contains("permission denied")
    {
        return Some(false);
    }
    None
}

#[cfg(test)]
mod public_remote_tests {
    use super::anonymous_url;

    #[test]
    fn an_ssh_remote_becomes_the_address_a_stranger_would_try() {
        assert_eq!(
            anonymous_url("git@github.com:zyads/heddle.git").as_deref(),
            Some("https://github.com/zyads/heddle.git")
        );
    }

    #[test]
    fn a_username_in_the_url_is_dropped_so_the_probe_is_anonymous() {
        // Leaving the userinfo in would let the probe authenticate and report
        // a private repo as world-readable — the exact wrong answer.
        assert_eq!(
            anonymous_url("https://zman@github.com/zyads/aether.git").as_deref(),
            Some("https://github.com/zyads/aether.git")
        );
        assert_eq!(
            anonymous_url("ssh://git@gitlab.com/team/thing").as_deref(),
            Some("https://gitlab.com/team/thing")
        );
    }

    #[test]
    fn a_local_path_is_not_something_we_can_ask_the_world_about() {
        assert_eq!(anonymous_url("/srv/git/thing.git"), None);
        assert_eq!(anonymous_url("../peer"), None);
        assert_eq!(anonymous_url("file:///srv/git/thing"), None);
        assert_eq!(anonymous_url(""), None);
    }
}
