// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! The standalone `heddle` binary — solo/CLI front-end over the engine in
//! `lib.rs`, plus `heddle mcp`, a stdio MCP server for agent sessions.
//!
//! Verbs: `init · config · lease · stitch · propose · export · withdraw ·
//! adopt · status · log · mcp`. State lives under `~/.heddle` (override with
//! `HEDDLE_DATA`).
//! The current lease per repo is remembered in a solo-mode pointer
//! (`solo.json`) so `stitch`/`propose`/`withdraw` need no ids.
//!
//! The honesty rules the engine enforces hold here too: `propose` runs the
//! verify in a scratch copy and, on green, asks YOU at the terminal before
//! anything touches the working tree. Non-interactive stdin means the apply
//! is refused, with the reason recorded on the thread.

use std::process::ExitCode;

use heddle::consent::{TerminalConsent, WeaveDisposition};
use heddle::{solo, store, sync, IsolationMode, Heddle, RepoConfig, ThreadStatus};

mod mcp;

const USAGE: &str = "\
heddle — version control for many hands moving at once (docs/DESIGN.md)

usage:
  heddle init [--verify CMD] [--git-bridge]     register the current directory
       [--bridge-mode squash|stitches|both]   (bridge granularity; default
                                              squash — one commit per weave)
                                              CMD runs on EVERY propose and
                                              every agent waits for it: point
                                              it at a fast subset (a few
                                              seconds), not the full suite —
                                              leave that to CI.
  heddle config [--bridge-mode MODE]            show this repo's config, or set
                                              the git-bridge granularity
  heddle lease \"<goal>\" <scope...>              declare an intent lease; on a git
       [--criteria TEXT]... [--ttl-ms N]      repo the thread gets its OWN
       [--isolated | --in-place]              worktree — edit there
  heddle stitch [--lease ID]                    checkpoint the leased scope
  heddle propose [--thread ID]                  verify in a scratch copy; green
                                              asks you before applying
  heddle export [--thread ID]                   write the thread's stitch chain to
                                              its draft branch heddle/<id>-<goal>
                                              for review — nothing lands
  heddle rebase [--thread ID]                   refresh the thread's worktree from
                                              the fabric (after \"fabric moved\")
  heddle withdraw                               return a proposed thread to active
  heddle adopt <thread-id>                      take over an orphaned thread
                                              (local, or a synced peer's)
  heddle clean                                  remove worktrees of woven threads
                                              (refuses uncaptured divergence)
  heddle sync [--remote NAME] [--auto on|off]   sync leases/threads/fabric with a
       [--anyway]                             git remote. Publishes the content
                                              of leased files there, so it
                                              refuses a remote anyone can read
                                              unless you pass --anyway.
  heddle status                                 threads, leases, orphans, peers
  heddle log                                    fabric history + recent events
  heddle mcp                                    stdio MCP server (heddle_status,
                                              heddle_lease, heddle_stitch,
                                              heddle_propose, heddle_rebase,
                                              heddle_adopt)

state: ~/.heddle (override with HEDDLE_DATA)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verb = args.first().map(String::as_str).unwrap_or("");
    // `&args[1..]` panics when there are no arguments at all — and running
    // `heddle` bare is exactly what the docs tell people to do to see the
    // command list, so the first thing a new user did was watch it crash.
    let rest = args.get(1..).unwrap_or(&[]);
    let out = match verb {
        "init" => cmd_init(rest),
        "config" => cmd_config(rest),
        "lease" => cmd_lease(rest),
        "stitch" => cmd_stitch(rest),
        "propose" => cmd_propose(rest),
        "export" => cmd_export(rest),
        "rebase" => cmd_rebase(rest),
        "withdraw" => cmd_withdraw(),
        "adopt" => cmd_adopt(rest),
        "clean" => cmd_clean(),
        "sync" => cmd_sync(rest),
        "status" => cmd_status(),
        "log" => cmd_log(),
        "mcp" => {
            mcp::serve(store());
            Ok(())
        }
        "help" | "--help" | "-h" | "" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown verb '{other}'\n\n{USAGE}")),
    };
    match out {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("heddle: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Who holds leases declared from this terminal.
fn holder() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| "solo".to_string())
}

/// The registered repo containing the current directory.
fn current_repo(engine: &Heddle) -> Result<RepoConfig, String> {
    engine.repo_containing(".").ok_or_else(|| {
        "no registered repo contains this directory — run `heddle init` here first".to_string()
    })
}

/// The solo pointer for the current repo, validated against the engine.
fn current_pointer(engine: &Heddle) -> Result<(RepoConfig, solo::SoloPointer), String> {
    let repo = current_repo(engine)?;
    let ptr = solo::get(&engine.base(), &repo.id).ok_or_else(|| {
        "no current lease in this repo — `heddle lease \"<goal>\" <scope...>` first \
         (or `heddle adopt <thread-id>`)"
            .to_string()
    })?;
    Ok((repo, ptr))
}

/// An explicit `--lease <id>` / `--thread <id>` flag value, when given.
/// Several terminals (or agents) sharing one data dir use these to address
/// their own thread instead of the shared solo pointer.
fn flag_value(rest: &[String], flag: &str) -> Result<Option<String>, String> {
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return match it.next() {
                Some(v) => Ok(Some(v.clone())),
                None => Err(format!("{flag} needs a value")),
            };
        }
    }
    Ok(None)
}

/// Resolve which lease+thread a verb addresses: explicit flags win, the
/// solo pointer covers the everyday single-seat case.
fn target(
    engine: &Heddle,
    rest: &[String],
) -> Result<(RepoConfig, String /*lease*/, String /*thread*/), String> {
    let lease_flag = flag_value(rest, "--lease")?;
    let thread_flag = flag_value(rest, "--thread")?;
    let repo = current_repo(engine)?;
    if lease_flag.is_some() || thread_flag.is_some() {
        let snap = engine.snapshot();
        let rs = snap
            .repo_states
            .get(&repo.id)
            .cloned()
            .unwrap_or_default();
        let thread = rs
            .threads
            .iter()
            .find(|t| {
                Some(&t.id) == thread_flag.as_ref()
                    || (thread_flag.is_none() && t.lease_id == lease_flag)
            })
            .cloned()
            .ok_or_else(|| "no thread matches the given --lease/--thread id".to_string())?;
        let lease = thread
            .lease_id
            .clone()
            .ok_or_else(|| format!("thread {} has no lease", thread.id))?;
        return Ok((repo, lease, thread.id));
    }
    let (repo, ptr) = current_pointer(engine)?;
    Ok((repo, ptr.lease_id, ptr.thread_id))
}

fn cmd_init(rest: &[String]) -> Result<(), String> {
    let mut verify = None;
    let mut git_bridge = false;
    let mut bridge_mode = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--verify" => {
                verify = Some(
                    it.next()
                        .ok_or_else(|| "--verify needs a command".to_string())?
                        .clone(),
                )
            }
            "--git-bridge" => git_bridge = true,
            "--bridge-mode" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--bridge-mode needs squash | stitches | both".to_string())?;
                bridge_mode = Some(v.parse::<heddle::BridgeMode>()?);
            }
            other => return Err(format!("unknown init flag '{other}'")),
        }
    }
    let engine = store();
    let mut repo = engine.register_repo(".", verify, git_bridge)?;
    if let Some(mode) = bridge_mode {
        repo = engine.set_bridge_mode(&repo.id, mode)?;
    }
    println!("registered {} as {}", repo.path, repo.id);
    println!("  verify:     {}", repo.verify_cmd);
    println!("  git bridge: {}", bridge_line(&repo));
    println!("  data:       {}", engine.base().display());
    if let Some(note) = time_the_verify(&repo) {
        println!("{note}");
    }
    Ok(())
}

/// How long `init` will wait on the verify before giving up on timing it.
/// The gate itself allows far longer ([`heddle::weave::VERIFY_TIMEOUT_SECS`]);
/// this is only about telling you what you signed up for, so it stops early.
const TIMING_LIMIT_SECS: u64 = 20;

/// Past this, a verify is slow enough that it will be felt on every propose.
const SLOW_VERIFY_SECS: u64 = 5;

/// Time the freshly-registered verify command ONCE and, when it is slow,
/// say so. A warning, never a refusal: `init` has already succeeded by the
/// time this runs, and neither a red verify nor a timeout changes that.
///
/// Skipped when stdout is not a terminal (scripts and CI get no value from a
/// note nobody reads, and should not pay 20 seconds for it) or when
/// `HEDDLE_SKIP_VERIFY_TIMING` is set.
fn time_the_verify(repo: &RepoConfig) -> Option<String> {
    use std::io::IsTerminal;
    if std::env::var_os("HEDDLE_SKIP_VERIFY_TIMING").is_some() || !std::io::stdout().is_terminal() {
        return None;
    }
    println!("  timing that verify once (up to {TIMING_LIMIT_SECS}s) — ^C to skip…");
    let started = std::time::Instant::now();
    let _ = heddle::weave::run_verify(
        &repo.verify_cmd,
        std::path::Path::new(&repo.path),
        TIMING_LIMIT_SECS,
    );
    let secs = started.elapsed().as_secs();
    slow_verify_note(secs, secs >= TIMING_LIMIT_SECS)
}

/// The one honest line, or `None` when the verify was quick enough to say
/// nothing. Split out from the running so it can be tested without waiting
/// on a real command.
fn slow_verify_note(secs: u64, hit_limit: bool) -> Option<String> {
    let took = if hit_limit {
        format!("that verify was still going after {secs}s")
    } else if secs < SLOW_VERIFY_SECS {
        return None;
    } else {
        format!("that verify took {secs}s")
    };
    Some(format!(
        "note: {took} — it runs on every propose, and every agent waits for it.\n\
         Consider pointing --verify at a fast subset (e.g. `pytest -q -m \"not slow\"`, \
         `make test-fast`) and leaving the full suite to CI."
    ))
}

/// One honest line about what the bridge will do for this repo.
fn bridge_line(repo: &RepoConfig) -> String {
    if !repo.git_bridge {
        return format!(
            "off (bridge mode {} takes effect when the bridge is on)",
            repo.bridge_mode.as_str()
        );
    }
    match repo.bridge_mode {
        heddle::BridgeMode::Squash => {
            "on, squash — one local commit per landed weave (never a push)".into()
        }
        heddle::BridgeMode::Stitches => {
            "on, stitches — checkpoint commits on a heddle/<thread> branch, \
             merged with the weave message (never a push)"
                .into()
        }
        heddle::BridgeMode::Both => {
            "on, both — squash commit + the heddle/<thread> branch kept \
             unmerged (never a push)"
                .into()
        }
    }
}

fn cmd_config(rest: &[String]) -> Result<(), String> {
    let mut bridge_mode = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--bridge-mode" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--bridge-mode needs squash | stitches | both".to_string())?;
                bridge_mode = Some(v.parse::<heddle::BridgeMode>()?);
            }
            other => return Err(format!("unknown config flag '{other}'")),
        }
    }
    let engine = store();
    let mut repo = current_repo(engine)?;
    if let Some(mode) = bridge_mode {
        repo = engine.set_bridge_mode(&repo.id, mode)?;
        println!("bridge mode set to {}", repo.bridge_mode.as_str());
    }
    println!("repo {} ({})", repo.id, repo.path);
    println!("  verify:      {}", repo.verify_cmd);
    println!("  git bridge:  {}", bridge_line(&repo));
    println!(
        "  sync:        {}{}",
        repo.sync_remote.as_deref().unwrap_or("(no remote configured)"),
        if repo.auto_sync { ", auto" } else { "" }
    );
    Ok(())
}

fn cmd_export(rest: &[String]) -> Result<(), String> {
    let engine = store();
    let repo = current_repo(engine)?;
    // Explicit --thread wins; otherwise the solo pointer's thread. Unlike
    // stitch/propose this never needs a live lease — woven and orphaned
    // threads export too.
    let thread_id = match flag_value(rest, "--thread")? {
        Some(id) => id,
        None => current_pointer(engine).map(|(_, ptr)| ptr.thread_id).map_err(|_| {
            "no current thread — `heddle export --thread <id>` (see `heddle status`)".to_string()
        })?,
    };
    let _ = repo;
    let out = engine.export_thread(&thread_id)?;
    println!(
        "exported {} stitch commit(s) to branch {}",
        out.commits, out.branch
    );
    println!("  goal: {}", out.thread.goal);
    println!("  review with: git log -p {}", out.branch);
    println!("  nothing landed — the working tree and current branch are untouched");
    Ok(())
}

fn cmd_lease(rest: &[String]) -> Result<(), String> {
    let mut goal = None;
    let mut scope = Vec::new();
    let mut criteria = Vec::new();
    let mut ttl_ms = None;
    let mut mode = IsolationMode::Auto;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--criteria" => criteria.push(
                it.next()
                    .ok_or_else(|| "--criteria needs a value".to_string())?
                    .clone(),
            ),
            "--ttl-ms" => {
                let v = it.next().ok_or_else(|| "--ttl-ms needs a number".to_string())?;
                ttl_ms = Some(v.parse::<u64>().map_err(|_| format!("bad --ttl-ms '{v}'"))?);
            }
            "--isolated" => mode = IsolationMode::Isolated,
            "--in-place" => mode = IsolationMode::InPlace,
            other if goal.is_none() => goal = Some(other.to_string()),
            other => scope.push(other.to_string()),
        }
    }
    let goal = goal.ok_or_else(|| {
        "usage: heddle lease \"<goal>\" <scope...> [--criteria TEXT]... [--ttl-ms N] \
         [--isolated|--in-place]"
            .to_string()
    })?;
    let engine = store();
    let repo = current_repo(engine)?;
    let out = engine.declare_lease_mode(&repo.id, &holder(), &goal, scope, criteria, ttl_ms, mode)?;
    solo::set(&engine.base(), &repo.id, &out.lease.id, &out.thread.id);
    println!("lease {} (thread {})", out.lease.id, out.thread.id);
    println!("  goal:   {}", out.lease.goal);
    println!("  scope:  {}", out.lease.scope.join(", "));
    if !out.lease.criteria.is_empty() {
        println!("  criteria: {}", out.lease.criteria.join("; "));
    }
    if out.thread.worktree.is_some() {
        println!();
        println!("  WORK IN: {}", out.working_dir);
        println!("           (your isolated worktree — edit files THERE; the repo tree");
        println!("            changes only when your weave lands, and never over anyone)");
        println!();
    } else {
        println!("  working in place: {}", out.working_dir);
        if !out.thread.note.is_empty() {
            println!("  note: {}", out.thread.note);
        }
    }
    println!(
        "  expires in {}s without a heartbeat (stitching heartbeats for you)",
        out.lease.ttl_ms / 1000
    );
    for t in &out.toe_steps {
        println!("  TOE-STEP: your '{}' overlaps '{}' ({} vs {})", t.goal_a, t.goal_b, t.pattern_a, t.pattern_b);
        for s in &t.suggested_split {
            println!("    · {s}");
        }
        println!("    (a lease warns, it never blocks — coordinate or continue)");
    }
    autosync(engine, &repo);
    Ok(())
}

/// Fire an auto-sync pass when the repo opted in; print, never fail the
/// local operation.
fn autosync(engine: &Heddle, repo: &RepoConfig) {
    // Re-read the repo config: sync flags may have just changed.
    let repo = engine
        .snapshot()
        .repos
        .into_iter()
        .find(|r| r.id == repo.id)
        .unwrap_or_else(|| repo.clone());
    match sync::maybe_autosync(engine, &repo) {
        None => {}
        Some(Ok(out)) => print_sync(&out, true),
        Some(Err(e)) => println!("  auto-sync failed (local work unaffected): {e}"),
    }
}

fn print_sync(out: &sync::SyncOutcome, brief: bool) {
    if !brief {
        println!("synced with '{}' as {}", out.remote, out.machine);
    } else {
        println!("  auto-synced with '{}'", out.remote);
    }
    if out.fabric_pulled > 0 {
        println!(
            "  pulled {} weave(s) from the shared fabric into this tree",
            out.fabric_pulled
        );
    }
    if out.fabric_pushed > 0 {
        println!("  published {} local weave(s) to the shared fabric", out.fabric_pushed);
    }
    if let Some(note) = &out.cas_refused {
        println!("  FABRIC MOVED: {note}");
    }
    if !out.peers.is_empty() {
        println!("  peers: {}", out.peers.join(", "));
    }
    for t in &out.toe_steps {
        println!(
            "  CROSS-MACHINE TOE-STEP: your '{}' overlaps '{}' ({} vs {})",
            t.goal_a, t.goal_b, t.pattern_a, t.pattern_b
        );
    }
    for (tid, goal, machine) in &out.remote_orphans {
        println!("  adoptable on {machine}: {tid} — {goal} (heddle adopt {tid})");
    }
}

fn cmd_sync(rest: &[String]) -> Result<(), String> {
    let mut remote = None;
    let mut auto = None;
    let mut anyway = false;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--remote" => {
                remote = Some(
                    it.next()
                        .ok_or_else(|| "--remote needs a git remote name".to_string())?
                        .clone(),
                )
            }
            "--auto" => {
                let v = it.next().map(String::as_str).unwrap_or("on");
                auto = Some(match v {
                    "on" | "true" => true,
                    "off" | "false" => false,
                    other => return Err(format!("--auto takes on|off, got '{other}'")),
                });
            }
            "--anyway" => anyway = true,
            other => return Err(format!("unknown sync flag '{other}'")),
        }
    }
    let engine = store();
    let repo = current_repo(engine)?;
    if remote.is_some() || auto.is_some() {
        println!(
            "note: sync shares this repo's heddle metadata AND scoped file content with the \
             remote — the same exposure as pushing a branch there."
        );
    }
    let out = sync::sync_opts(engine, &repo.id, remote.as_deref(), auto, anyway)?;
    print_sync(&out, false);
    if out.fabric_pulled == 0
        && out.fabric_pushed == 0
        && out.cas_refused.is_none()
        && out.peers.is_empty()
    {
        println!("  nothing new either way");
    }
    Ok(())
}

fn cmd_rebase(rest: &[String]) -> Result<(), String> {
    let engine = store();
    let (_repo, _lease_id, thread_id) = target(engine, rest)?;
    let out = engine.rebase_thread(&thread_id)?;
    if out.fast_forwarded.is_empty() && out.conflicts.is_empty() {
        println!("already in step with the fabric — nothing to do");
    }
    for f in &out.fast_forwarded {
        println!("  fast-forwarded from fabric: {f}");
    }
    if !out.conflicts.is_empty() {
        println!("  CONFLICTS — both the fabric and this thread changed:");
        for f in &out.conflicts {
            println!("    {f} (your version kept in the worktree; the fabric's is in the repo tree)");
        }
        println!("  review those files, then `heddle stitch` and `heddle propose`.");
    } else {
        println!("rebased clean — `heddle propose` when ready");
    }
    Ok(())
}

fn cmd_clean() -> Result<(), String> {
    let engine = store();
    let repo = current_repo(engine)?;
    let report = engine.clean_worktrees(&repo.id)?;
    for (tid, path) in &report.removed {
        println!("removed {path} ({tid})");
    }
    for (tid, reason) in &report.skipped {
        println!("kept {tid}: {reason}");
    }
    if report.removed.is_empty() && report.skipped.is_empty() {
        println!("no worktrees to clean");
    }
    Ok(())
}

fn cmd_stitch(rest: &[String]) -> Result<(), String> {
    let engine = store();
    let (repo, lease_id, _thread_id) = target(engine, rest)?;
    // A human running `stitch` is alive: heartbeat first so active work
    // never orphans mid-session. (Refused for orphans — adopt instead.)
    engine.heartbeat(&lease_id)?;
    let out = engine.stitch(&lease_id)?;
    if out.unchanged {
        println!(
            "unchanged — head stitch {} already matches the scope",
            out.stitch.id
        );
    } else {
        let dels = out.stitch.files.values().filter(|h| *h == heddle::TOMBSTONE).count();
        if dels > 0 {
            println!(
                "stitch {} ({} files, {} deletion(s))",
                out.stitch.id,
                out.stitch.files.len() - dels,
                dels
            );
        } else {
            println!("stitch {} ({} files)", out.stitch.id, out.stitch.files.len());
        }
    }
    for s in &out.skipped {
        println!("  skipped: {s}");
    }
    println!(
        "  lease expires in {}s",
        out.lease.expires_in_ms(wall_ms()) / 1000
    );
    autosync(engine, &repo);
    Ok(())
}

/// Wall-clock milliseconds (same clock as the engine's timestamps).
fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn cmd_propose(rest: &[String]) -> Result<(), String> {
    let engine = store();
    let (repo, _lease_id, thread_id) = target(engine, rest)?;
    println!("running verify in a scratch copy (never the real tree)…");
    match engine.propose_with_consent(&thread_id, &TerminalConsent)? {
        WeaveDisposition::Red { weave, .. } => Err(format!(
            "verify RED ({}) — nothing can land; the thread stays active.\n--- log tail ---\n{}",
            weave.verify.cmd, weave.verify.log_tail
        )),
        WeaveDisposition::Refused { reason, thread, .. } => {
            println!("verified green; NOTHING was applied — {reason}");
            println!("thread note: {}", thread.note);
            Ok(())
        }
        WeaveDisposition::Landed { land, git } => {
            let is_current = solo::get(&engine.base(), &repo.id)
                .map(|p| p.thread_id == thread_id)
                .unwrap_or(false);
            if is_current {
                solo::clear(&engine.base(), &repo.id);
            }
            println!(
                "woven: {} files applied, fabric tip is now {}",
                land.files_applied, land.weave.id
            );
            match git {
                Some(Ok(note)) => println!("git bridge: {note}"),
                Some(Err(e)) => println!("git bridge FAILED (weave stays landed): {e}"),
                None => {}
            }
            autosync(engine, &repo);
            Ok(())
        }
    }
}

fn cmd_withdraw() -> Result<(), String> {
    let engine = store();
    let (_repo, ptr) = current_pointer(engine)?;
    let (thread, _aid) = engine.withdraw(&ptr.thread_id, "")?;
    println!("thread {} is active again — {}", thread.id, thread.note);
    Ok(())
}

fn cmd_adopt(rest: &[String]) -> Result<(), String> {
    let thread_id = rest
        .first()
        .ok_or_else(|| "usage: heddle adopt <thread-id> (see `heddle status` for orphans)".to_string())?;
    let engine = store();
    // Local first; unknown thread + a sync remote → try the claims flow.
    let (thread, lease) = match engine.adopt(thread_id, &holder()) {
        Ok(pair) => pair,
        Err(e) if e.contains("no thread with id") => {
            let repo = current_repo(engine)?;
            if repo.sync_remote.is_some() {
                println!("not a local thread — claiming it from a synced peer…");
                sync::adopt_remote(engine, &repo.id, thread_id, &holder())?
            } else {
                return Err(e);
            }
        }
        Err(e) => return Err(e),
    };
    solo::set(&engine.base(), &thread.repo_id, &lease.id, &thread.id);
    println!("adopted {} — goal: {}", thread.id, lease.goal);
    if !lease.criteria.is_empty() {
        println!("  criteria: {}", lease.criteria.join("; "));
    }
    println!("  scope: {}", lease.scope.join(", "));
    if let Some(wt) = &thread.worktree {
        println!();
        println!("  WORK IN: {wt}");
        println!("           (the thread's worktree, last stitch materialized — continue there)");
        println!();
    }
    println!("  continue from the last stitch; `heddle stitch` and `heddle propose` as usual");
    Ok(())
}

fn cmd_status() -> Result<(), String> {
    let engine = store();
    let repo = current_repo(engine)?;
    let snap = engine.snapshot();
    let rs = snap
        .repo_states
        .get(&repo.id)
        .cloned()
        .unwrap_or_default();
    let ptr = solo::get(&engine.base(), &repo.id);
    println!("repo {} ({})", repo.id, repo.path);
    println!(
        "  verify: {}   git bridge: {}",
        repo.verify_cmd,
        if repo.git_bridge {
            format!("on ({})", repo.bridge_mode.as_str())
        } else {
            "off".to_string()
        }
    );
    println!(
        "  fabric: {} ({} weaves landed — every one verified green)",
        rs.fabric.tip.as_deref().unwrap_or("empty"),
        rs.fabric.history.len()
    );
    let now = wall_ms();
    println!("  threads:");
    for t in &rs.threads {
        let current = ptr.as_ref().is_some_and(|p| p.thread_id == t.id);
        let lease_info = t
            .lease_id
            .as_ref()
            .and_then(|lid| rs.leases.iter().find(|l| l.id == *lid))
            .map(|l| {
                if t.status.is_live() && !l.expired(now) {
                    format!(" lease expires {}s", l.expires_in_ms(now) / 1000)
                } else {
                    String::new()
                }
            })
            .unwrap_or_default();
        println!(
            "    {}{} [{:?}] {}{}{}",
            if current { "* " } else { "  " },
            t.id,
            t.status,
            t.goal,
            if t.note.is_empty() {
                String::new()
            } else {
                format!(" — {}", t.note)
            },
            lease_info,
        );
        if let Some(wt) = &t.worktree {
            if t.status.is_live() {
                println!("        worktree: {wt}");
            }
        }
    }
    if rs.threads.is_empty() {
        println!("    (none — `heddle lease \"<goal>\" <scope...>` to start)");
    }
    let orphans: Vec<_> = rs
        .threads
        .iter()
        .filter(|t| t.status == ThreadStatus::Orphaned)
        .collect();
    if !orphans.is_empty() {
        println!("  orphans (adoptable — `heddle adopt <thread-id>`):");
        for t in orphans {
            println!("    {} — {}", t.id, t.goal);
        }
    }
    for t in rs.toe_steps.iter().rev().take(3) {
        println!(
            "  toe-step: '{}' vs '{}' ({} / {})",
            t.goal_a, t.goal_b, t.pattern_a, t.pattern_b
        );
    }
    if !rs.peers.is_empty() {
        println!("  peers (as of last `heddle sync`):");
        let now = wall_ms();
        for p in &rs.peers {
            let age_s = now.saturating_sub(p.fetched_ms) / 1000;
            println!("    {} (fetched {age_s}s ago):", p.machine);
            for t in &p.threads {
                let lease_dead = t
                    .lease_id
                    .as_ref()
                    .and_then(|lid| p.leases.iter().find(|l| l.id == *lid))
                    .map(|l| l.expired(now))
                    .unwrap_or(true);
                let adoptable =
                    t.status == ThreadStatus::Orphaned || (t.status.is_live() && lease_dead);
                println!(
                    "      {} [{:?}] {}{}",
                    t.id,
                    t.status,
                    t.goal,
                    if adoptable { "  ← adoptable (heddle adopt)" } else { "" }
                );
            }
        }
    }
    Ok(())
}

fn cmd_log() -> Result<(), String> {
    let engine = store();
    let repo = current_repo(engine)?;
    let snap = engine.snapshot();
    let rs = snap
        .repo_states
        .get(&repo.id)
        .cloned()
        .unwrap_or_default();
    println!("fabric history for {} (oldest first, all green):", repo.id);
    if rs.fabric.history.is_empty() {
        println!("  (no weaves landed yet)");
    }
    for wid in &rs.fabric.history {
        if let Some(w) = rs.weaves.iter().find(|w| w.id == *wid) {
            let goal = rs
                .threads
                .iter()
                .find(|t| t.id == w.thread_id)
                .map(|t| t.goal.clone())
                .unwrap_or_else(|| w.thread_id.clone());
            println!("  {} — {} (verify: {})", w.id, goal, w.verify.cmd);
        } else {
            println!("  {wid} (pruned from the weave ring)");
        }
    }
    println!("recent events:");
    for e in heddle::store::read_events(&engine.base(), &repo.id, 15) {
        println!(
            "  {} {}",
            e.get("kind").and_then(|k| k.as_str()).unwrap_or("?"),
            e.get("thread")
                .or_else(|| e.get("lease"))
                .or_else(|| e.get("weave"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_slow_verify_gets_a_note_and_it_names_the_seconds() {
        // Quick enough that nobody will feel it — say nothing.
        assert_eq!(slow_verify_note(0, false), None);
        assert_eq!(slow_verify_note(SLOW_VERIFY_SECS - 1, false), None);

        let note = slow_verify_note(49, false).expect("49s is worth a word");
        assert!(note.contains("took 49s"), "{note}");
        assert!(note.contains("every propose"), "{note}");
        assert!(note.contains("test-fast"), "{note}");

        // Timed out: honest about not having waited for the end.
        let note = slow_verify_note(TIMING_LIMIT_SECS, true).expect("a timeout is always slow");
        assert!(
            note.contains(&format!("still going after {TIMING_LIMIT_SECS}s")),
            "{note}"
        );
    }

    #[test]
    fn timing_is_skipped_when_asked_to_be() {
        // Set for this process; the check is env-var presence, and cargo's
        // test harness gives us no tty either — both paths return None, so
        // no test ever waits on a real verify command.
        std::env::set_var("HEDDLE_SKIP_VERIFY_TIMING", "1");
        let repo = RepoConfig {
            id: "repo-t".into(),
            path: ".".into(),
            verify_cmd: "sleep 60".into(),
            git_bridge: false,
            bridge_mode: Default::default(),
            registered_ms: 0,
            sync_remote: None,
            auto_sync: false,
        };
        let started = std::time::Instant::now();
        assert_eq!(time_the_verify(&repo), None);
        assert!(started.elapsed().as_secs() < 5, "it must not have run anything");
    }
}
