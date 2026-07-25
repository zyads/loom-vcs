// Loom — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! The standalone `loom` binary — solo/CLI front-end over the engine in
//! `lib.rs`, plus `loom mcp`, a stdio MCP server for agent sessions.
//!
//! Verbs: `init · lease · stitch · propose · withdraw · adopt · status ·
//! log · mcp`. State lives under `~/.loom` (override with `LOOM_DATA`).
//! The current lease per repo is remembered in a solo-mode pointer
//! (`solo.json`) so `stitch`/`propose`/`withdraw` need no ids.
//!
//! The honesty rules the engine enforces hold here too: `propose` runs the
//! verify in a scratch copy and, on green, asks YOU at the terminal before
//! anything touches the working tree. Non-interactive stdin means the apply
//! is refused, with the reason recorded on the thread.

use std::process::ExitCode;

use loom::consent::{TerminalConsent, WeaveDisposition};
use loom::{solo, store, Loom, RepoConfig, ThreadStatus};

mod mcp;

const USAGE: &str = "\
loom — version control for many hands moving at once (docs/DESIGN.md)

usage:
  loom init [--verify CMD] [--git-bridge]     register the current directory
  loom lease \"<goal>\" <scope...>              declare an intent lease
       [--criteria TEXT]... [--ttl-ms N]      (scopes are repo-relative globs)
  loom stitch                                 checkpoint the leased scope
  loom propose                                verify in a scratch copy; green
                                              asks you before applying
  loom withdraw                               return a proposed thread to active
  loom adopt <thread-id>                      take over an orphaned thread
  loom status                                 threads, leases, orphans, fabric
  loom log                                    fabric history + recent events
  loom mcp                                    stdio MCP server (loom_status,
                                              loom_lease, loom_stitch,
                                              loom_propose, loom_adopt)

state: ~/.loom (override with LOOM_DATA)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verb = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[1..];
    let out = match verb {
        "init" => cmd_init(rest),
        "lease" => cmd_lease(rest),
        "stitch" => cmd_stitch(),
        "propose" => cmd_propose(),
        "withdraw" => cmd_withdraw(),
        "adopt" => cmd_adopt(rest),
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

fn cmd_init(rest: &[String]) -> Result<(), String> {
    let mut verify = None;
    let mut git_bridge = false;
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
            other => return Err(format!("unknown init flag '{other}'")),
        }
    }
    let engine = store();
    let repo = engine.register_repo(".", verify, git_bridge)?;
    println!("registered {} as {}", repo.path, repo.id);
    println!("  verify:     {}", repo.verify_cmd);
    println!(
        "  git bridge: {}",
        if repo.git_bridge {
            "on — one local commit per landed weave (never a push)"
        } else {
            "off"
        }
    );
    println!("  data:       {}", engine.base().display());
    Ok(())
}

fn cmd_lease(rest: &[String]) -> Result<(), String> {
    let mut goal = None;
    let mut scope = Vec::new();
    let mut criteria = Vec::new();
    let mut ttl_ms = None;
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
            other if goal.is_none() => goal = Some(other.to_string()),
            other => scope.push(other.to_string()),
        }
    }
    let goal = goal.ok_or_else(|| {
        "usage: loom lease \"<goal>\" <scope...> [--criteria TEXT]... [--ttl-ms N]".to_string()
    })?;
    let engine = store();
    let repo = current_repo(engine)?;
    let out = engine.declare_lease(&repo.id, &holder(), &goal, scope, criteria, ttl_ms)?;
    solo::set(&engine.base(), &repo.id, &out.lease.id, &out.thread.id);
    println!("lease {} (thread {})", out.lease.id, out.thread.id);
    println!("  goal:   {}", out.lease.goal);
    println!("  scope:  {}", out.lease.scope.join(", "));
    if !out.lease.criteria.is_empty() {
        println!("  criteria: {}", out.lease.criteria.join("; "));
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
    Ok(())
}

fn cmd_stitch() -> Result<(), String> {
    let engine = store();
    let (_repo, ptr) = current_pointer(engine)?;
    // A human running `stitch` is alive: heartbeat first so active work
    // never orphans mid-session. (Refused for orphans — adopt instead.)
    engine.heartbeat(&ptr.lease_id)?;
    let out = engine.stitch(&ptr.lease_id)?;
    if out.unchanged {
        println!(
            "unchanged — head stitch {} already matches the scope",
            out.stitch.id
        );
    } else {
        println!("stitch {} ({} files)", out.stitch.id, out.stitch.files.len());
    }
    for s in &out.skipped {
        println!("  skipped: {s}");
    }
    println!(
        "  lease expires in {}s",
        out.lease.expires_in_ms(wall_ms()) / 1000
    );
    Ok(())
}

/// Wall-clock milliseconds (same clock as the engine's timestamps).
fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn cmd_propose() -> Result<(), String> {
    let engine = store();
    let (repo, ptr) = current_pointer(engine)?;
    println!("running verify in a scratch copy (never the real tree)…");
    match engine.propose_with_consent(&ptr.thread_id, &TerminalConsent)? {
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
            solo::clear(&engine.base(), &repo.id);
            println!(
                "woven: {} files applied, fabric tip is now {}",
                land.files_applied, land.weave.id
            );
            match git {
                Some(Ok(note)) => println!("git bridge: {note}"),
                Some(Err(e)) => println!("git bridge FAILED (weave stays landed): {e}"),
                None => {}
            }
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
    let (thread, lease) = engine.adopt(thread_id, &holder())?;
    solo::set(&engine.base(), &thread.repo_id, &lease.id, &thread.id);
    println!("adopted {} — goal: {}", thread.id, lease.goal);
    if !lease.criteria.is_empty() {
        println!("  criteria: {}", lease.criteria.join("; "));
    }
    println!("  scope: {}", lease.scope.join(", "));
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
    println!("  verify: {}   git bridge: {}", repo.verify_cmd, repo.git_bridge);
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
