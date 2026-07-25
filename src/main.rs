// Loom — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! The standalone `loom` binary — solo/CLI front-end over the engine in
//! `lib.rs`, plus `loom mcp`, a stdio MCP server for agent sessions.
//!
//! Verbs: `init · config · lease · stitch · propose · export · withdraw ·
//! adopt · status · log · mcp`. State lives under `~/.loom` (override with
//! `LOOM_DATA`).
//! The current lease per repo is remembered in a solo-mode pointer
//! (`solo.json`) so `stitch`/`propose`/`withdraw` need no ids.
//!
//! The honesty rules the engine enforces hold here too: `propose` runs the
//! verify in a scratch copy and, on green, asks YOU at the terminal before
//! anything touches the working tree. Non-interactive stdin means the apply
//! is refused, with the reason recorded on the thread.

use std::process::ExitCode;

use loom::consent::{TerminalConsent, WeaveDisposition};
use loom::{solo, store, sync, IsolationMode, Loom, RepoConfig, ThreadStatus};

mod mcp;

const USAGE: &str = "\
loom — version control for many hands moving at once (docs/DESIGN.md)

usage:
  loom init [--verify CMD] [--git-bridge]     register the current directory
       [--bridge-mode squash|stitches|both]   (bridge granularity; default
                                              squash — one commit per weave)
  loom config [--bridge-mode MODE]            show this repo's config, or set
                                              the git-bridge granularity
  loom lease \"<goal>\" <scope...>              declare an intent lease; on a git
       [--criteria TEXT]... [--ttl-ms N]      repo the thread gets its OWN
       [--isolated | --in-place]              worktree — edit there
  loom stitch [--lease ID]                    checkpoint the leased scope
  loom propose [--thread ID]                  verify in a scratch copy; green
                                              asks you before applying
  loom export [--thread ID]                   write the thread's stitch chain to
                                              its draft branch loom/<id>-<goal>
                                              for review — nothing lands
  loom rebase [--thread ID]                   refresh the thread's worktree from
                                              the fabric (after \"fabric moved\")
  loom withdraw                               return a proposed thread to active
  loom adopt <thread-id>                      take over an orphaned thread
                                              (local, or a synced peer's)
  loom clean                                  remove worktrees of woven threads
                                              (refuses uncaptured divergence)
  loom sync [--remote NAME] [--auto on|off]   sync leases/threads/fabric with a
                                              git remote (shares scoped content
                                              there — same exposure as a push)
  loom status                                 threads, leases, orphans, peers
  loom log                                    fabric history + recent events
  loom mcp                                    stdio MCP server (loom_status,
                                              loom_lease, loom_stitch,
                                              loom_propose, loom_rebase,
                                              loom_adopt)

state: ~/.loom (override with LOOM_DATA)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verb = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[1..];
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
            eprintln!("loom: {e}");
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
fn current_repo(engine: &Loom) -> Result<RepoConfig, String> {
    engine.repo_containing(".").ok_or_else(|| {
        "no registered repo contains this directory — run `loom init` here first".to_string()
    })
}

/// The solo pointer for the current repo, validated against the engine.
fn current_pointer(engine: &Loom) -> Result<(RepoConfig, solo::SoloPointer), String> {
    let repo = current_repo(engine)?;
    let ptr = solo::get(&engine.base(), &repo.id).ok_or_else(|| {
        "no current lease in this repo — `loom lease \"<goal>\" <scope...>` first \
         (or `loom adopt <thread-id>`)"
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
    engine: &Loom,
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
                bridge_mode = Some(v.parse::<loom::BridgeMode>()?);
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
    Ok(())
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
        loom::BridgeMode::Squash => {
            "on, squash — one local commit per landed weave (never a push)".into()
        }
        loom::BridgeMode::Stitches => {
            "on, stitches — checkpoint commits on a loom/<thread> branch, \
             merged with the weave message (never a push)"
                .into()
        }
        loom::BridgeMode::Both => {
            "on, both — squash commit + the loom/<thread> branch kept \
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
                bridge_mode = Some(v.parse::<loom::BridgeMode>()?);
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
            "no current thread — `loom export --thread <id>` (see `loom status`)".to_string()
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
        "usage: loom lease \"<goal>\" <scope...> [--criteria TEXT]... [--ttl-ms N] \
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
fn autosync(engine: &Loom, repo: &RepoConfig) {
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
        println!("  adoptable on {machine}: {tid} — {goal} (loom adopt {tid})");
    }
}

fn cmd_sync(rest: &[String]) -> Result<(), String> {
    let mut remote = None;
    let mut auto = None;
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
            other => return Err(format!("unknown sync flag '{other}'")),
        }
    }
    let engine = store();
    let repo = current_repo(engine)?;
    if remote.is_some() || auto.is_some() {
        println!(
            "note: sync shares this repo's loom metadata AND scoped file content with the \
             remote — the same exposure as pushing a branch there."
        );
    }
    let out = sync::sync(engine, &repo.id, remote.as_deref(), auto)?;
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
        println!("  review those files, then `loom stitch` and `loom propose`.");
    } else {
        println!("rebased clean — `loom propose` when ready");
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
        let dels = out.stitch.files.values().filter(|h| *h == loom::TOMBSTONE).count();
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
        .ok_or_else(|| "usage: loom adopt <thread-id> (see `loom status` for orphans)".to_string())?;
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
    println!("  continue from the last stitch; `loom stitch` and `loom propose` as usual");
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
        println!("    (none — `loom lease \"<goal>\" <scope...>` to start)");
    }
    let orphans: Vec<_> = rs
        .threads
        .iter()
        .filter(|t| t.status == ThreadStatus::Orphaned)
        .collect();
    if !orphans.is_empty() {
        println!("  orphans (adoptable — `loom adopt <thread-id>`):");
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
        println!("  peers (as of last `loom sync`):");
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
                    if adoptable { "  ← adoptable (loom adopt)" } else { "" }
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
    for e in loom::store::read_events(&engine.base(), &repo.id, 15) {
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
