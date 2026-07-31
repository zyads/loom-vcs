// Heddle — version control for many hands moving at once.
// Copyright (c) 2026 Aether-OS contributors. MIT license; see LICENSE.

//! Zero-prompt agent enrollment — `heddle init` / `heddle adopt` (no thread
//! id) wire a repo so any Claude Code session picks Heddle up WITHOUT the
//! user ever telling it to. Three files, all in the repo:
//!
//! * **`.mcp.json`** — the project-scope MCP registry Claude Code actually
//!   reads (its `settings.json` has no `mcpServers` key): registers the
//!   `heddle mcp` stdio server.
//! * **`.claude/settings.json`** — `enableAllProjectMcpServers: true` so the
//!   server needs no first-use approval prompt, plus a `SessionStart` hook
//!   running `heddle status --brief`: hook stdout lands in the agent's
//!   context, so a fresh session sees live threads and their scopes unasked
//!   (and sees nothing at all when no threads are live).
//! * **`CLAUDE.md`** — a short imperative section between marker comments:
//!   what Heddle is, lease-before-edit, the exact commands, and the two rules
//!   agents get wrong (bare verbs refuse when ambiguous; out-of-lease edits
//!   are dropped by export, so they must be flagged instead).
//!
//! **The write rules** (the operator's law): everything to be written is
//! shown BEFORE it is written; every write is idempotent and re-runnable;
//! merges preserve the user's existing content and key order (`serde_json`
//! `preserve_order`); a file this module cannot parse is refused with the
//! reason, never overwritten; `--dry-run` prints the plan and touches
//! nothing. Existing user config always wins: a key the user already set —
//! even to a value Heddle would not pick — is left exactly as it is.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// The hook command a fresh agent session runs. Silent when no threads are
/// live, so it never spends context on an empty repo.
pub const BRIEF_HOOK_CMD: &str = "heddle status --brief";

/// Marker pair for the managed CLAUDE.md block. Idempotency hangs on these
/// exact strings — never reword them without a migration.
pub const MARK_BEGIN: &str =
    "<!-- heddle:begin — managed by `heddle adopt`; edits between these markers are overwritten -->";
pub const MARK_END: &str = "<!-- heddle:end -->";

/// The CLAUDE.md section, markers included. Deliberately terse — it competes
/// for context with everything else the agent reads.
pub fn claude_md_block() -> String {
    // NB: `\`-continuations strip the next line's leading whitespace, so the
    // markdown indents live at the END of the preceding fragment (`\n   `).
    format!(
        "{MARK_BEGIN}\n\
## Heddle — agent coordination (this repo uses it)\n\
\n\
Heddle keeps concurrent agent sessions from silently overwriting each other:\n\
leased scopes, an isolated git worktree per task, a mainline that only moves\n\
green. If you are an agent editing this repo, coordinate through it:\n\
\n\
1. `heddle status --brief` — see live threads and their leased scopes first.\n\
2. `heddle lease \"<goal>\" <scope-globs...> --isolated` — declare BEFORE\n   \
editing, then cd into the printed `WORK IN:` worktree and edit ONLY there.\n\
3. `heddle stitch --lease <id>` — checkpoint every few edits (heartbeats too).\n\
4. `heddle export --thread <id>` — your chain onto a review branch.\n   \
`heddle propose` verifies; landing always waits for a human at a terminal.\n\
\n\
Rules that are not optional:\n\
- With several live threads, bare verbs REFUSE rather than guess — pass\n  \
`--lease <id>`/`--thread <id>`, or run from inside your own worktree.\n\
- Edits OUTSIDE your leased scope are DROPPED by stitch and export. Never fix\n  \
an out-of-scope file silently: FLAG it in your report, or lease it too.\n\
- A TOE-STEP warning means a live thread overlaps your scope: take the\n  \
suggested split or coordinate — do not proceed overlapped.\n\
{MARK_END}"
    )
}

/// One file the plan touches: what changes, what was already there, and the
/// exact content shown to the operator before anything is written.
#[derive(Clone, Debug)]
pub struct PlannedFile {
    pub path: PathBuf,
    pub existed: bool,
    /// False = nothing to do for this file; `new_content` then mirrors disk.
    pub changed: bool,
    /// Human lines: what this run adds.
    pub added: Vec<String>,
    /// Human lines: what was already in place (left untouched).
    pub already: Vec<String>,
    /// What to show the operator: full content for the small JSON files,
    /// just the managed block for CLAUDE.md (the rest is the user's file).
    pub display: String,
    /// The full bytes `apply` writes when `changed`.
    new_content: String,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub files: Vec<PlannedFile>,
}

impl Plan {
    pub fn any_changes(&self) -> bool {
        self.files.iter().any(|f| f.changed)
    }

    /// Write every changed file (creating `.claude/` as needed). Returns the
    /// paths written. Never called by `--dry-run`.
    pub fn apply(&self) -> Result<Vec<PathBuf>, String> {
        let mut written = Vec::new();
        for f in self.files.iter().filter(|f| f.changed) {
            if let Some(dir) = f.path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            }
            std::fs::write(&f.path, &f.new_content)
                .map_err(|e| format!("cannot write {}: {e}", f.path.display()))?;
            written.push(f.path.clone());
        }
        Ok(written)
    }
}

/// Compute the whole enrollment plan for a repo. Read-only: looking is free,
/// writing is [`Plan::apply`]'s job.
pub fn plan(repo_root: &Path) -> Result<Plan, String> {
    Ok(Plan {
        files: vec![
            plan_mcp_json(repo_root)?,
            plan_settings(repo_root)?,
            plan_claude_md(repo_root)?,
        ],
    })
}

/// Read a JSON file into an order-preserving object map; a missing or empty
/// file is an empty object; anything unparseable is refused, never clobbered.
fn read_json_object(path: &Path) -> Result<(bool, Map<String, Value>), String> {
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(_) => return Ok((false, Map::new())),
    };
    if body.trim().is_empty() {
        return Ok((true, Map::new()));
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| {
        format!(
            "refusing to touch {}: it is not valid JSON ({e}) — fix or remove it, then re-run",
            path.display()
        )
    })?;
    match v {
        Value::Object(m) => Ok((true, m)),
        _ => Err(format!(
            "refusing to touch {}: expected a JSON object at the top level",
            path.display()
        )),
    }
}

fn pretty(map: &Map<String, Value>) -> String {
    let mut s = serde_json::to_string_pretty(&Value::Object(map.clone()))
        .unwrap_or_else(|_| "{}".to_string());
    s.push('\n');
    s
}

/// `.mcp.json`: register the stdio server under `mcpServers.heddle`. An
/// existing `heddle` entry — however the user shaped it — is left alone.
fn plan_mcp_json(repo_root: &Path) -> Result<PlannedFile, String> {
    let path = repo_root.join(".mcp.json");
    let (existed, mut map) = read_json_object(&path)?;
    let mut added = Vec::new();
    let mut already = Vec::new();
    let servers = map
        .entry("mcpServers".to_string())
        .or_insert_with(|| json!({}));
    let Some(servers) = servers.as_object_mut() else {
        return Err(format!(
            "refusing to touch {}: mcpServers is not an object",
            path.display()
        ));
    };
    let mut changed = false;
    if servers.contains_key("heddle") {
        already.push("mcpServers.heddle (left exactly as you have it)".into());
    } else {
        servers.insert(
            "heddle".to_string(),
            json!({"type": "stdio", "command": "heddle", "args": ["mcp"]}),
        );
        added.push("mcpServers.heddle → `heddle mcp` (stdio) — project-scope, read by any Claude Code session here".into());
        changed = true;
    }
    let new_content = pretty(&map);
    Ok(PlannedFile {
        path,
        existed,
        changed,
        added,
        already,
        display: new_content.clone(),
        new_content,
    })
}

/// `.claude/settings.json`: pre-approve the project's MCP servers and hook
/// `heddle status --brief` into SessionStart. Keys the user already set are
/// never changed — even `enableAllProjectMcpServers: false` stands.
fn plan_settings(repo_root: &Path) -> Result<PlannedFile, String> {
    let path = repo_root.join(".claude").join("settings.json");
    let (existed, mut map) = read_json_object(&path)?;
    let mut added = Vec::new();
    let mut already = Vec::new();
    let mut changed = false;

    match map.get("enableAllProjectMcpServers") {
        Some(v) => already.push(format!(
            "enableAllProjectMcpServers: {v} (your setting — left as-is)"
        )),
        None => {
            map.insert("enableAllProjectMcpServers".to_string(), json!(true));
            added.push(
                "enableAllProjectMcpServers: true — pre-approves servers in THIS repo's \
                 .mcp.json (heddle, plus anything else this repo defines); delete the key \
                 if you'd rather keep the one-time approval prompt"
                    .into(),
            );
            changed = true;
        }
    }

    let hooks = map.entry("hooks".to_string()).or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        return Err(format!(
            "refusing to touch {}: \"hooks\" is not an object",
            path.display()
        ));
    };
    let session_start = hooks
        .entry("SessionStart".to_string())
        .or_insert_with(|| json!([]));
    let Some(session_start) = session_start.as_array_mut() else {
        return Err(format!(
            "refusing to touch {}: hooks.SessionStart is not an array",
            path.display()
        ));
    };
    let hook_present = session_start.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hs| {
                hs.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|c| c.contains(BRIEF_HOOK_CMD))
                })
            })
            .unwrap_or(false)
    });
    if hook_present {
        already.push(format!("SessionStart hook `{BRIEF_HOOK_CMD}`"));
    } else {
        session_start.push(json!({
            "hooks": [{"type": "command", "command": BRIEF_HOOK_CMD}]
        }));
        added.push(format!(
            "SessionStart hook `{BRIEF_HOOK_CMD}` — its stdout lands in the agent's \
             context, so a fresh session sees live threads unasked (silent when none)"
        ));
        changed = true;
    }

    let new_content = pretty(&map);
    Ok(PlannedFile {
        path,
        existed,
        changed,
        added,
        already,
        display: new_content.clone(),
        new_content,
    })
}

/// `CLAUDE.md`: the managed block between markers. Appended when absent,
/// rewritten in place when stale, untouched when current. A lone marker is a
/// half-edited file — refused with the reason, never guessed at.
fn plan_claude_md(repo_root: &Path) -> Result<PlannedFile, String> {
    let path = repo_root.join("CLAUDE.md");
    let body = std::fs::read_to_string(&path).unwrap_or_default();
    let existed = !body.is_empty() || path.exists();
    let block = claude_md_block();
    let mut added = Vec::new();
    let mut already = Vec::new();

    let (new_content, changed) = match (body.find(MARK_BEGIN), body.find(MARK_END)) {
        (Some(b), Some(e)) if e > b => {
            let current = &body[b..e + MARK_END.len()];
            if current == block {
                already.push("heddle section (markers present, content current)".into());
                (body.clone(), false)
            } else {
                added.push("heddle section rewritten between the existing markers (it was stale)".into());
                let mut s = String::with_capacity(body.len() + block.len());
                s.push_str(&body[..b]);
                s.push_str(&block);
                s.push_str(&body[e + MARK_END.len()..]);
                (s, true)
            }
        }
        (None, None) => {
            added.push(format!(
                "heddle section appended between markers ({} lines)",
                block.lines().count()
            ));
            let mut s = body.clone();
            if !s.is_empty() && !s.ends_with("\n\n") {
                if !s.ends_with('\n') {
                    s.push('\n');
                }
                s.push('\n');
            }
            s.push_str(&block);
            s.push('\n');
            (s, true)
        }
        _ => {
            return Err(format!(
                "refusing to touch {}: found one heddle marker without its pair — \
                 restore or delete the stray marker, then re-run",
                path.display()
            ))
        }
    };

    Ok(PlannedFile {
        path,
        existed,
        changed,
        added,
        already,
        display: format!("{block}\n"),
        new_content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "heddle-enroll-{tag}-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mk scratch");
        p
    }

    #[test]
    fn a_bare_repo_gets_all_three_files_and_a_rerun_changes_nothing() {
        let root = scratch("bare");
        let p = plan(&root).expect("plan");
        assert!(p.any_changes());
        assert_eq!(p.files.iter().filter(|f| f.changed).count(), 3);
        let written = p.apply().expect("apply");
        assert_eq!(written.len(), 3);
        // The written artifacts are what Claude Code needs.
        let mcp: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(mcp["mcpServers"]["heddle"]["command"], "heddle");
        assert_eq!(mcp["mcpServers"]["heddle"]["args"][0], "mcp");
        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings["enableAllProjectMcpServers"], true);
        assert_eq!(
            settings["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            BRIEF_HOOK_CMD
        );
        let md = std::fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        assert!(md.contains(MARK_BEGIN) && md.contains(MARK_END));
        assert!(md.contains("DROPPED by stitch and export"), "the export rule is in");
        // Idempotent: a second plan finds nothing to do.
        let p2 = plan(&root).expect("plan 2");
        assert!(!p2.any_changes(), "{p2:?}");
        assert_eq!(p2.files.iter().flat_map(|f| f.already.iter()).count(), 4);
    }

    #[test]
    fn merging_preserves_existing_content_and_key_order() {
        let root = scratch("merge");
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        // Keys deliberately NOT alphabetical, with the user's own hook.
        std::fs::write(
            root.join(".claude/settings.json"),
            r#"{
  "model": "opus",
  "permissions": { "allow": ["Bash(npm test)"] },
  "hooks": {
    "SessionStart": [
      { "matcher": "startup", "hooks": [{ "type": "command", "command": "echo hi" }] }
    ],
    "PostToolUse": []
  },
  "env": { "FOO": "1" }
}
"#,
        )
        .unwrap();
        std::fs::write(
            root.join(".mcp.json"),
            r#"{ "mcpServers": { "zeta": { "command": "zeta" } } }"#,
        )
        .unwrap();
        let p = plan(&root).expect("plan");
        p.apply().expect("apply");
        let body = std::fs::read_to_string(root.join(".claude/settings.json")).unwrap();
        // The user's keys survive, in THEIR order — heddle's additions come after.
        let order: Vec<usize> = ["\"model\"", "\"permissions\"", "\"hooks\"", "\"env\""]
            .iter()
            .map(|k| body.find(k).unwrap_or_else(|| panic!("{k} kept: {body}")))
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "key order kept: {body}");
        assert!(body.contains("echo hi"), "user hook kept");
        assert!(body.contains(BRIEF_HOOK_CMD), "heddle hook added");
        let settings: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            settings["hooks"]["SessionStart"].as_array().unwrap().len(),
            2,
            "appended a group, replaced nothing"
        );
        // .mcp.json: the user's server survives beside heddle's.
        let mcp: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join(".mcp.json")).unwrap())
                .unwrap();
        assert!(mcp["mcpServers"]["zeta"].is_object());
        assert!(mcp["mcpServers"]["heddle"].is_object());
        // Second run: byte-identical files.
        let before = (
            std::fs::read_to_string(root.join(".claude/settings.json")).unwrap(),
            std::fs::read_to_string(root.join(".mcp.json")).unwrap(),
        );
        let p2 = plan(&root).expect("plan 2");
        assert!(!p2.any_changes());
        p2.apply().expect("apply 2 is a no-op");
        assert_eq!(
            before.0,
            std::fs::read_to_string(root.join(".claude/settings.json")).unwrap()
        );
        assert_eq!(before.1, std::fs::read_to_string(root.join(".mcp.json")).unwrap());
    }

    #[test]
    fn user_settings_are_respected_even_when_heddle_disagrees() {
        let root = scratch("respect");
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        // The user explicitly said NO to auto-approval, and already routed
        // their own heddle MCP entry through an absolute path.
        std::fs::write(
            root.join(".claude/settings.json"),
            r#"{ "enableAllProjectMcpServers": false }"#,
        )
        .unwrap();
        std::fs::write(
            root.join(".mcp.json"),
            r#"{ "mcpServers": { "heddle": { "command": "/opt/heddle", "args": ["mcp", "-v"] } } }"#,
        )
        .unwrap();
        let p = plan(&root).expect("plan");
        p.apply().expect("apply");
        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            settings["enableAllProjectMcpServers"], false,
            "an explicit false is the user's call, not heddle's"
        );
        let mcp: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(mcp["mcpServers"]["heddle"]["command"], "/opt/heddle");
    }

    #[test]
    fn claude_md_block_is_appended_once_replaced_when_stale_and_never_eats_the_file() {
        let root = scratch("md");
        std::fs::write(root.join("CLAUDE.md"), "# My project\n\nUser prose stays.\n").unwrap();
        let p = plan(&root).expect("plan");
        p.apply().expect("apply");
        let md = std::fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        assert!(md.starts_with("# My project"), "user prose first: {md}");
        assert!(md.contains("User prose stays."));
        assert_eq!(md.matches(MARK_BEGIN).count(), 1);
        // Re-run: unchanged.
        let p2 = plan(&root).expect("plan 2");
        assert!(!p2.files.iter().find(|f| f.path.ends_with("CLAUDE.md")).unwrap().changed);
        // Staleness: doctor the block body; the next run rewrites BETWEEN the
        // markers and leaves everything outside them alone.
        let doctored = md.replace("Rules that are not optional", "Rules that are optional");
        std::fs::write(root.join("CLAUDE.md"), &doctored).unwrap();
        let p3 = plan(&root).expect("plan 3");
        p3.apply().expect("apply 3");
        let md3 = std::fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        assert!(md3.contains("Rules that are not optional"), "block restored");
        assert!(md3.contains("User prose stays."), "prose untouched");
        assert_eq!(md3.matches(MARK_BEGIN).count(), 1, "no duplicate block");
        // A lone marker is refused, not guessed at.
        std::fs::write(root.join("CLAUDE.md"), format!("hello\n{MARK_BEGIN}\n")).unwrap();
        let err = plan(&root).unwrap_err();
        assert!(err.contains("without its pair"), "{err}");
    }

    #[test]
    fn unparseable_json_is_refused_never_overwritten() {
        let root = scratch("corrupt");
        std::fs::write(root.join(".mcp.json"), "{ not json").unwrap();
        let err = plan(&root).unwrap_err();
        assert!(err.contains("refusing to touch"), "{err}");
        assert_eq!(
            std::fs::read_to_string(root.join(".mcp.json")).unwrap(),
            "{ not json",
            "the broken file is exactly as the user left it"
        );
    }

    #[test]
    fn a_plan_alone_writes_nothing_dry_run_contract() {
        let root = scratch("dry");
        let p = plan(&root).expect("plan");
        assert!(p.any_changes());
        // The dry-run path prints `display` and never calls apply(): the
        // contract is that plan() itself must not have touched disk.
        assert!(!root.join(".mcp.json").exists());
        assert!(!root.join(".claude").exists());
        assert!(!root.join("CLAUDE.md").exists());
        // And every changed file carries the exact content it WOULD write.
        for f in &p.files {
            assert!(!f.display.is_empty(), "{:?} shows what it would write", f.path);
        }
    }

    #[test]
    fn the_claude_md_block_stays_short_enough_to_deserve_its_context() {
        let block = claude_md_block();
        let inner = block.lines().count() - 2; // markers excluded
        assert!(
            inner <= 22,
            "the block competes for context with everything else — {inner} lines is too many"
        );
        for needle in [
            "heddle lease",
            "--isolated",
            "heddle stitch --lease",
            "heddle export --thread",
            "REFUSE",
            "FLAG",
        ] {
            assert!(block.contains(needle), "block must teach {needle:?}:\n{block}");
        }
    }
}
