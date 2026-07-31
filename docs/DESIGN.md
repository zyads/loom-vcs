# Heddle — version control for many hands moving at once

*Threads woven into a fabric that is green by construction.*

## Why git is the wrong shape for agent-scale collaboration

Git assumes a small number of long-lived branches, merged occasionally, by
humans who coordinate out-of-band. Agent development breaks every one of those
assumptions at once:

- **Frequency.** An agent checkpoints every few seconds, not every few hours.
  Commit ceremony (stage → message → merge dance) is overhead per keystroke.
- **Concurrency.** Ten agents on one repo collide constantly. Git detects the
  collision *at merge time* — hours after the toes were stepped on, after the
  tokens are spent. The coordination information existed the whole time; git
  just has nowhere to put it. A branch name is a string; nothing in git is a
  machine-readable "I am doing X to these files", so agents cannot
  self-partition.
- **Green is a snapshot, not an invariant.** CI tells you a commit *was* green
  once. Nothing in git makes the shared line *stay* green while ten writers
  race it.
- **Death is unmodeled.** An agent OOM-killed mid-refactor leaves a dirty
  worktree nobody owns. Git has no concept of "work in flight, owner gone,
  adoptable."
- **Merge hands reconciliation to the wrong party.** Whoever merges last —
  often a fresh session with neither author's context — reconciles two
  semantic rewrites, the most expensive and error-prone work agents do. The
  goal is not faster rebases; it is **rare** rebases.

Heddle is not a git replacement for humans reviewing PRs. It is the
coordination layer that sits above git: agents live in Heddle; Heddle projects
its history down into ordinary git commits so every existing tool, host, and
habit keeps working.

None of the diagnosis above is unique to this design. Several projects have
converged on worktree isolation plus gated merging — daemon-run hierarchical
teams (batty), web control planes with dispatch and merge stewards
(stoneforge), coordination servers with file locks and presence (aweb),
forge-label orchestration (rjwalters/loom), single-agent delivery harnesses
(valkor-ai/loom). That convergence is evidence the problem is real, and this
document claims none of those pieces as its own. What it does claim as its
distinct core is the combination delivered without a platform: leaderless,
warn-only intent leases; an always-green fabric advanced only by
compare-and-swap over bare git remotes; decentralized orphan adoption; and
stitch-level history — a serverless, git-native protocol and one binary. The
README's "Adjacent projects" section places each neighbor precisely.

## The six ideas

### 1. Worktree isolation — a tree per task
Every thread (one agent's work-line) gets its own **git worktree**, detached
at HEAD, under the heddle data dir — outside the repo tree, invisible to other
threads' scopes and to git status. The holder edits there and only there.
Right after creation the worktree's scope is **aligned to the repo's live
tree** (the fabric may be ahead of git HEAD, since landed weaves sit in the
working tree until committed); that aligned state is captured as the
thread's **base** — the reference point for every merge decision later.
In-place mode (`--in-place`, or any non-git repo) skips all of this and
edits the repo directly, v0.1-style; isolation failures under Auto degrade
to in-place with the reason noted on the thread, never silently.

### 2. Intent leases — say what you're about to touch
Before editing, a thread declares an **intent lease**: a scope (path globs),
a goal in one sentence, and acceptance criteria. Leases are visible to every
other thread immediately (and to other machines after a sync), heartbeat-
renewed, TTL-expired. Overlap does not block — it **warns at declaration
time**, the moment coordination is still cheap: both threads see the
collision, with a suggested split. A lease is not a lock; it is knowledge,
enforced only where it must be (at the weave).

### 3. Stitches — commits at the frequency agents actually work
A **stitch** is a micro-snapshot of the leased scope in the thread's working
dir: content-addressed, deduplicated, deletion-aware. Stitching every few
seconds costs bytes, not ceremony — no message required; the lease's goal
*is* the message. A thread is a chain of stitches, durable the moment it is
written.

### 4. The fabric — a mainline that is green by construction
The shared line is called the **fabric**. Nothing lands on it by push. A
thread proposes a **weave**; the gate copies the repo to scratch, overlays
the thread's delta, runs the verify command there, and records the outcome.
Landing a green weave is a separate, consent-gated step with file-level
merge rules (below). Red never lands — not "shouldn't," *cannot*: fabric
advancement is a single operation whose preconditions are a green verify, an
explicit yes, an unmoved fabric parent, and no merge conflicts.

The verify command is therefore on the critical path of every propose, for
every agent — so it should take a few seconds, not minutes. Configure it as a
fast subset (`cargo check`, `pytest -q -m "not slow"`, `make test-fast`) and
leave the full suite to CI; `heddle init` times it once and warns when it is
slower than ~5s. A gate people wait on is a gate people turn off.

### 5. Orphans — crash-safety for work, not just data
When a thread's lease stops heart-beating, the thread becomes an **orphan**:
last stitch seconds old, goal and criteria attached, worktree preserved.
Orphans appear in every holder's queue (and, after sync, on every machine)
as adoptable work parcels. Adoption hands over the same lease — goal,
criteria, scope intact — and the worktree; an orphan arriving from another
machine gets a fresh worktree with its last stitch materialized. Abrupt
death is a normal, recoverable state of work.

### 6. The git bridge — meet every developer where they live
The fabric exports to plain git at a per-repo granularity (`bridge_mode`):
`squash` (default) — one local commit per landed weave, message composed
from goal + criteria + verify result; `stitches` — the thread's checkpoint
chain replays as commits on a `heddle/<thread>-<goal>` branch (temp-index
plumbing, never the working tree), merged with the weave message; `both` —
squash plus the branch kept unmerged. Never a push, in any mode. Humans
keep GitHub, `git log`, bisect, blame; agents keep stitches, leases, and
sync. `heddle export` writes an UNLANDED thread's chain to the same branch
for human review of in-flight work.

## Exact merge semantics (isolated threads)

Definitions, per repo-relative file `f` in the thread's head stitch:

- `base(f)` — hash in the thread's base stitch (the fabric snapshot taken at
  worktree creation, refreshed by `rebase`). Absence = "did not exist".
- `head(f)` — hash in the head stitch; the sentinel `deleted` (a tombstone)
  records a deletion. Deletions are detected against the previous stitch and
  the base; a first stitch with neither reference cannot see them.
- `cur(f)` — hash of the file in the live repo tree right now; absence reads
  as the tombstone.

**Landing** (after green verify + consent, and only while the weave's
`fabric_parent` is still the fabric tip):

| condition | action |
|---|---|
| `head(f) == base(f)` | skip — the thread didn't change it; the fabric's current version stays, whatever it is |
| `head(f) != base(f)` and `cur(f) == head(f)` | skip — the fabric already agrees |
| `head(f) != base(f)` and `cur(f) == base(f)` | apply `head(f)` (write, or delete on tombstone) |
| `head(f) != base(f)` and `cur(f) != base(f)` and `cur(f) != head(f)` | **conflict** |

Any conflict refuses the whole weave — nothing is written — with the file
list in the error and on the thread's note: *"fabric moved under you on
`<files>` — rebase, then re-propose."* Edit-vs-edit, edit-vs-delete and
delete-vs-edit all land in the conflict row. In-place threads have no base;
their whole manifest applies (v0.1 semantics), which is why isolation is the
default on git repos.

**Rebase** (`heddle rebase`) walks base ∪ worktree ∪ repo per file:

- fabric-only changes (thread didn't touch `f`) **fast-forward** into the
  worktree — copies, and deletions when the fabric deleted `f`;
- thread-only changes are kept;
- changed in **both** with disagreement: the worktree **keeps the thread's
  version** and `f` is reported as a conflict — the holder is told to review
  it against the repo tree before re-proposing. Nothing merges silently; the
  informed overwrite that may follow happens behind a re-propose and a
  fresh human yes.

Then the base is re-snapshotted to the fabric's current state and the head
stitch re-captured, so the next land measures purely against the new base. A
Proposed thread returns to Active; any parked approval is handed back.

**Hygiene.** `heddle clean` removes worktrees of Woven threads only, and only
when every file in the base ∪ head manifests still matches the last capture
— uncaptured divergence refuses with the file list. Live threads and orphans
are never cleaned. (Files created in a worktree after its last stitch and
outside any manifest are undetectable here — a documented v1 limit.)

## Trust boundary

Heddle automates coordination, never consent. A stitch **only reads**. The
weave gate verifies **in a scratch copy**. Applying a green weave to the
real tree is an *action*: it happens only past an explicit human yes,
expressed through the `WeaveConsent` trait — the standalone binary asks y/N
at the terminal and refuses when stdin is not a terminal; the MCP server
always refuses (its stdin is the protocol channel; no human is at it); an
embedding host implements consent over its own approvals queue. "Auto-weave
on green" would be an explicit, revocable grant, off by default, and is not
implemented here. Cross-machine: `heddle sync` shares metadata and scoped
file blobs with the configured remote — the same exposure as pushing a
branch there — and runs only when invoked (`--auto` is per-repo opt-in).
Adoption claims decide races; they never move a live holder's work.

## Multi-machine sync (shipped)

Any git remote both machines can push to is the whole infrastructure. All
heddle traffic rides hidden refs — never branches, tags, or checkouts:

```text
refs/heddle/<machine-id>/state    published state: commit tree of
                                  state.json        threads/leases/stitches
                                  objects/<sha256>  scoped file blobs
refs/heddle/fabric                THE shared fabric: fabric.json = ordered
                                landed-weave entries, each carrying its
                                apply manifest; blobs in objects/
refs/heddle/claims/<thread-id>    orphan-adoption claims
refs/heddle/<machine-id>/mail/*   opaque mailbox payloads
```

**Sync pass order:** (1) fetch `refs/heddle/*` (pruned); (2) reconcile the
fabric — if the shared list strictly extends the local one, materialize the
new entries' blobs and replay their apply manifests onto the local tree; if
the local list strictly extends the shared one, publish the missing entries
behind the CAS; equal = done; anything else = an honest "diverged" error
that refuses to guess; (3) publish this machine's state ref (plain forced
update of its own namespace); (4) refresh the peer view — peers' threads and
leases are cached read-only for `status`, cross-machine toe-steps computed
against local live leases, adoptable orphans listed.

**Fabric authority is a compare-and-swap ref push:** `git push
--force-with-lease=refs/heddle/fabric:<sha-this-machine-last-fetched>`. The
push succeeds only if the remote still has that value — git's atomic ref
update IS the shuttle token. A lost race degrades into the same honest flow
as a local collision: fetch, "fabric moved", rebase, re-propose. **Claims**
use the same primitive with expected-value "absent": the earliest push wins
deterministically and the loser is told who won.

**The mailbox** carries `kind` + opaque bytes that Heddle never interprets or
verifies — sign payloads yourself if you need authenticity. It exists so
higher layers (an embedding host's team envelopes, consent dials) can ride
the same remote without teaching this crate their formats.

**Posture:** machine ids are identity, not authentication. Anyone who can
push to the remote can write any heddle ref — the trust model is exactly "who
you give push access to", the same as branches. Signatures are the first
federation work item below.

## Federation (design — the many-peers rung)

The shipped sync assumes one blessed remote. The gossip design removes it:

1. **Signatures first.** Each machine key-signs every object it authors
   (`sig` field, serde-defaulted — no format break). Peers verify on fetch;
   unsigned/invalid objects are quarantined, named, and never applied.
2. **Object sync order** per peer pair: exchange log heads → fetch missing
   stitches/leases/threads (content-addressed, so order within a kind is
   free) → fabric entries last, applied only in fabric order. Everything is
   append-only and idempotent; re-fetch is always safe.
3. **Shuttle rotation** replaces the CAS ref when there is no single remote:
   the right to advance the fabric for epoch `E` belongs to the live peer
   minimizing `hash(E ‖ peer-key)`; a silent shuttle-holder is skipped after
   TTL, so fabric advancement survives any machine's death. CAS-on-a-remote
   is the degenerate single-shuttle case of the same rule — which is why the
   shipped flow already teaches the right habits.
4. **Adoption across gossip** reuses claims, now signed and timestamped;
   ties break on the same hash rule.

None of this changes the object model — every object already carries a
stable id and serializes cleanly. That is the test by which the design
stays honest: federation must be additive.

## Objects

| object | fields |
|---|---|
| `Lease` | id, thread, scope[], goal, criteria[], holder, ttl, heartbeat |
| `Stitch` | id, thread, parent, files{path → sha256 \| tombstone}, ts |
| `Thread` | id, goal, head stitch, base stitch, worktree, lease, status: active · proposed · woven · orphaned · adopted, note, approval |
| `Weave` | id, thread, fabric-parent, verify {cmd, result, log-tail}, applied{…} |
| `Fabric` | repo id, tip weave, ordered history |

Storage is boring on purpose: JSON state + append-only JSONL events +
content-addressed whole-file blobs under `~/.heddle` (override `HEDDLE_DATA`),
0o600, bounded, corrupt-line tolerant.

## Capture rules

- Built-in excludes: `.git`, `target`, `node_modules` — any entry type
  (`.git` is a *file* inside a worktree). Never overridable.
- `.heddleignore` at the repo root extends them: one pattern per line in
  Heddle's glob grammar (`**`, `*`, `?`; a fully-literal line ignores that
  path and everything under it), `#` comments. It cannot re-include
  built-ins. Applies to capture and to the gate's scratch copy.
- Files over 8 MiB are skipped and named in the stitch outcome
  (`HEDDLE_MAX_FILE_MB` adjusts the cap, clamped 1–1024). Heddle snapshots
  source, not artifacts.
- Symlinks are never followed. Manifest paths are re-validated on every
  apply (relative, no `..`) — stored data never picks filesystem paths.

## What exists in this crate today

- The engine (`lib.rs`): leases/toe-steps, worktree isolation + base
  tracking, tombstoned stitches, the weave gate (scratch copy, hard
  timeout), file-level merge + conflict refusal at land, rebase, orphan/
  adopt (worktree handover + materialization), clean, consent trait
  (terminal / auto-deny / embeddable), git bridge (local commits only).
- `sync.rs`: the multi-machine layer above — state refs, CAS fabric,
  claims, mailbox — implemented over the repo's own `git` binary; the
  engine never shells git itself.
- `savings.rs`: honest value accounting — counted facts from the event log
  (toe-steps, same-file concurrent edits absorbed by isolation
  (`OverlapEdit`, detected at stitch time, deduplicated per thread pair),
  refused lands, rebases) and ONE labeled estimate whose constants are
  measured locally or printed as stated assumptions; `--record-tokens`
  attaches real harness numbers that replace the assumption. Warnings are
  never monetized, and "nothing measurable was prevented" is a first-class
  answer.
- `enroll.rs`: zero-config agent adoption — `heddle init` / thread-less
  `heddle adopt` write `.mcp.json` (MCP server), `.claude/settings.json`
  (approval + `SessionStart` hook running `heddle status --brief`) and a
  marker-fenced `CLAUDE.md` section. Order-preserving merges, user keys
  always win, unparseable files refused, everything shown before writing,
  `--dry-run` touches nothing.
- `heddle` CLI: `init · config · lease · stitch · propose · export · rebase ·
  withdraw · adopt · clean · sync · savings · status · log · mcp`, with
  `--lease/--thread` overrides for multi-seat terminals. Bare (flag-less)
  verbs resolve to the thread whose worktree contains the cwd, else the
  repo's only live thread; with several live threads they refuse with the
  list — never a guess, because a guess writes onto another agent's thread
  (the shared solo pointer is a display convenience, not targeting truth).
- `heddle mcp`: stdio MCP server — `heddle_status · heddle_lease · heddle_stitch ·
  heddle_propose · heddle_rebase · heddle_adopt`. Proposing verifies; landing
  always requires the human path.
- **Not yet**, honestly: signatures; gossip without a blessed remote;
  rolling-hash chunking (whole-file snapshots dedup by sha256); per-slice
  test impact (verify is whole-repo); gitignore parsing beyond
  `.heddleignore`.
