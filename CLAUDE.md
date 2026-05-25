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

## Dev workbench

Portfolio tools and skills available when working in this repo. Same shape across dossier, ship, and rooms — regenerate this block with `/dev-workbench` when it drifts.

### dossier

Project memory — the primary context store for the portfolio.

- **What:** tasks, decisions, cross-repo links, session handoffs.
- **When:** start of any non-trivial session; before writing a spec; after merging to record outcomes.
- **Where:** `pers/dossier` (separate repo).
- **Invoke:** open dossier in Cursor; reference task IDs from spec doc headers (`Related: dossier task ...`).

### ship

Workflow execution — fires agents against repos with structured task docs.

- **What:** `mcp__ship__ship` MCP tool; local and cloud runtimes; auto-PR; reviewer requests.
- **When:** implementing a spec doc end-to-end; cloud agents for parallel-safe tasks.
- **Where:** `pers/ship`.
- **Invoke:** ship skill or MCP from a repo with a task doc. For rooms substrate work in cloud:

```js
cloud: {
  repos: [{ name: "rooms" }],
  env: { type: "cloud" },
  autoCreatePR: true,
}
```

Prefer local runtime when the PR needs heavy framing (architecture pivots, discovery logs).

### huddle

Multi-seat coordination — shared session state when more than one operator or agent is in the loop.

- **What:** synchronized context, handoff notes, who's driving.
- **When:** pairing sessions; operator + agent on different machines; long-running POC spikes.
- **Where:** huddle skill / MCP (portfolio-wide).
- **Invoke:** `/huddle` at session start when coordination matters; `/huddle leave` at end.

### playwright

Browser automation — for tasks that need a real browser, not curl.

- **What:** Playwright MCP / skill; snapshot + act on DOM.
- **When:** web UI verification, OAuth flows, anything SSH + curl can't reach.
- **Where:** portfolio playwright skill.
- **Invoke:** playwright MCP tools from agent context. Not used for rooms substrate work in v0.

### `/work-driver`

Orchestration — fans out spec-doc tasks from a driver manifest.

- **What:** reads `docs/features/<phase>/driver.md`, creates branches/worktrees, dispatches agents per stream, updates status.
- **When:** running a productionization batch (e.g. rooms batch 1 after POC is green).
- **Invoke:**

```sh
/work-driver docs/features/01-productionization/driver.md
/work-driver docs/features/01-productionization/driver.md --batch 1
```

Rooms productionization manifest: [`docs/features/01-productionization/driver.md`](docs/features/01-productionization/driver.md).

### `/work-driver-prep`

Planning — turns a task table into spec docs + a driver manifest.

- **What:** emits one `docs/features/<slug>/spec.md` per row and the batched `driver.md`.
- **When:** POC is done; you have a table of parallel-safe tasks and need specs before `/work-driver`.
- **Invoke:** `/work-driver-prep` with the phase context (already done for rooms productionization).

### `/worktree-*`

Worktree management — isolated git worktrees per task/stream.

- **What:** create, list, switch, cleanup worktrees without stashing on main.
- **When:** parallel local streams; matching branch-per-task convention (`prod-<slug>`).
- **Invoke:** `/worktree-create`, `/worktree-list`, `/worktree-remove` (portfolio worktree skills). Replaces deprecated tower worktree tracking.

### The loop

```
┌─────────────┐     ┌──────────────┐     ┌─────────────┐
│  dossier    │────▶│ spec doc     │────▶│ ship /      │
│  (memory)   │     │ features/…   │     │ work-driver │
└─────────────┘     └──────────────┘     └──────┬──────┘
       ▲                                        │
       │              ┌──────────────┐          ▼
       │              │ make check   │     ┌─────────────┐
       └──────────────│ + review     │◀────│ implement   │
                      │ (@codex,     │     │ (branch)    │
                      │  @claude)    │     └─────────────┘
                      └──────────────┘
```

1. Task lands in dossier → spec doc written (or `/work-driver-prep`).
2. `/work-driver` or ship fires an agent against the spec.
3. Agent implements on a feature branch; runs `make check`.
4. PR opened; Copilot + `@codex review` + `@claude review`.
5. Merge; update dossier / driver manifest status.

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
