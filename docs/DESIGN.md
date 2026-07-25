# Loom — version control for many hands moving at once

*Threads woven into a fabric that is green by construction.*

## Why git is the wrong shape for agent-scale collaboration

Git assumes a small number of long-lived branches, merged occasionally, by
humans who coordinate out-of-band. Agent development breaks every one of those
assumptions at once:

- **Frequency.** An agent checkpoints every few seconds, not every few hours.
  Commit ceremony (stage → message → merge dance) is overhead per keystroke.
- **Concurrency.** Ten agents on one repo collide constantly. Git detects the
  collision *at merge time* — hours after the toes were stepped on. The
  coordination information existed the whole time; git just has nowhere to put
  it.
- **Green is a snapshot, not an invariant.** CI tells you a commit *was* green
  once. Nothing in git makes the shared line *stay* green while ten writers
  race it.
- **Death is unmodeled.** An agent OOM-killed mid-refactor leaves a dirty
  worktree nobody owns. Git has no concept of "work in flight, owner gone,
  adoptable."
- **Distribution is theoretical.** Git is distributed in storage but
  centralized in practice (one origin, one trunk). Machines that pair up to
  share work need peers that sync continuously without a blessed center.

Loom is not a git replacement for humans reviewing PRs. It is the
**high-frequency collaboration layer that sits above git**: agents live in
Loom; Loom projects its history down into ordinary git commits so every
existing tool, host, and habit keeps working.

## The five ideas

### 1. Intent leases — say what you're about to touch
Before editing, a thread (one agent's work-line) declares an **intent lease**:
a scope (path globs), a goal in one sentence, and acceptance criteria
(the same shape a done-checker would ask for). Leases are visible to every
other thread immediately, heartbeat-renewed, and TTL-expired. Overlap does not block — it
**warns at declaration time**, the moment coordination is still cheap: both
threads see the collision, with a suggested split of the scope. A lease is not
a lock; it is knowledge, enforced only where it must be (at the weave).

### 2. Stitches — commits at the frequency agents actually work
A **stitch** is a micro-snapshot of the leased scope: content-addressed and
deduplicated (v1: whole files; rolling-hash chunking and per-machine
signatures are future work). Stitching every few seconds costs bytes, not
ceremony — no message required; the lease's goal *is* the message. A thread
is a chain of stitches, durable the moment it is written. In the federated
design, peers stream each other's stitches live — live cursors for code,
without a central server.

### 3. The fabric — a mainline that is green by construction
The shared line is called the **fabric**. Nothing lands on it by push. A
thread proposes a **weave**; the weave gate replays the thread's diff onto the
current fabric tip, runs the verify command for the affected slice (test
impact is tracked incrementally per scope), and advances the fabric only on
green. Red never lands — not "shouldn't land," *cannot*: fabric advancement
is a single atomic operation whose precondition is a green verify plus an
explicit human yes (in the federated design, performed by whichever peer
holds the rotating **shuttle token**). The answer to "is main green?" is
"main is green" — by construction, always.

### 4. Orphans — crash-safety for work, not just data
When a thread's lease stops heart-beating — agent crashed, machine died,
session hit its context limit mid-flow — the thread becomes an **orphan**:
last stitch seconds old, goal and acceptance criteria attached, scope known.
Orphans appear in every peer's queue as adoptable work parcels. Any agent (or
human) **adopts** an orphan: takes over the lease, reads the goal, continues
from the last stitch. Nothing is ever lost, and nothing dirty is ever left
lying around unowned — abrupt death is a normal, recoverable state of work.

### 5. The git bridge — meet every developer where they live
The fabric exports to a plain git branch: one git commit per weave, message
composed from the lease goal + criteria + verify result. Threads can export as
draft branches for human review. Humans keep GitHub, `git log`, bisect, blame;
agents keep stitches, leases, and live sync. `git bisect` over weaves is
strictly better than over commits: every point in fabric history passed its
verify by construction.

## Trust boundary

Loom automates coordination, never consent. A stitch **only reads** the repo
— snapshots go into the data dir, nothing is written back. The weave gate
verifies **in a scratch copy**, never the real tree. Applying a green weave
to the real working tree is an *action*: it happens only past an explicit
human yes, expressed through the `WeaveConsent` trait — the standalone
binary asks y/N at the terminal (showing repo, goal and verify result
first), refuses outright when stdin is not a terminal, and the MCP server
always refuses (its stdin is the protocol channel; no human is at it). A
host that embeds the engine can implement consent over its own approvals
queue instead — "auto-weave on green" would be an explicit, revocable grant,
off by default, and is not implemented here. In the federated design,
adopting an orphan from a remote peer gates the same way: work changes
machines only past a human yes.

## Objects (all content-addressed; signatures are future work)

| object | fields |
|---|---|
| `Lease` | id, thread, scope[], goal, criteria[], holder (peer key), ttl, heartbeat |
| `Stitch` | id, thread, parent, lease, chunk-manifest, ts, sig |
| `Thread` | id, goal, stitches head, lease, status: active · proposed · woven · orphaned · adopted |
| `Weave` | id, thread, fabric-parent, verify {cmd, slice, result, log-digest}, shuttle-sig |
| `Fabric` | repo id, tip weave, always-green invariant |

Storage is a blob store + append-only logs — boring on purpose: JSON state,
JSONL events, content-addressed whole-file blobs under `~/.loom` (override
with `LOOM_DATA`), 0o600, bounded, corrupt-line tolerant. In the federated
design, sync is gossip: peers exchange log heads, fetch missing objects,
verify signatures; there is no central server and no blessed peer. The
shuttle token rotates by deterministic schedule among live peers
(lowest-hash-of-(epoch, peer-key) wins); a dead shuttle-holder is skipped
after TTL, so fabric advancement survives any single machine's death. None
of that gossip exists in v1 — see the scope section below.

## What a developer feels

- `loom init` in a repo; other sessions' threads appear in `loom status`.
- Your agent says "refactoring parser, leased `src/parse/**`" and every other
  agent routes around it *before* conflict, not after.
- Main is green. Always. You stop asking.
- A session dies at 3am mid-migration; at 9am you (or the next agent) adopt
  its orphan and continue from the last five-seconds-old stitch — goal and
  acceptance criteria attached.
- Your repo on GitHub looks like a tidy history of green, well-described
  commits — written by a loom you watched weave in real time.

## v1 in this crate (what exists today)

- `src/` — the engine: content-addressed blob store, stitch log, leases with
  TTL/heartbeat, thread lifecycle incl. orphan/adopt, weave gate
  (configurable verify command, default `cargo check`, hard timeout, scratch
  copy), fabric log, git bridge (one local commit per landed weave — never a
  push), and the `WeaveConsent` trait with `TerminalConsent` (interactive
  y/N) and `AutoDeny` (non-interactive: refuses, states why).
- `loom` CLI verbs: `init · lease · stitch · propose · withdraw · adopt ·
  status · log`. A solo-mode pointer in the data dir remembers your current
  lease per repo, so the everyday verbs need no ids.
- `loom mcp` — a stdio MCP server exposing `loom_status · loom_lease ·
  loom_stitch · loom_propose · loom_adopt`, so any agent session can use
  Loom without the CLI. Proposing runs the verify; landing asks the human at
  a terminal or is refused when non-interactive.
- **Not yet** (design above, honestly absent below): federation gossip and
  peer sync; object signatures; the shuttle token; rolling-hash chunking
  (v1 snapshots whole files, dedup by sha256); per-slice test impact (v1
  verifies the whole scratch copy); deletion tracking in stitches; gitignore
  parsing (`.git`, `target`, `node_modules` are hard-coded excludes);
  per-thread draft-branch export (`loom export`).

## Quickstart — the two-terminal demo

Two agents, one repo, zero merge conflicts, one dead agent recovered.

```bash
# Once, in the repo:
loom init --verify "cargo check"       # add --git-bridge for one commit per
                                       # landed weave (current branch, never
                                       # a push)
```

**Terminal A** (agent A):

```bash
loom lease "extract the tokenizer" 'src/a/**' --criteria "cargo check passes"
# edit files under src/a/ …
loom stitch          # checkpoint — seconds-cheap, repeat as you go
loom propose         # runs the verify in a scratch copy; on green it shows
                     # repo, goal and verify result, then asks y/N before
                     # anything touches the working tree
```

**Terminal B** (agent B), at the same time:

```bash
loom lease "tighten the emitter" 'src/b/**'
# edit files under src/b/ …
loom stitch
loom propose
```

The scopes don't overlap, so neither lease warned. (Lease `src/a/**` twice to
see a toe-step warning with a suggested split — the second lease still
succeeds; a lease warns, it never blocks.) Answer `y` at each terminal:
each weave applies to the working tree only after that yes, and `loom log`
now shows two green weaves, the newest as the fabric tip.

**Now kill agent A mid-flow.** Lease again in terminal A (short TTL so the
demo doesn't wait 30 minutes), start editing, stitch once — then close the
terminal and walk away:

```bash
loom lease "rename the config keys" 'src/a/**' --ttl-ms 30000   # then die
```

When the lease's TTL passes with no heartbeat, the thread appears in
`loom status` as **orphaned** — goal, criteria and last stitch attached.
From terminal B, `loom adopt <thread-id>`: the adopter takes over the same
lease, reads the goal, and continues from the last stitch. Nothing was lost,
and nothing dirty is left unowned.

Agent sessions do the same thing without the CLI via `loom mcp` —
`loom_lease → loom_stitch → loom_propose` — and every green propose still
waits for the same human yes at a terminal. Per-thread draft-branch export
(`loom export`) is future work and says so.
