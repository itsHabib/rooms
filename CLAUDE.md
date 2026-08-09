# rooms

Notes for agents working on this repo. Read before touching code.

`rooms` is the substrate that spawns disposable Firecracker microVMs with specified deps, runs a command in them, and collects artifacts. First consumer is an LLM agent (`rooms exec <id> -- claude -p < task.md`); other consumers (ship's `RoomCursorRunner`, `/work-driver` crash recovery, future replay) compose the primitive without it knowing about them.

## State

Live state — what's shipped, what's in flight, and what's next — lives in dossier (project `rooms`, phase `01-productionization`), not in this durable doc. Start a session with `mcp__dossier__project_get` + `mcp__dossier__task_list`; the dossier section of the dev workbench below has the query patterns.

**Spec (single source of truth for v0):** [`docs/features/rooms-v0/spec.md`](docs/features/rooms-v0/spec.md).

<!-- BEGIN dev-workbench (managed by /dev-workbench skill - re-run to refresh; hand-edits inside this block will be overwritten) -->
## Dev workbench

These MCPs, planes, and skills are available in Claude and Codex sessions on this machine; each harness injects tool signatures, so this is the map of how they compose, not a second verb manual. When the signal matches, call the verb. Knowledge questions about another portfolio repo go to `/consult`; authority questions - direction, spend, credentials, irreversible actions - go to the operator.

**MCPs (in-session):**
- **dossier** - durable project memory: projects -> phases -> tasks -> artifacts.
- **ship** - dispatch an agent and persist dispatch -> poll -> judgment -> land -> record.
- **huddle** - optional multi-seat coordination; off the normal PR path.
- **playwright** - browser automation when the task requires a real DOM.

**Planes (workbench CLIs composed through exit codes and JSONL, not MCPs):**
- **gate** - authorization at the exact PR head against an operator-minted grant; findings are not authorization. Exit 0 pass / 1 block / 2 park / 3 refuse / 4 error.
- **flare** - best-effort notification sink over authoritative receipts; never gates.
- **console** - read-only local view of Gate's inbox and grant ledger; explains, never decides.
- **escalate** - agent -> human -> agent resolution channel for a parked Gate run.

**Skills:**
- **/work-driver** + **/work-driver-prep** - drive implementation; prep builds specs and conflict-safe batches.
- **/pr-risk** - decide how much review a change needs; reviewers perform it.
- **/review-coordinator** + **/review-digest** - consolidate reviewer findings; digest is the deterministic pre-pass.
- **/shipped** / **/status** / **/wip** - retrospective / current-session / portfolio-liveness views.
- **/consult** - ask a sibling repo's steward; knowledge to peers, authority to the operator.
- **/worktree-*** - add / list / remove / transfer / locate isolated checkouts.

### The loop

```text
dossier task -> /worktree-add -> spec -> ship driver (dispatch -> poll -> judgment -> land -> record)
  -> PR + CI -> /pr-risk -> reviewer panel -> /review-coordinator -> one findings artifact
  -> gate evaluates the exact head -> 0: emitted head-pinned merge command -> merge
  -> authoritative receipts -> dossier close-out -> /worktree-remove
       \-> 2: park -> console / gate next -> human decision -> escalate -> gate resolve -> gate next
       \-> attention or terminal receipt -> flare -> Slack (best effort; never gates)
```

`/work-driver` coordinates dispatch -> poll -> land and runs its own review triage inline. `/pr-risk` and `/review-coordinator` are explicit steps; the driver does not invoke them automatically.

### Why this shape

Each layer owns one responsibility and can be replaced without rippling: dossier owns what needs doing; worktree skills own where work happens; Ship owns agent execution and durable run state; pr-risk owns review depth; reviewer bots are swappable finders; review-coordinator owns their consolidated artifact; Gate alone owns exact-head merge authorization; Escalate carries a human resolution without deciding it; Console explains Gate state; Flare notifies; Consult handles cross-repo knowledge. The workbench is a menu, not a checklist.

### The shape underneath

The contract planes are **State** (dossier plus run, verdict, grant, and receipt artifacts), **Execution** (Ship), **Verification** (review and Gate's escalate-only verifier ladder), **Capability** (scoped operator-minted grants), and **Observability** (Console, Flare, /wip, /shipped, /status). This section is **Composition**. Planes share typed artifacts - evidence -> verdict -> action - rather than call stacks.
<!-- END dev-workbench -->

## Architecture

Strict layered dependency direction (mirrored from dossier / tower):

```
config / room → firecracker / rootfs / transport → runner / registry → main
```

- **`config`** — runtime config + the room path layout (state base, room dir, jail dirs); no I/O.
- **`room`** — per-room metadata (`RoomMeta`) + liveness probe; plain data plus its own persistence.
- **`firecracker`** — process spawn, API socket, VM config, boot/shutdown, orphan reap.
- **`rootfs`** — overlay/CoW, image paths (flake input lands here in v0.1).
- **`transport`** — repo bundle, scp into guest.
- **`runner`** — SSH exec, artifact capture, guest readiness.
- **`registry`** — `rooms ls` / `rooms gc` policy: scan the state base, classify liveness, reap orphans (composes the `firecracker` teardown).
- **`main`** — clap CLI, wires layers.

Don't introduce a downward import. If a feature needs a new dependency direction, lift the shared concern into `domain`.

Host layout (v0): Windows → Hyper-V → Ubuntu `rooms-host` → Firecracker microVM per room. The `rooms` binary runs on the Ubuntu host, not on Windows.

## Docs

| Doc | Purpose |
| --- | --- |
| [`docs/vision.md`](docs/vision.md) | What/why/non-goals/roadmap — operator-facing |
| [`docs/rooms-host-runbook.md`](docs/rooms-host-runbook.md) | Rebuild/reach/provision/validate the rooms-host VM end-to-end (+ gotchas) |
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

Per PR: Copilot, comment `@codex review`, comment `@claude review`, comment `@cursor review`. Ship-driven runs use `.ship.json` to trigger this panel. CI green before merge.

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

## Shipping features

Adapted from ship's workflow:

1. Spec doc under `docs/features/<feature>/spec.md` — what + why + acceptance + scope.
2. Branch (e.g. `prod-runner-contract`).
3. Implement.
4. PR with reviewers above.
5. CI green (`make check`).
6. Address review comments; repeat ~3× before merge.

## Merge authorization (gate)

Rooms PRs merge through **gate** (the governed boundary — see the workbench map): `gate grant -repo itsHabib/rooms -action merge` → `gate gate -pr N -grant <id>` → judge → run the head-pinned `gh pr merge` gate prints. Gate parks the typical rooms PR at *"no review decision reported by GitHub"* — the AI reviewers comment rather than APPROVE — plus the bot-comment consolidation.

**When the driver authored the PR, resolve that park with `gate judge -auto`, not `-decision pass`.** `-auto` sends the decision to an independent frontier model (opus, high effort) that rules from the recorded artifacts alone — a genuine second party — whereas the author asserting `-decision pass` is self-approval and defeats two-party review (the auto-mode classifier blocks it). Reserve `-decision pass` for a PR you did not author, or an explicit operator override.

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
- **"Add Docker / devcontainer / web preview"** → pause; these belong to other layers or a different product shape, while `rooms` owns the microVM lifecycle. Re-read [`docs/vision.md`](docs/vision.md); if a real consumer keeps needing one here, that's a design conversation, not an automatic no.
- **Firecracker misbehaving** → check per-room log under work dir, serial output, `tap-fc0` exists (`scripts/setup-tap.sh`).
- **Out-of-scope discovery during implementation** → add a one-line entry to [`docs/follow-ups.md`](docs/follow-ups.md); defer depth to the originating PR.

## Cloud runtime defaults

When firing `mcp__ship__ship` with `runtime: "cloud"` against this repo, use:

```js
cloud: {
  repos: [{ url: "https://github.com/itsHabib/rooms" }],
  env: { type: "cloud" },
  autoCreatePR: true,
}
```

`autoCreatePR: true` means ship writes the PR body. Prefer local runtime when the PR needs heavy framing.

<!-- local-offload:start -->
## Local-first offload

Before spending cloud tokens on a mechanical sub-step, check for a free local path (needs the `local` CLI / Ollama on this machine):

- Narrowing a big file list, extracting structure from noisy tool output, shallow classification -> `/offload`
- "Have we solved/decided this before?" questions about the operator's own work -> `/ask-portfolio`
- Triaging a PR's bot-comment pile -> `/review-digest <PR#>`

Deep judgment (code review, risk calls, dense-diff reasoning) stays with the primary model. If `local` is not on PATH, skip silently -- never block on this.
<!-- local-offload:end -->
