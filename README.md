# Loom

*Version control for many hands moving at once.*

Git assumes a small number of long-lived branches, merged occasionally, by
humans who coordinate out-of-band. Agent development breaks every one of
those assumptions: agents checkpoint every few seconds, collide constantly,
and die mid-refactor leaving dirty worktrees nobody owns — and git notices
the collision hours later, at merge time. Loom is not a git replacement for
humans reviewing PRs. It is the **high-frequency collaboration layer that
sits above git**: agents declare **intent leases** before touching files,
checkpoint **stitches** every few seconds, and land on the shared **fabric**
only through a **weave gate** that verifies green first — so the answer to
"is main green?" is "main is green", by construction. Crashed work becomes an
adoptable **orphan**, never a mess, and the git bridge projects every landed
weave down into an ordinary local git commit so your existing tools keep
working.

The full design (and the honesty invariants the engine enforces) is in
[`docs/DESIGN.md`](docs/DESIGN.md).

## Install

```bash
cargo install --path .        # or: cargo build --release
```

One binary: `loom`. State lives in `~/.loom` (override with `LOOM_DATA`).

## Quickstart — the two-terminal demo

Two agents (or two of you), one repo, zero merge conflicts.

```bash
# Once, in the repo:
loom init --verify "cargo check"     # add --git-bridge for one local git
                                     # commit per landed weave (never a push)
```

**Terminal A:**

```bash
loom lease "extract the tokenizer" 'src/a/**' --criteria "cargo check passes"
# edit files under src/a/ …
loom stitch          # checkpoint — seconds-cheap, repeat as you go
loom propose         # runs the verify in a scratch copy; on green it shows
                     # you repo, goal and verify result, then asks y/N —
                     # NOTHING touches the working tree before your yes
```

**Terminal B**, at the same time:

```bash
loom lease "tighten the emitter" 'src/b/**'
# edit files under src/b/ …
loom stitch
loom propose
```

The scopes don't overlap, so neither lease warned. (Lease `src/a/**` twice to
see a toe-step warning with a suggested split — the second lease still
succeeds; a lease warns, it never blocks.) Approve both proposes at their
terminals and `loom log` shows two green weaves, the newest as the fabric
tip. With `--git-bridge` on, `git log` shows one tidy commit per weave —
goal, criteria, and verify result in the message.

## The orphan demo — crash-safety for work, not just data

Kill a worker mid-flow and nothing is lost:

```bash
loom lease "rename the config keys" 'src/**' --ttl-ms 30000
# edit, then:
loom stitch
# …now close the terminal and walk away.
```

When the lease's TTL passes with no heartbeat (stitching heartbeats for
you), the thread appears in `loom status` as **orphaned** — goal, acceptance
criteria and last stitch attached. From any other terminal:

```bash
loom status                  # shows the orphan queue
loom adopt <thread-id>       # same lease: goal, criteria, scope preserved
loom stitch && loom propose  # continue from the last stitch
```

## Works with Claude Code (or any MCP client): `loom mcp`

`loom mcp` is a stdio MCP server exposing `loom_status`, `loom_lease`,
`loom_stitch`, `loom_propose`, `loom_adopt` — so an agent session can lease,
checkpoint and propose without the CLI. Register it, e.g. for Claude Code:

```bash
claude mcp add loom -- loom mcp
```

Honesty at the protocol edge: the MCP server's stdin is the protocol
channel, so there is no terminal to ask a human at. `loom_propose` runs the
verify for real (in a scratch copy) and reports green or red — but the
**apply is always refused** in MCP with the reason stated; a human lands it
by running `loom propose` at a terminal. Proposing verifies; it never lands.

## What the engine promises (and how)

- **A lease is knowledge, not a lock.** Overlap warns at declaration time —
  the moment coordination is cheap — with both goals and a suggested split.
  Nothing ever blocks an agent from working.
- **A stitch only reads.** Snapshots are content-addressed under the data
  dir; nothing is written into your repo.
- **Red never lands.** The weave gate verifies in a scratch copy with a hard
  timeout; the fabric tip advances only on green *plus* an explicit human
  yes, and only if no other weave landed in between (stale parents must
  re-propose).
- **The git bridge never pushes.** One local commit per landed weave,
  opt-in per repo.

## v1 scope / not yet

Honest edges of what exists today:

- **Single machine.** No federation, no gossip, no peer sync — the objects
  carry stable ids and serialize cleanly so the same model can gossip later,
  but today there is exactly one peer: your machine.
- **Whole-file snapshots**, not rolling-hash chunking. Identical content
  dedups; large files with small edits pay full price (files > 8 MB are
  skipped and reported).
- **Whole-repo verify**, not per-slice test impact. The verify command runs
  against the full scratch copy.
- **Deletions aren't tracked** in stitches: a stitch snapshots what exists;
  a file the thread deleted simply stops appearing in the manifest.
- **Excluded dirs are hard-coded** (`.git`, `target`, `node_modules`) — a
  minimal .gitignore stand-in, not a gitignore parser.
- **No draft-branch export yet** (`loom export` is future work); the bridge
  commits landed weaves to the current branch only.
- **Solo pointer, not multi-seat CLI:** one current lease per repo per data
  dir. Multiple local agents can share a repo today by using explicit ids or
  one `LOOM_DATA` per agent.

## License

MIT — see [LICENSE](LICENSE). Copyright (c) 2026 Aether-OS contributors.
