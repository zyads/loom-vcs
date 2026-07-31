# Heddle

*Version control for many hands moving at once.*

Git is the filesystem; Heddle is air traffic control.

A heddle is the part of a loom that keeps threads separated so they never
tangle. That's the product.

Formerly known as loom-vcs; renamed to avoid collision with several
unrelated Loom projects.

## Two timelines

**Without Heddle.** You point two Claude Code sessions at your repo. Agent A
spends 40 minutes and ~200k tokens rewriting `src/auth/` around a new session
model. Agent B, told to "clean up the login flow", touches the same files with
the opposite assumption. Neither knows the other exists — git has nowhere to
put that knowledge. B finishes last, so B's edits win; A's work is silently
gone from the tree. The test suite goes red at commit time, and you spend the
evening (and another few hundred thousand tokens) having a third session
diagnose a breakage that is really two AIs' half-rewrites interleaved. The
most expensive thing agents do is semantically reconcile two overlapping
rewrites after the fact — and this workflow makes it routine.

**With Heddle.** A leases `src/auth/**` with the goal "move auth to the new
session model". When B's session asks Heddle what's being worked on — one MCP
call — it sees that lease, goal attached, and self-partitions: it either takes
the suggested disjoint scope or waits. The collision is reported at the
*start*, when handling it costs one tool call, not at the *end*, after both
budgets are spent. If both proceed anyway, each works in its own worktree,
nothing is overwritten, and the second landing is told exactly which files
moved and rebases once — the expensive reconciliation becomes rare instead of
fast.

That is the entire bet: **git reports collisions after the tokens are spent;
Heddle reports them before.**

## The problem

You run several coding agents on one repo. They overwrite each other's files,
land red on top of red, and die mid-task leaving dirty worktrees nobody owns.
You can't see who is doing what, so every collision is discovered at the end
— at merge or in CI — when the only fix is expensive re-work. Git has no
concept of *intent*: a branch name is a string, so agents can't ask "who is
already working on this?" and route around each other.

## What Heddle does

- **Isolation** — every task gets its own git worktree; two threads
  *physically cannot* clobber each other's edits.
- **Coordination** — before editing, a thread declares an **intent lease**
  (machine-readable goal + file globs); overlaps warn at declaration time,
  which lets agents self-partition work over MCP with no human traffic cop.
  Leases warn, they never block — an agent is never stopped; the only gate is
  at landing.
- **Integration** — the mainline (**fabric**) advances only through a green
  verify run plus a human/consent gate, with an honest file-level merge:
  a file changed in both the fabric and the thread refuses to land ("fabric
  moved under you — rebase") instead of overwriting either side. Never-red
  main means no fleet-wide token burn diagnosing a stranger's breakage.
- **Recovery** — a dead agent's thread becomes an **orphan**: goal, acceptance
  criteria, and a seconds-old checkpoint attached, claimable by anyone. That
  is schedulable work, not a mystery dangling branch.

An honest concession: two careful humans working slowly don't need this. The
value scales with writer count and edit frequency — it exists for fleets of
agents (and humans who work like them).

Heddle is not alone in this space. Several projects have converged on
worktree isolation + gated merging — that convergence is evidence the
problem is real. What Heddle claims as its own is narrower and specific:
**leaderless, warn-only intent leases** (no dispatcher, no locks), an
**always-green fabric advanced only by compare-and-swap over bare git
remotes** (no server), **decentralized orphan adoption**, and
**stitch-level history** — a serverless, git-native *protocol* and one
binary, not a platform. See [Adjacent projects](#adjacent-projects) for the
neighbors and what each does that Heddle doesn't.

## What it's not for

- **N agents racing the SAME task.** Heddle coordinates *different* tasks on
  one repo. For redundant attempts at one task, use a judge/tournament
  harness — then lease the winner's landing.
- **Replacing git.** Heddle sits on top; git stays your history, remotes, and
  review tooling. The optional bridge projects each landed weave into local
  git history — one commit per weave by default, checkpoint-level if you ask
  (see `bridge_mode` below) — and never pushes.
- **Humans doing normal PRs.** Slow, coordinated-out-of-band work is what
  git+GitHub already does well.

## Two-minute quickstart

The disaster being prevented, on your own machine. One repo, two terminals
("agents" A and B):

```bash
cargo install --path .        # one binary: heddle
cd your-repo
heddle init --verify "cargo check"     # or any command that exits 0 on green
```

**Keep the verify fast.** It runs on *every* `heddle propose`, and every agent
waits for it before its work can land — a 49-second suite is 49 seconds of
every agent's time, every attempt. Point it at a quick subset (a few seconds:
`cargo check`, `pytest -q -m "not slow"`, `npm run test:fast`) and let the full
suite run in CI. `heddle init` times the command once and tells you if it is
slow (it skips that when stdout isn't a terminal, or with
`HEDDLE_SKIP_VERIFY_TIMING=1`).

**A:**

```bash
heddle lease "greet in french" 'greeting.txt'
#   WORK IN: ~/.heddle/<repo>/worktrees/<thread-A>     ← A's own worktree
```

**B, at the same time:**

```bash
heddle lease "greet louder" 'greeting.txt'
#   TOE-STEP: your 'greet louder' overlaps 'greet in french'
#   (a lease warns, it never blocks — coordinate or continue)
#   WORK IN: ~/.heddle/<repo>/worktrees/<thread-B>     ← a DIFFERENT worktree
```

Both edit the same file — in their own worktrees, so nothing clobbers. A
finishes first:

```bash
# A, from inside A's worktree — a bare verb targets the thread whose
# worktree you are standing in. Anywhere else, with two live threads,
# heddle refuses and lists them (pass --lease <id>); it never guesses,
# because a wrong guess would stitch onto B's thread.
cd ~/.heddle/<repo>/worktrees/<thread-A>
heddle stitch          # checkpoint (content-addressed; deletions tracked)
heddle propose         # verify runs in a scratch copy; on green it asks y/N
# → woven: 1 files applied
```

B proposes next, and gets the truth instead of a silent overwrite:

```bash
heddle stitch && heddle propose
# → verify green … but landing refuses:
#   fabric moved under you on greeting.txt — rebase the thread and re-propose
heddle rebase
#   CONFLICTS — both the fabric and this thread changed:
#     greeting.txt (your version kept in the worktree; the fabric's is in the repo tree)
# reconcile the file in B's worktree, then:
heddle stitch && heddle propose
# → woven — B's landing includes A's work instead of erasing it
```

Kill a session mid-task (or let its lease TTL lapse) and `heddle status` shows
the thread as an **orphan** — `heddle adopt <thread-id>` hands the next agent
its goal, criteria, and worktree, checkpoint intact.

## Agents (Claude Code or any MCP client)

```bash
claude mcp add heddle -- heddle mcp
```

The tool flow an agent follows:

1. `heddle_status` — who's working on what (threads, live leases, orphans).
2. `heddle_lease` — declare goal + scope. The response's `working_dir` is the
   thread's worktree: **cd there and make all edits in it.** Toe-step
   warnings come back in the same response — the moment to renegotiate scope.
3. work → `heddle_stitch` every few edits (checkpoints + heartbeats the lease).
4. `heddle_propose` — runs the verify in a scratch copy and reports green/red.
   The apply is always refused over MCP: landing takes a human at a terminal
   (`heddle propose`) or a host with an approvals queue. Proposing verifies;
   it never lands.
5. On "fabric moved": `heddle_rebase`, reconcile any conflicts in the worktree,
   stitch, re-propose. `heddle_adopt` picks up orphans (local or a synced
   peer's).

## Work with a friend in 5 minutes

Two people, one private GitHub repo. Heddle syncs through it over hidden
`refs/heddle/*` refs — no server, no daemon, nothing new to host.

```bash
# Both of you, once, in your own clone:
heddle init --verify "npm test"        # whatever green means for the project
heddle sync --remote origin            # remembers the remote for this repo
```

Then work exactly as in the quickstart — lease, edit in your worktree,
stitch, propose — and run `heddle sync` whenever you want to exchange state
(or opt in to `heddle sync --auto` to sync after every stitch/propose):

```bash
you>    heddle lease "dark mode styles" 'style.css'      # + edit, stitch
friend> heddle lease "compact header" 'style.css'        # + edit, stitch
you>    heddle propose && heddle sync                      # lands, publishes
friend> heddle sync        # pulls your weave into their tree
friend> heddle propose     # green — but landing says: fabric moved under you
friend> heddle rebase      # your change fast-forwards in; real conflicts kept
friend> heddle stitch && heddle propose && heddle sync       # merged result lands
you>    heddle sync        # both trees now identical, both changes present
```

Zero merge fear in two sentences: neither of you can overwrite the other,
because edits live in per-thread worktrees and the shared line only advances
through a compare-and-swap ref push that refuses when it moved under you.
The worst case is not a broken tree — it is being told, by name of file,
what to look at before your work lands.

What sync shares, plainly: your heddle metadata (goals, scopes, holders) and
the file content of your stitched scope go to that remote — the same
exposure as pushing a branch there. `--auto` is opt-in per repo. Dead
machines' threads show up as adoptable orphans; claims are first-push-wins
on a git ref, and the loser is told who won.

## Why not just git merge/rebase?

One concrete breakage. Agents A and B both edit `config.rs` on branches.
Merge time: git sees the same *lines* touched, declares a conflict, and hands
it to whoever merges last — a fresh agent session with no memory of either
intent, reconciling two rewrites token-by-token. Or worse: they touched
*different* lines, git auto-merges silently, and main is red with a breakage
neither author can reproduce alone. Every agent then pulls red main and burns
tokens on a failure that was manufactured by the workflow.

Heddle's answer, point by point:

- The overlap was knowable at lease time; git had nowhere to record it. Heddle
  reports it before either agent spends anything.
- Reconciliation, when it must happen, goes to the *surviving author* (who
  has the context), scoped to named files, against a green tree — not to a
  stranger at merge time against a red one.
- Rebases are made **rare**, not fast: self-partitioning at lease time means
  most collisions never happen.

## Details that keep it honest

- **Landing is consent-gated.** A green verify is evidence, not an action.
  The terminal asks y/N; MCP always refuses the apply; embedding hosts park
  an approval. Nothing reaches the repo tree without a yes.
- **Deletions are first-class.** A file you delete in your worktree is
  recorded as a tombstone, lands as a deletion, and delete-vs-edit collisions
  refuse like any other conflict.
- **Big files are skipped, loudly.** Files over 8 MiB are not snapshotted;
  each one is named in the stitch result. Raise the cap with
  `HEDDLE_MAX_FILE_MB` if you must; prefer a `.heddleignore` at the repo root
  (gitignore-lite: literal dirs and simple globs, one per line) — it extends
  the built-in `.git`/`target`/`node_modules` excludes and cannot un-ignore
  them.
- **`heddle clean`** removes worktrees of woven threads — and refuses if the
  worktree holds anything not captured in a stitch.
- **Verify is whole-repo, in a scratch copy, with a hard timeout.** Red never
  lands; a gate that can't even stage its check reports red, not silence.
- **Rename compatibility (loom-vcs → heddle).** The old `LOOM_DATA` and
  `LOOM_MAX_FILE_MB` env vars are honored as silent fallbacks when the
  `HEDDLE_*` ones are unset, and a `.loomignore` is read when no
  `.heddleignore` exists. If `~/.loom` exists and `~/.heddle` doesn't,
  Heddle keeps using `~/.loom` (with a one-line notice) — data is never
  moved silently; migrate with `mv ~/.loom ~/.heddle` when convenient.

## Git history granularity (`bridge_mode`)

The philosophy: **one lease = one goal = one commit.** Scope leases small and
the bridge's default gives you a semantic git log where every commit is a
goal that verified green. `bridge_mode` exists for teams who want
checkpoint-level history in git itself — set it at `heddle init --bridge-mode
<mode>` or later with `heddle config --bridge-mode <mode>`:

| mode | what lands in git | one-line guidance |
|---|---|---|
| `squash` (default) | one commit per weave: goal + criteria + verify | scope leases small; keep the log semantic |
| `stitches` | every checkpoint as a commit on a `heddle/<thread>-<goal>` branch, then a merge commit carrying the weave message | you want checkpoint-level `git bisect`/review without losing the semantic landing |
| `both` | the squash commit + the per-thread branch preserved, unmerged | clean mainline log, archaeology on the side |

Checkpoint replay is pure git plumbing (a temporary index; `read-tree` →
`hash-object` → `update-index` → `write-tree` → `commit-tree`) — it never
touches your working tree, real index, or current branch; empty-diff
checkpoints are skipped. Nothing is ever pushed, in any mode.

`heddle export [--thread <id>]` writes an **unlanded** thread's checkpoints to
the same per-thread branch — review an agent's in-flight work with plain
`git log -p heddle/<thread>-<goal>`; nothing lands, nothing moves.

## Scale ladder

| rung | status |
|---|---|
| Several agents, one machine | shipped — worktree isolation, leases, green gate, orphans, configurable git bridge + draft-branch export (45 tests) |
| A few people/machines, one shared git remote | shipped — `heddle sync`: state over `refs/heddle/*`, CAS fabric ref, cross-machine adoption claims; metadata is unsigned (machine ids are identity, not authentication) |
| Team knobs (consent dials, envelopes) | exists in the Aether integration, which embeds this crate; the generic mailbox namespace here is the hook it rides on |
| Many peers, no blessed remote (gossip) | design only — see [docs/DESIGN.md](docs/DESIGN.md), "Federation" |

## Comparison

Against the substrates Heddle builds on or replaces the workflow of:

| | git worktrees alone | Jujutsu | CI-gated trunk | Heddle |
|---|---|---|---|---|
| Per-task isolation | yes | yes (working-copy commits) | no | yes (worktree per thread) |
| Machine-readable intent, collision warning at start | no | no | no | yes (leases + toe-steps) |
| Mainline can't go red | no | no | yes (post-hoc, in CI) | yes (verify before land, local) |
| Refuses to overwrite concurrent work | manual merge | manual resolve | last merge wins | yes (three-way refusal + rebase) |
| Crashed work is claimable with its goal attached | no | no | no | yes (orphans + adoption) |
| Agent-native interface (MCP) | no | no | no | yes |
| Multi-machine without new infra | via remotes, manual | via git remotes, manual | server | any shared git remote (`heddle sync`) |
| Is your git history | yes | yes | yes | yes (bridge: squash, per-checkpoint, or both — never pushed) |

Against the adjacent agent-coordination projects (see
[Adjacent projects](#adjacent-projects) for links; claims below are from
their own docs as of July 2026):

| | what it is | needs a server / control plane? | coordination style | multi-machine without new infra? | always-green invariant? | crash-recovery semantics |
|---|---|---|---|---|---|---|
| aweb | platform (coordination server + identity registry) | yes — server (FastAPI/Postgres/Redis) plus `awid` identity service | mail, chat, tasks, roles, presence, **file locks**; MCP tools | yes, but through its server (team certificates) | not documented | not documented |
| batty | platform (Rust daemon driving tmux agent teams) | yes — persistent daemon | hierarchical dispatch: architects plan, managers route, engineers execute — each engineer in its own worktree | single host (daemon + tmux) | yes — daemon auto-tests completions and merges on green; no agent in the merge path | crash respawn, stall detection (all roles), auto-restart |
| stoneforge | platform (TypeScript web control plane) | yes — local server + web dashboard | dispatch daemon assigns dependency-ordered tasks; Director / Worker / Steward roles; worktree per worker | not addressed — local orchestration | merge steward runs your test command, squash-merges on pass, hands failures to a new worker | event-sourced log; session resumption not yet implemented |
| valkor-ai/loom | delivery harness (local MCP state machine for one agent) | no | single-agent plan → build → test → fix loop; not multi-agent coordination | n/a (per-machine, per-project) | review/repair loop with recorded evidence; no hard merge block | state saved under `.loom/`; resume with `/loom continue` |
| Heddle | protocol + one binary on top of git | no — state lives in your repo and any ordinary git remote | leaderless, **warn-only** intent leases; no dispatcher, no locks | yes — any shared git remote (`heddle sync`) | yes — verify in a scratch copy + consent gate; fabric advances only by CAS ref push | orphans: goal, criteria, seconds-old checkpoint attached; adoptable cross-machine, first CAS claim wins |

Read the columns honestly: the platforms above do things Heddle deliberately
does not — orchestration, role hierarchies, task boards, chat, presence,
automated dispatch. Heddle only coordinates writers and keeps the shared
line green.

## Adjacent projects

- [aweb](https://github.com/awebai/aweb) — a team-coordination platform for
  agents: mail, chat, tasks, file locks, presence, and roles behind a server
  and an identity registry.
- [batty](https://github.com/battysh/batty) — a Rust daemon that runs
  hierarchical agent teams in tmux, with a worktree per engineer and a
  verify-then-auto-merge loop that keeps agents out of the merge path.
- [stoneforge](https://github.com/stoneforge-ai/stoneforge) — a TypeScript
  web control plane with Director/Worker/Steward roles, a dispatch daemon,
  worktree isolation, and test-gated merge review.
- [valkor-ai/loom](https://github.com/valkor-ai/loom) — a delivery harness
  that keeps a single agent on track through a multi-step task (plan, build,
  test, fix) with resumable state between sessions.
- [rjwalters/loom](https://github.com/rjwalters/loom) — orchestration that
  uses your git forge as the coordination layer, driving agents through
  labels on issues and PRs.

Heddle is deliberately the thin layer: a protocol and a binary, not a
platform. If you want orchestration on top, these are good; Heddle aims to
be what they could coordinate through.

## License

MIT — see [LICENSE](LICENSE).
