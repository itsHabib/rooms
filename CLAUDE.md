# rooms

Notes for agents working on this repo. Read before touching code.

`rooms` is the substrate that spawns disposable Firecracker microVMs with specified deps, runs a command in them, and collects artifacts. First consumer is an LLM agent (`rooms exec <id> -- claude -p < task.md`); other consumers (ship's `RoomCursorRunner`, `/work-driver` crash recovery, future replay) compose the primitive without it knowing about them.

## State

**Shipped (POC):**

- `rooms run --image <ext4> [--command <cmd>|--keep]` — boot microVM, optional SSH exec, shutdown.
- Firecracker control, TAP networking, SSH transport, guest CRNG seeding, Ctrl-C cleanup.
- CI: fmt + clippy + unit tests on `ubuntu-latest`; Claude review workflow wired.
- Host bootstrap scripts under `scripts/`.

**In flight:**

- POC upper bar: `rooms run --repo <path> --task <task.md>` → repo in guest + `claude -p` + `result.patch` on host.
- Primitive verbs (`create`, `exec`, `collect`, `destroy`) and real `doctor`.

**Next (productionization — see driver manifest):**

- [`docs/features/01-productionization/driver.md`](docs/features/01-productionization/driver.md) — 8 specs in 3 batches.
- Key paths: harden firecracker control, runner contract + `docs/runner-contract.md`, rootfs builder, cursor SDK runner, Nix flake input + `docs/flakes.md`, ship backend (in ship repo).

**Spec (single source of truth for v0):** [`docs/features/rooms-v0/spec.md`](docs/features/rooms-v0/spec.md).

<!-- BEGIN dev-workbench (managed by /dev-workbench skill — re-run to refresh; hand-edits inside this block will be overwritten) -->
## Dev workbench

Several MCP servers + skills are available in any Claude session on this machine. Same shape across every repo in the portfolio — refresh with `/dev-workbench` when the canonical set evolves.

### dossier — project memory plane

Markdown-on-disk corpus + Rust MCP server. Owns state across sessions: tasks, decisions, cross-repo links, session handoffs. Source of truth for "what's the current goal and what's been tried."

**Use proactively for:**

- *"start a session on rooms"* → `mcp__dossier__project_get` + `mcp__dossier__task_list`
- *"this just landed"* → `mcp__dossier__task_complete` with merge SHA in the artifact link
- *"what's left in productionization"* → `mcp__dossier__phase_list` / `task_list --phase`
- *"where did we decide X"* → `mcp__dossier__search` across decisions and task bodies

**Don't use for:**

- Ephemeral within-session todos (the harness's task tool covers those)
- GitHub-issue-style external collaboration — follow-ups live in in-repo status docs, not `gh issue create`

### ship — workflow execution

TypeScript MCP server that hands a spec doc to cursor (local or cloud), persists the run, lets you inspect/cancel/replay. The agent runner you fire when a spec is ready to implement.

**Use proactively for:**

- *"implement this spec"* → `mcp__ship__ship` with the spec path + runtime
- *"fire batch 2 in parallel"* → `mcp__ship__ship` with N stream entries
- *"is that cursor run still going"* → `mcp__ship__get_workflow_run`
- *"cancel that"* → `mcp__ship__cancel_workflow_run`

**Cloud defaults for this repo** (canonical, locked 2026-05-25):

```js
cloud: { repos: [{ name: "rooms" }], env: { type: "cloud" }, autoCreatePR: true }
```

Prefer local runtime when the PR needs heavy framing (architecture pivots, discovery logs).

**Don't use for:**

- Single ad-hoc commands (`make check`, a one-shot test) — just shell them
- Work without a written spec doc — write the spec first, then ship

### huddle — multi-seat coordination

Per-seat keys + Slack channels for when more than one operator/agent is in the loop. Synchronizes context + handoff notes across machines.

**Use proactively for:**

- *"pairing on this with codex"* → `/huddle` at session start
- *"resume from the laptop"* → `mcp__huddle__huddle_read` for current state

**Don't use for:**

- Solo single-machine sessions (overhead without payoff)

### playwright — browser automation

Playwright MCP plugin. Snapshot + act on DOM. For tasks that need a real browser, not curl.

**Use proactively for:**

- *"verify the OAuth flow"* → `mcp__plugin_playwright_playwright__browser_navigate` + friends
- *"screenshot the deployed page"* → `mcp__plugin_playwright_playwright__browser_take_screenshot`

**Don't use for:**

- Anything reachable via plain HTTP / curl
- rooms substrate work in v0 (no web surface yet)

### `/work-driver` — drive impl end-to-end

Orchestrates spec-doc tasks from a driver manifest: pre-flight worktrees, fan out via `mcp__ship__ship` (single stream or N parallel; mixed-runtime batches admitted), poll terminal states, verify auto-commit (local) or trust cloud's terminal status (cloud), open PRs, coordinate reviews, merge in dep order.

**Triggers:** "drive batch 1", "ship these specs in parallel", "run the productionization", explicit `/work-driver`.

**This repo's manifest:** [`docs/features/01-productionization/driver.md`](docs/features/01-productionization/driver.md).

```sh
/work-driver docs/features/01-productionization/driver.md           # whole manifest
/work-driver docs/features/01-productionization/driver.md --batch 1 # one batch
```

**Pair with:** `/work-driver-prep` to generate the manifest beforehand; `/shipped` for the post-run recap.

### `/work-driver-prep` — build the spec docs + manifest

Resolves dossier task IDs or a phase slug, generates one `docs/features/<slug>/spec.md` per task, detects file-overlap conflicts, groups into parallel-safe batches, emits a ready `/work-driver` command.

**Triggers:** "prep the productionization batch", "spec out these tasks for parallel ship", explicit `/work-driver-prep`.

**Pair with:** `/work-driver` — prep is the planning seam, driver is the execution seam.

### `/shipped` — retrospective recap after work lands

Pulls work-driver manifests (ground truth) + git/gh/dossier signals: PRs merged with shas + weighted-LOC, dossier task closures, chips filed, friction-log delta, what's still open, what to do next.

**Triggers:** "what just shipped", "what merged today", "post-run summary", explicit `/shipped`.

**Pair with:** Use after `/work-driver` exits, after a chip blitz, or any time you need a punch-list of "what landed and what's next." Distinct from `/status` — `/shipped` is retrospective on landed work, `/status` is in-flight.

### `/status` — tight 4-section in-flight ping

Produces a 4-section update: What happened / What's next / What I recommend / What I need from you. 1-3 sentences each. Skip-when-empty rather than padding.

**Triggers:** "give me an update", "sitrep", "where are we", "recap", explicit `/status`.

**Pair with:** `/shipped` for the retrospective. `/status` is mid-session; `/shipped` is post-landing.

### `/worktree-*` — manage secondary git worktrees

Thin skill family over plain `git worktree`. Use these instead of an MCP — the verbs that mattered (add, list, remove, transfer, where) cover the common cases without an external state store. Convention in this repo: feature branches use `prod-<slug>` prefix; worktrees live at `.claude/worktrees/<branch>/`.

- **`/worktree-add`** — *"spin up a worktree for cursor-sdk-runner"* → creates `.claude/worktrees/prod-cursor-sdk-runner/`
- **`/worktree-list`** — *"what worktrees do I have"* → branch, dirty state, optional PR/CI from `gh`
- **`/worktree-remove`** — *"clean up the worktree"* → dirty-state aware (commit-WIP / stash / discard)
- **`/worktree-transfer`** — *"bring this work over to main"* → removes secondary, checks out branch in root
- **`/worktree-where`** — *"where am I"* → which worktree, branch, and cwd this session is pointing at

### The loop

```
┌─────────────┐   ┌──────────────────┐   ┌─────────────────┐
│  dossier    │──▶│ /work-driver-    │──▶│ /worktree-add   │
│  (memory)   │   │     prep         │   │  + ship.ship    │
└──────┬──────┘   └──────────────────┘   └────────┬────────┘
       ▲                                          │
       │          ┌──────────────────┐            ▼
       │          │  make check      │   ┌─────────────────┐
       │          │  + review        │◀──│   implement     │
       │          │  (@codex,        │   │   (branch)      │
       │          │   @claude,       │   └─────────────────┘
       │          │   Copilot)       │            │
       │          └────────┬─────────┘            │
       │                   │                      ▼
       │                   ▼              ┌─────────────────┐
       └──────[ /shipped recap ]──────────│  merge + PR     │
                                          │  + worktree-rm  │
                                          └─────────────────┘
```

1. Task lands in dossier → `/work-driver-prep` writes spec(s) + manifest.
2. `/worktree-add` per stream → `mcp__ship__ship` fires the agent.
3. Agent implements on a feature branch; `make check` locally before push.
4. PR opened; Copilot + `@codex review` + `@claude review`. CI green before merge.
5. Merge; `/worktree-remove` (or `/worktree-transfer` back to root); dossier task closed with artifact link.
6. `/shipped` post-landing or `/status` mid-stream as the chain runs.

### Why this shape

Each layer swappable, seams deliberate. dossier could be Linear; ship could be a different agent runner; `/worktree-*` could be hand-rolled `git worktree` calls; playwright is one MCP among many that might serve a given browser task. The canonical set is what worked in this portfolio's actual day-to-day — substituting one tier doesn't ripple into the others, which is the value of the shape.
<!-- END dev-workbench -->

## Architecture

Strict layered dependency direction (mirrored from dossier / tower):

```
domain → firecracker / rootfs / transport → runner → main
```

- **`domain`** — plain types (`RoomId`, outcomes); no I/O.
- **`firecracker`** — process spawn, API socket, VM config, boot/shutdown.
- **`rootfs`** — overlay/CoW, image paths (flake input lands here in v0.1).
- **`transport`** — repo bundle, scp into guest.
- **`runner`** — SSH exec, artifact capture, guest readiness.
- **`main`** — clap CLI, wires layers.

Don't introduce a downward import. If a feature needs a new dependency direction, lift the shared concern into `domain`.

Host layout (v0): Windows → Hyper-V → Ubuntu `rooms-host` → Firecracker microVM per room. The `rooms` binary runs on the Ubuntu host, not on Windows.

## Docs

| Doc | Purpose |
| --- | --- |
| [`docs/vision.md`](docs/vision.md) | What/why/non-goals/roadmap — operator-facing |
| [`docs/features/rooms-v0/spec.md`](docs/features/rooms-v0/spec.md) | v0 contract — read first |
| [`docs/features/<slug>/spec.md`](docs/features/) | One spec per productionization task |
| [`docs/features/01-productionization/driver.md`](docs/features/01-productionization/driver.md) | Batched work-driver manifest |
| [`docs/features/runner-contract/spec.md`](docs/features/runner-contract/spec.md) | Runner artifact schema (operator doc: `docs/runner-contract.md`, task #3) |
| [`docs/features/nix-flake-input/spec.md`](docs/features/nix-flake-input/spec.md) | Nix deps spec (operator doc: `docs/flakes.md`, task #7) |

## Develop

```sh
make check        # fmt-check + clippy --all-targets -- -D warnings + test
make fmt          # apply rustfmt
make lint         # clippy strict (no fix)
make test         # unit tests only
make build        # debug build
make release      # release build
```

`make check` is the single command CI runs and you run before push. E2e tests (`cargo test --features e2e`) require Firecracker + KVM on the rooms-host; CI intentionally skips them.

Guest rootfs images are built with `scripts/build-rootfs.sh` on the rooms-host (see `scripts/README.md`); artifacts under `images/` are gitignored.

### Lint discipline

Mirrors dossier's:

- `clippy::all`, `pedantic`, `nursery`, `cargo` all warn-by-default
- Selective restriction: no `panic!`, `unwrap`, `indexing_slicing`, `dbg!`, `print_stdout`, `todo!`, `unimplemented!` in non-test code
- `unsafe_code = forbid`; `unreachable_pub`, `unused_lifetimes`, `unused_qualifications`, `non_ascii_idents` warn
- Complexity caps in `clippy.toml`: cognitive 20, lines 100, args 6
- CI fails on any warning (`-D warnings`)

Don't add `#[allow(...)]` without a one-line justification comment.

### PR sizing

| Band | Limit (weighted LOC) |
| --- | --- |
| amazing | < 500 |
| ideal | < 700 |
| stretch | < 1000 |

Weights: production source **1.0×**, tests + fixtures **0.5×**, lockfiles/configs/docs **0×**.

### Reviewers

Per PR: Copilot, comment `@codex review`, comment `@claude review`. CI green before merge.

## How rooms fits

- **ship (v0.1):** `backend: "rooms"` in `mcp__ship__ship`; `RoomCursorRunner` in ship's `packages/cursor-runner` calls `rooms run` / primitives. Rooms repo does not import ship.
- **work-driver:** productionization batches for this repo; future crash recovery reuses room lifecycle instead of ad-hoc cleanup.
- **dossier:** tracks tasks, links spec docs to dossier task IDs in headers.

Rooms is substrate; consumers compose it. Don't bake agent or ship concepts into `src/`.

## Conventions

- **Errors-not-capitalized** (Go convention). `bail!("no /dev/kvm; nested virt off?")` not `bail!("No /dev/kvm...")`.
- **No design-doc or phase refs in code comments** — doc comments describe behavior; roadmap context belongs in commit messages and spec docs.
- **POC scope: anyhow is fine.** Structured errors (`FirecrackerError` enum) land in task #2 `harden-firecracker-control`.
- **Atomic writes for artifact files** — write to a temp path in the room work dir, then rename. Partial files on crash are worse than no file. (Full artifact layout lands with runner-contract, task #3.)

## Shipping features

Adapted from ship's workflow:

1. Spec doc under `docs/features/<feature>/spec.md` — what + why + acceptance + scope.
2. Branch (e.g. `prod-runner-contract`).
3. Implement.
4. PR with reviewers above.
5. CI green (`make check`).
6. Address review comments; repeat ~3× before merge.

## Common gotchas

- **`/dev/kvm` missing** — nested virtualization off on the Hyper-V VM. Fix: `Set-VMProcessor -ExposeVirtualizationExtensions $true`, reboot guest, verify `ls /dev/kvm`.
- **SSH key mismatches** — rooms expects `~/.ssh/id_rooms` (from `bake-rootfs-ssh.sh`). Use `ssh -i ~/.ssh/id_rooms ...`; don't assume default `id_ed25519`. Running under `sudo` breaks `HOME` — bake script refuses sudo for this reason.
- **Rootfs size vs ext4 cap** — the quickstart image has a fixed size; stuffing too much into the rootfs during bake fails at copy time. Rootfs builder (task #6) makes this repeatable.
- **`--keep` and `--command` are mutually exclusive** — clap enforces this at parse time.
- **Guest host keys change every boot** — use `StrictHostKeyChecking=accept-new` or `/dev/null` known_hosts until rootfs builder stabilizes host keys.
- **Cloud Agent VMs** — no KVM/Firecracker here; `make check` (unit tests) only. E2e is rooms-host only.

## When you're stuck

- **Behavior contract unclear** → read the spec doc for that task (`docs/features/<slug>/spec.md`), then v0 spec.
- **Clippy warning** → `make lint` locally, read the suggestion.
- **"I want the runner to know about the agent"** → stop. Runner contract is substrate-side; agent logic lives in the rootfs runner script ([`docs/features/cursor-sdk-runner/spec.md`](docs/features/cursor-sdk-runner/spec.md)).
- **"Add Docker / devcontainer / web preview"** → stop. Non-goals. Re-read [`docs/vision.md`](docs/vision.md).
- **Firecracker misbehaving** → check per-room log under work dir, serial output, `tap-fc0` exists (`scripts/setup-tap.sh`).

## Cloud runtime defaults

When firing `mcp__ship__ship` with `runtime: "cloud"` against this repo, use:

```js
cloud: {
  repos: [{ name: "rooms" }],
  env: { type: "cloud" },
  autoCreatePR: true,
}
```

`autoCreatePR: true` means ship writes the PR body. Prefer local runtime when the PR needs heavy framing.
