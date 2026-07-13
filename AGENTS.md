## Cursor Cloud specific instructions

### Overview

`rooms` is a Rust CLI (single crate, no workspace) that spawns disposable Firecracker microVMs. Dev workflow is pure Rust — no Docker, no external services required for build/lint/test.

### Quick reference

- **Single CI command:** `make check` (runs `fmt-check` → `lint` → `test`)
- See `Makefile` and `CLAUDE.md` for the full set of targets and coding conventions.

### Toolchain

The Cloud Agent VM ships with an older Rust pinned as the default (1.83.0 at time of writing). The update script runs `rustup default stable && rustup update stable` so the latest stable toolchain is active. If `cargo` commands fail with a "requires Rust X.Y or later" message or a missing-stable-feature error, the old default is still active — run `rustup default stable && rustc --version` to fix and confirm.

### E2E tests

`cargo test --features e2e` requires a real Firecracker binary, `/dev/kvm`, and kernel + rootfs images — none of which are available in Cloud Agent VMs. Only unit tests (`make test` / `cargo test` without `--features e2e`) run here. This is expected and not a setup failure. (The e2e test itself boots with no network, so TAP devices aren't on the requirements list — but `rooms run` with `--command` is, see `CLAUDE.md`.)

### Running the CLI

The binary is `rooms`. Both subcommands need the rooms-host stack (Firecracker + KVM + a rootfs at `--image`), so they only produce meaningful output on a properly set-up host — but they compile and parse arguments correctly in any environment. Example invocation:

```sh
cargo run -- run --image ~/rooms/images/rootfs.ext4 --command 'uname -a'
cargo run -- doctor   # stub
cargo run -- run --help   # for the full argument list
```

<!-- BEGIN dev-workbench (managed by /dev-workbench skill — re-run to refresh; hand-edits inside this block will be overwritten) -->
## Dev workbench

These MCPs, planes, and skills are available in any agent session on this machine; the harness injects each tool's signature, so this is the *map* — how they compose — not the per-verb manual. When the signal matches, call the verb; don't ask permission. Stuck on a *knowledge* question about another portfolio repo → `/consult` its steward; only *authority* questions (direction, spend, irreversible calls) go to the operator.

**MCPs (in-session):**
- **dossier** — durable project memory: projects → phases → tasks → artifacts (markdown-on-disk).
- **ship** — the driver engine: dispatch a task to a cloud/local agent and persist the run (dispatch→poll→judgment→land→record); inspect/cancel/replay.
- **huddle** — *optional* multi-seat coordination (Slack-backed); off the normal PR path.
- **playwright** — browser automation when a task needs a real DOM.

**Planes (CLIs, composed via exit codes + JSONL — not MCPs):**
- **gate** — authorization: evaluates the *exact* PR head, emits governed-path merge authorization. Findings ≠ authorization; gate is the merge boundary.
- **flare** — notification: best-effort escalation sink over authoritative receipts → its own Slack app/channel. Pure sink; never gates; not built on huddle.

**Skills:**
- **/work-driver** [+ **/work-driver-prep**] — drive agent-led impl end-to-end; prep builds the specs + conflict-batched plan.
- **/pr-risk** — size how much review a PR needs (deterministic floor + agent advisory); upstream of the reviewers — it decides *how much*, they *do* it.
- **/review-coordinator** [+ **/review-digest**] — consolidate the AI PR reviewers into one verdict (the judge over the finders); digest pre-triages the bot pile locally.
- **/shipped** · **/status** · **/wip** — retrospective recap · in-flight update · cross-store live board.
- **/consult** — summon a sibling repo's steward for a same-turn answer; knowledge → peer, authority → operator.
- **/worktree-*** — add · list · remove · transfer · where, over `git worktree`.

### The loop

```
dossier task → /worktree-add → spec → ship driver (cloud-first: dispatch→poll→judgment→land→record)
   → PR + CI → /pr-risk tiers it → reviewers fire → /review-coordinator → one verdict
   → gate evaluates the exact head → governed-path authorization → merge
   → authoritative receipts → dossier close-out → /worktree-remove
        ↘ any attention/terminal receipt → best-effort flare sweep → Slack   (independent; never gates)
```

`/work-driver` coordinates dispatch→poll→land and runs its own review triage inline. `/pr-risk` and `/review-coordinator` are steps you *invoke* — the driver→pr-risk / driver→coordinator wiring is planned, not built, so nothing here auto-delegates.

### Why this shape

Each layer owns one responsibility and is swappable without rippling: dossier owns *what needs doing*; worktree skills own *where work happens*; ship owns *drive an agent + persist the run*; pr-risk owns *how much review*; review-coordinator owns *consolidate the finders* (the bots are swappable under it); **gate owns *authorization* — is this exact head allowed to merge — which is not the reviewers' findings**; **flare owns *notification* — a best-effort sink on authoritative receipts, its own Slack app, never blocking the driver, never depending on huddle**; consult owns the stuck path; huddle owns optional multi-seat; playwright owns browser. The workbench is a menu, not a checklist — skip what a flow doesn't need.

### The shape underneath

These tools instantiate the redesign's five contract planes — coupled only by typed artifacts (`evidence → verdict → action`), never call stacks:

- **State** (remembers) — dossier + run/verdict/grant/receipt artifacts; the append-only substrate.
- **Execution** (does) — ship's driver; emits evidence, never judges itself.
- **Verification** (judges) — the escalate-only ladder (deterministic floor → local → premium), monotone `worst`/`max`: gate's reducer, review-coordinator, sense/triage/tracelens.
- **Capability** (bounds) — scoped/timed grants; every effectful verb needs a live grant + a supporting verdict.
- **Observability** (explains) — read-only, storeless views from State: flare, /wip, /shipped, /status.

This section is the sixth — **Composition**: the agent + thin policy choosing which planes a task needs. The boundaries above *are* the plane laws, not conventions.
<!-- END dev-workbench -->

<!-- BEGIN eng-philo (managed by /eng-philo — re-run to refresh; hand-edits inside this block will be overwritten) -->
## Engineering principles

How code is written here — Dave Cheney lineage ([Practical Go](https://dave.cheney.net/practical-go)): simplicity, clarity, line-of-sight. Apply on every change; the lint below catches the slips.

1. **No `else` — line-of-sight.** Handle errors / edge cases with early returns and guard clauses; keep the happy path un-indented, flowing down the left margin. Reaching for `else` → return early instead.
2. **Shallow nesting — ≤2 levels *per scope*.** A `for` + an `if` is the ceiling in one scope. The budget is per-scope, not per-function — a closure / anon fn is its own scope, so a `for`+`if` inside a closure is fine. Deeper in one scope → extract a function.
3. **Policy vs mechanism.** Separate the decisions (policy: validation, state machines, business rules) from the plumbing (mechanism: persistence, transport, I/O). Mechanism is dumb and swappable; policy lives in a layer above it. Never let policy leak into a mechanism layer.
4. **Composition of single-responsibility layers.** Each layer / package owns ~one responsibility; the app is a *composition* of them; any piece is swappable without rippling into the others. Dependencies flow one direction.
5. **Small, sharp APIs.** Export the least callers need. Intention-revealing names. Accept the narrowest input, return concrete types. Make the zero value useful.
6. **Errors are values; simplicity over cleverness.** Handle or propagate errors explicitly — never swallow. Readable > clever > short. A little copying beats a premature abstraction or dependency.

### Rust idioms + enforcement

`?` over nested `match`; early-return guards, no `else` after a `return`; newtypes for domain values; minimal surface (lean on `pub(crate)`, `unreachable_pub`).

*Enforce:* clippy `cognitive_complexity` + `too_many_lines`, `clippy.toml` complexity caps, `-D warnings`.
<!-- END eng-philo -->
