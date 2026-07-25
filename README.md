# Loom

*Version control for many hands moving at once.*

Git is the filesystem; Loom is air traffic control.

## Two timelines

**Without Loom.** You point two Claude Code sessions at your repo. Agent A
spends 40 minutes and ~200k tokens rewriting `src/auth/` around a new session
model. Agent B, told to "clean up the login flow", touches the same files with
the opposite assumption. Neither knows the other exists — git has nowhere to
put that knowledge. B finishes last, so B's edits win; A's work is silently
gone from the tree. The test suite goes red at commit time, and you spend the
evening (and another few hundred thousand tokens) having a third session
diagnose a breakage that is really two AIs' half-rewrites interleaved. The
most expensive thing agents do is semantically reconcile two overlapping
rewrites after the fact — and this workflow makes it routine.

**With Loom.** A leases `src/auth/**` with the goal "move auth to the new
session model". When B's session asks Loom what's being worked on — one MCP
call — it sees that lease, goal attached, and self-partitions: it either takes
the suggested disjoint scope or waits. The collision is reported at the
*start*, when handling it costs one tool call, not at the *end*, after both
budgets are spent. If both proceed anyway, each works in its own worktree,
nothing is overwritten, and the second landing is told exactly which files
moved and rebases once — the expensive reconciliation becomes rare instead of
fast.

That is the entire bet: **git reports collisions after the tokens are spent;
Loom reports them before.**

## The problem

You run several coding agents on one repo. They overwrite each other's files,
land red on top of red, and die mid-task leaving dirty worktrees nobody owns.
You can't see who is doing what, so every collision is discovered at the end
— at merge or in CI — when the only fix is expensive re-work. Git has no
concept of *intent*: a branch name is a string, so agents can't ask "who is
already working on this?" and route around each other.

## What Loom does

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

## What it's not for

- **N agents racing the SAME task.** Loom coordinates *different* tasks on
  one repo. For redundant attempts at one task, use a judge/tournament
  harness — then lease the winner's landing.
- **Replacing git.** Loom sits on top; git stays your history, remotes, and
  review tooling. The optional bridge turns each landed weave into one local
  commit (never a push).
- **Humans doing normal PRs.** Slow, coordinated-out-of-band work is what
  git+GitHub already does well.

## Two-minute quickstart

The disaster being prevented, on your own machine. One repo, two terminals
("agents" A and B):

```bash
cargo install --path .        # one binary: loom
cd your-repo
loom init --verify "cargo check"     # or any command that exits 0 on green
```

**A:**

```bash
loom lease "greet in french" 'greeting.txt'
#   WORK IN: ~/.loom/<repo>/worktrees/<thread-A>     ← A's own worktree
```

**B, at the same time:**

```bash
loom lease "greet louder" 'greeting.txt'
#   TOE-STEP: your 'greet louder' overlaps 'greet in french'
#   (a lease warns, it never blocks — coordinate or continue)
#   WORK IN: ~/.loom/<repo>/worktrees/<thread-B>     ← a DIFFERENT worktree
```

Both edit the same file — in their own worktrees, so nothing clobbers. A
finishes first:

```bash
# A (use --lease/--thread ids from the lease output when sharing a terminal):
loom stitch          # checkpoint (content-addressed; deletions tracked)
loom propose         # verify runs in a scratch copy; on green it asks y/N
# → woven: 1 files applied
```

B proposes next, and gets the truth instead of a silent overwrite:

```bash
loom stitch && loom propose
# → verify green … but landing refuses:
#   fabric moved under you on greeting.txt — rebase the thread and re-propose
loom rebase
#   CONFLICTS — both the fabric and this thread changed:
#     greeting.txt (your version kept in the worktree; the fabric's is in the repo tree)
# reconcile the file in B's worktree, then:
loom stitch && loom propose
# → woven — B's landing includes A's work instead of erasing it
```

Kill a session mid-task (or let its lease TTL lapse) and `loom status` shows
the thread as an **orphan** — `loom adopt <thread-id>` hands the next agent
its goal, criteria, and worktree, checkpoint intact.

## Agents (Claude Code or any MCP client)

```bash
claude mcp add loom -- loom mcp
```

The tool flow an agent follows:

1. `loom_status` — who's working on what (threads, live leases, orphans).
2. `loom_lease` — declare goal + scope. The response's `working_dir` is the
   thread's worktree: **cd there and make all edits in it.** Toe-step
   warnings come back in the same response — the moment to renegotiate scope.
3. work → `loom_stitch` every few edits (checkpoints + heartbeats the lease).
4. `loom_propose` — runs the verify in a scratch copy and reports green/red.
   The apply is always refused over MCP: landing takes a human at a terminal
   (`loom propose`) or a host with an approvals queue. Proposing verifies;
   it never lands.
5. On "fabric moved": `loom_rebase`, reconcile any conflicts in the worktree,
   stitch, re-propose. `loom_adopt` picks up orphans (local or a synced
   peer's).

## Work with a friend in 5 minutes

Two people, one private GitHub repo. Loom syncs through it over hidden
`refs/loom/*` refs — no server, no daemon, nothing new to host.

```bash
# Both of you, once, in your own clone:
loom init --verify "npm test"        # whatever green means for the project
loom sync --remote origin            # remembers the remote for this repo
```

Then work exactly as in the quickstart — lease, edit in your worktree,
stitch, propose — and run `loom sync` whenever you want to exchange state
(or opt in to `loom sync --auto` to sync after every stitch/propose):

```bash
you>    loom lease "dark mode styles" 'style.css'      # + edit, stitch
friend> loom lease "compact header" 'style.css'        # + edit, stitch
you>    loom propose && loom sync                      # lands, publishes
friend> loom sync        # pulls your weave into their tree
friend> loom propose     # green — but landing says: fabric moved under you
friend> loom rebase      # your change fast-forwards in; real conflicts kept
friend> loom stitch && loom propose && loom sync       # merged result lands
you>    loom sync        # both trees now identical, both changes present
```

Zero merge fear in two sentences: neither of you can overwrite the other,
because edits live in per-thread worktrees and the shared line only advances
through a compare-and-swap ref push that refuses when it moved under you.
The worst case is not a broken tree — it is being told, by name of file,
what to look at before your work lands.

What sync shares, plainly: your loom metadata (goals, scopes, holders) and
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

Loom's answer, point by point:

- The overlap was knowable at lease time; git had nowhere to record it. Loom
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
  `LOOM_MAX_FILE_MB` if you must; prefer a `.loomignore` at the repo root
  (gitignore-lite: literal dirs and simple globs, one per line) — it extends
  the built-in `.git`/`target`/`node_modules` excludes and cannot un-ignore
  them.
- **`loom clean`** removes worktrees of woven threads — and refuses if the
  worktree holds anything not captured in a stitch.
- **Verify is whole-repo, in a scratch copy, with a hard timeout.** Red never
  lands; a gate that can't even stage its check reports red, not silence.

## Scale ladder

| rung | status |
|---|---|
| Several agents, one machine | shipped — worktree isolation, leases, green gate, orphans (39 tests) |
| A few people/machines, one shared git remote | shipped — `loom sync`: state over `refs/loom/*`, CAS fabric ref, cross-machine adoption claims; metadata is unsigned (machine ids are identity, not authentication) |
| Team knobs (consent dials, envelopes) | exists in the Aether integration, which embeds this crate; the generic mailbox namespace here is the hook it rides on |
| Many peers, no blessed remote (gossip) | design only — see [docs/DESIGN.md](docs/DESIGN.md), "Federation" |

## Comparison

| | git worktrees alone | Jujutsu | CI-gated trunk | Loom |
|---|---|---|---|---|
| Per-task isolation | yes | yes (working-copy commits) | no | yes (worktree per thread) |
| Machine-readable intent, collision warning at start | no | no | no | yes (leases + toe-steps) |
| Mainline can't go red | no | no | yes (post-hoc, in CI) | yes (verify before land, local) |
| Refuses to overwrite concurrent work | manual merge | manual resolve | last merge wins | yes (three-way refusal + rebase) |
| Crashed work is claimable with its goal attached | no | no | no | yes (orphans + adoption) |
| Agent-native interface (MCP) | no | no | no | yes |
| Multi-machine without new infra | via remotes, manual | via git remotes, manual | server | any shared git remote (`loom sync`) |
| Is your git history | yes | yes | yes | yes (bridge: one commit per weave, never pushed) |

## License

MIT — see [LICENSE](LICENSE).
