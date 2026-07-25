// Loom — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! `loom mcp` — a minimal MCP (Model Context Protocol) stdio server, so any
//! agent session (Claude Code or otherwise) can lease/stitch/propose without
//! any other infrastructure.
//!
//! Hand-rolled on purpose: MCP's stdio transport is newline-delimited
//! JSON-RPC 2.0, and this server needs exactly three methods — `initialize`,
//! `tools/list`, `tools/call` — so ~200 lines beat an SDK dependency.
//!
//! **Honesty at the protocol edge:** this process's stdin is the protocol
//! channel, so there is no terminal to ask a human at. `loom_propose`
//! therefore uses [`AutoDeny`]: the verify runs for real (scratch copy,
//! never the working tree) and a green result is recorded, but the apply is
//! always refused with instructions to run `loom propose` at a terminal.
//! The tool descriptions say so, in those words — an agent reading them
//! learns the truth, not a euphemism.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use loom::consent::{AutoDeny, WeaveDisposition};
use loom::{solo, Loom};

/// Serve MCP over stdio until stdin closes.
pub fn serve(engine: &Loom) {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue; // not JSON — nothing sane to answer
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        // Notifications (no id) get no response, per JSON-RPC.
        let Some(id) = id else { continue };
        let reply = match method {
            "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "loom", "version": env!("CARGO_PKG_VERSION")},
            }}),
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools()}}),
            "tools/call" => {
                let name = msg
                    .pointer("/params/name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                let args = msg
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match call(engine, name, &args) {
                    Ok(text) => json!({"jsonrpc": "2.0", "id": id, "result": {
                        "content": [{"type": "text", "text": text}],
                        "isError": false,
                    }}),
                    Err(e) => json!({"jsonrpc": "2.0", "id": id, "result": {
                        "content": [{"type": "text", "text": e}],
                        "isError": true,
                    }}),
                }
            }
            other => json!({"jsonrpc": "2.0", "id": id, "error": {
                "code": -32601, "message": format!("method not found: {other}"),
            }}),
        };
        let Ok(body) = serde_json::to_string(&reply) else { continue };
        if writeln!(stdout, "{body}").is_err() || stdout.flush().is_err() {
            break;
        }
    }
}

fn tools() -> Value {
    let repo_prop = json!({"type": "string", "description":
        "Repo directory (default: the server's working directory). Must already be `loom init`-ed."});
    json!([
        {
            "name": "loom_status",
            "description": "Threads, live leases (with expiry), orphan queue, toe-step warnings and \
                fabric history for the repo. Read-only.",
            "inputSchema": {"type": "object", "properties": {"repo": repo_prop}},
        },
        {
            "name": "loom_lease",
            "description": "Declare an intent lease before editing: a one-sentence goal plus \
                repo-relative path globs (e.g. src/parse/**). Overlap with live leases SUCCEEDS \
                and returns toe_step warnings — a lease is knowledge, not a lock. On a git repo \
                the thread gets its OWN isolated worktree: the response's working_dir is where \
                you MUST cd and do ALL file edits (the repo tree itself changes only when your \
                weave lands). Becomes the current lease for loom_stitch/loom_propose.",
            "inputSchema": {"type": "object", "properties": {
                "goal": {"type": "string", "description": "One sentence: what you are about to do."},
                "scope": {"type": "array", "items": {"type": "string"},
                          "description": "Repo-relative path globs you intend to touch."},
                "criteria": {"type": "array", "items": {"type": "string"},
                             "description": "Acceptance criteria (optional)."},
                "ttl_ms": {"type": "integer", "description":
                    "Lease TTL in ms (default 30min; clamped 10s–24h). Stitching heartbeats it."},
                "in_place": {"type": "boolean", "description":
                    "true = edit the repo tree directly instead of an isolated worktree \
                     (v0.1 behavior; collisions then only warn)."},
                "repo": repo_prop,
            }, "required": ["goal", "scope"]},
        },
        {
            "name": "loom_stitch",
            "description": "Checkpoint the leased scope from the thread's working_dir (its \
                worktree when isolated): the server reads the files itself and snapshots them \
                content-addressed, deletions included (you never upload content, and nothing is \
                written to the repo). Cheap — call every few edits. Also heartbeats the lease.",
            "inputSchema": {"type": "object", "properties": {
                "lease_id": {"type": "string", "description": "Defaults to the current lease."},
                "repo": repo_prop,
            }},
        },
        {
            "name": "loom_rebase",
            "description": "After a land was refused with 'fabric moved under you': refresh the \
                thread's worktree from the fabric. Fabric-only changes fast-forward in; files \
                changed on BOTH sides keep YOUR version and come back as conflicts to review \
                (the fabric's copy is in the repo tree). Then loom_stitch and loom_propose again.",
            "inputSchema": {"type": "object", "properties": {
                "thread_id": {"type": "string", "description": "Defaults to the current thread."},
                "repo": repo_prop,
            }},
        },
        {
            "name": "loom_propose",
            "description": "Run the weave gate: the repo's verify command runs in a scratch copy, \
                never the working tree. Red is reported with the log tail. Green is recorded, but \
                landing it needs a human yes at a terminal (`loom propose`) — this server has no \
                terminal, so the apply is always refused here and the thread returns to active, \
                re-proposable. Honest summary: proposing verifies; it never lands.",
            "inputSchema": {"type": "object", "properties": {
                "thread_id": {"type": "string", "description": "Defaults to the current thread."},
                "repo": repo_prop,
            }},
        },
        {
            "name": "loom_adopt",
            "description": "Take over an orphaned thread (its holder's lease expired — crashed \
                session, dead machine). You inherit the SAME lease: goal, criteria and scope \
                preserved, fresh TTL. Continue from its last stitch.",
            "inputSchema": {"type": "object", "properties": {
                "thread_id": {"type": "string"},
                "repo": repo_prop,
            }, "required": ["thread_id"]},
        },
    ])
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty())
}

fn repo_of(engine: &Loom, args: &Value) -> Result<loom::RepoConfig, String> {
    let path = arg(args, "repo").unwrap_or(".");
    engine.repo_containing(path).ok_or_else(|| {
        format!("no registered loom repo contains '{path}' — run `loom init` there first")
    })
}

fn holder() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .map(|u| format!("{u} (mcp agent)"))
        .unwrap_or_else(|| "mcp agent".to_string())
}

fn call(engine: &Loom, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "loom_status" => {
            let repo = repo_of(engine, args)?;
            let snap = engine.snapshot();
            let rs = snap.repo_states.get(&repo.id).cloned().unwrap_or_default();
            let now = loom_now();
            let leases: Vec<Value> = rs
                .leases
                .iter()
                .filter(|l| !l.expired(now))
                .map(|l| json!({"lease": l, "expires_in_ms": l.expires_in_ms(now)}))
                .collect();
            Ok(json!({
                "repo": repo,
                "fabric": rs.fabric,
                "threads": rs.threads,
                "live_leases": leases,
                "orphans": rs.threads.iter()
                    .filter(|t| t.status == loom::ThreadStatus::Orphaned)
                    .collect::<Vec<_>>(),
                "toe_steps": rs.toe_steps.iter().rev().take(5).collect::<Vec<_>>(),
                "note": "leases warn, never block; weaves land only past a human yes at a terminal",
            })
            .to_string())
        }
        "loom_lease" => {
            let repo = repo_of(engine, args)?;
            let goal = arg(args, "goal").ok_or("loom_lease needs a goal")?;
            let scope: Vec<String> = args
                .get("scope")
                .and_then(|s| s.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let criteria: Vec<String> = args
                .get("criteria")
                .and_then(|s| s.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let ttl_ms = args.get("ttl_ms").and_then(|v| v.as_u64());
            let mode = if args.get("in_place").and_then(|v| v.as_bool()).unwrap_or(false) {
                loom::IsolationMode::InPlace
            } else {
                loom::IsolationMode::Auto
            };
            let out = engine
                .declare_lease_mode(&repo.id, &holder(), goal, scope, criteria, ttl_ms, mode)?;
            solo::set(&engine.base(), &repo.id, &out.lease.id, &out.thread.id);
            let isolated = out.thread.worktree.is_some();
            Ok(json!({
                "lease": out.lease,
                "thread": out.thread,
                "toe_steps": out.toe_steps,
                "working_dir": out.working_dir,
                "working_dir_note": if isolated {
                    "your isolated worktree — cd there and make ALL edits in it; the repo \
                     tree changes only when your weave lands"
                } else {
                    "no isolation (not a git repo, or in_place requested) — you are editing \
                     the repo tree directly"
                },
                "note": if out.toe_steps.is_empty() {
                    "no toe-steps — the scope is yours to work"
                } else {
                    "toe-step warnings above: another live lease overlaps your scope; \
                     the lease still succeeded — coordinate or continue"
                },
            })
            .to_string())
        }
        "loom_rebase" => {
            let repo = repo_of(engine, args)?;
            let thread_id = match arg(args, "thread_id") {
                Some(t) => t.to_string(),
                None => solo::get(&engine.base(), &repo.id)
                    .ok_or("no current thread in this repo — loom_lease first")?
                    .thread_id,
            };
            let out = engine.rebase_thread(&thread_id)?;
            Ok(json!({
                "thread": out.thread,
                "fast_forwarded": out.fast_forwarded,
                "conflicts": out.conflicts,
                "note": if out.conflicts.is_empty() {
                    "rebased clean — loom_stitch, then loom_propose when ready".to_string()
                } else {
                    format!(
                        "CONFLICTS on {} file(s): both the fabric and this thread changed \
                         them. Your version was kept in the worktree; the fabric's is in \
                         the repo tree. Reconcile in the worktree, then loom_stitch and \
                         loom_propose.",
                        out.conflicts.len()
                    )
                },
            })
            .to_string())
        }
        "loom_stitch" => {
            let repo = repo_of(engine, args)?;
            let lease_id = match arg(args, "lease_id") {
                Some(l) => l.to_string(),
                None => solo::get(&engine.base(), &repo.id)
                    .ok_or("no current lease in this repo — loom_lease first")?
                    .lease_id,
            };
            engine.heartbeat(&lease_id)?;
            let out = engine.stitch(&lease_id)?;
            Ok(json!({
                "stitch": out.stitch.id,
                "unchanged": out.unchanged,
                "files": out.stitch.files.len(),
                "skipped": out.skipped,
                "lease_expires_in_ms": out.lease.expires_in_ms(loom_now()),
            })
            .to_string())
        }
        "loom_propose" => {
            let repo = repo_of(engine, args)?;
            let thread_id = match arg(args, "thread_id") {
                Some(t) => t.to_string(),
                None => solo::get(&engine.base(), &repo.id)
                    .ok_or("no current thread in this repo — loom_lease first")?
                    .thread_id,
            };
            match engine.propose_with_consent(&thread_id, &AutoDeny)? {
                WeaveDisposition::Red { weave, .. } => Ok(json!({
                    "verify": "red",
                    "cmd": weave.verify.cmd,
                    "log_tail": weave.verify.log_tail,
                    "landed": false,
                    "note": "verify was red — nothing can land; fix and re-propose",
                })
                .to_string()),
                WeaveDisposition::Refused { weave, reason, .. } => Ok(json!({
                    "verify": "green",
                    "cmd": weave.verify.cmd,
                    "landed": false,
                    "reason": reason,
                    "note": "verified green; NOTHING was applied — landing requires a human at a \
                             terminal: run `loom propose` there (it re-runs the verify and asks)",
                })
                .to_string()),
                WeaveDisposition::Landed { .. } => {
                    Err("unreachable: AutoDeny never consents".to_string())
                }
            }
        }
        "loom_adopt" => {
            let thread_id = arg(args, "thread_id").ok_or("loom_adopt needs a thread_id")?;
            // Local first; unknown + a sync remote → the cross-machine
            // claims flow (first CAS push wins; losing names the winner).
            let (thread, lease) = match engine.adopt(thread_id, &holder()) {
                Ok(pair) => pair,
                Err(e) if e.contains("no thread with id") => {
                    let repo = repo_of(engine, args)?;
                    if repo.sync_remote.is_some() {
                        loom::sync::adopt_remote(engine, &repo.id, thread_id, &holder())?
                    } else {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            };
            solo::set(&engine.base(), &thread.repo_id, &lease.id, &thread.id);
            let working_dir = thread.worktree.clone();
            Ok(json!({
                "thread": thread,
                "lease": lease,
                "working_dir": working_dir,
                "note": "you inherited the goal, criteria and scope; the last stitch is \
                         materialized in working_dir (when set) — cd there and continue",
            })
            .to_string())
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

fn loom_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
