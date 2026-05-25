# rooms

Notes for agents working on this repo. Read before touching code.

`rooms` is the substrate that spawns disposable Firecracker microVMs with specified deps, runs a command in them, and collects artifacts. First consumer is an LLM agent (`rooms exec <id> -- claude -p < task.md`); other consumers (ship's `RoomCursorRunner`, `/work-driver` crash recovery, future replay) compose the primitive without it knowing about them.

This is a **v0 scaffold**. The POC is in flight; productionization (8 specs) waits on the POC's upper bar.

## State

- **Spec**: [`docs/features/rooms-v0/spec.md`](docs/features/rooms-v0/spec.md) — single source of truth for v0.
- **Productionization plan**: [`docs/features/01-productionization/driver.md`](docs/features/01-productionization/driver.md) — 8 specs in 3 batches, consumed by `/work-driver` after POC is green.
- **POC entry condition**: `rooms run --repo <path> --task <task.md>` produces a microVM boot + repo at `/workspace/repo` + `claude -p` run + `result.patch` returned, end-to-end.

A full CLAUDE.md (dev-workbench block, architecture deep-dive, conventions, common gotchas) lands as part of [task #8 `docs-vision-and-readme`](docs/features/docs-vision-and-readme/spec.md). This is the holding stub.

## Architecture (planned)

Strict layered dependency direction (mirrored from dossier / tower):

```
domain → firecracker / rootfs / transport → runner → main
```

Don't introduce a downward import. If a feature needs a new dependency direction, lift the shared concern into `domain`.

## Develop

```sh
make check        # fmt-check + clippy --all-targets -- -D warnings + test
```

`make check` is the single command CI runs (when CI lands per task #1) and the one to run before you push.

### Lint discipline

Mirrors dossier's:

- `clippy::all`, `pedantic`, `nursery`, `cargo` all warn-by-default
- Selective restriction: no `panic!`, `unwrap`, `indexing_slicing`, `dbg!`, `print_stdout`, `todo!`, `unimplemented!` in non-test code
- `unsafe_code = forbid`; `unreachable_pub`, `unused_lifetimes`, `unused_qualifications`, `non_ascii_idents` warn
- Complexity caps in `clippy.toml`: cognitive 20, lines 100, args 6
- CI fails on any warning (`-D warnings`)

Don't add `#[allow(...)]` without a one-line justification comment.

## Conventions

- **Errors-not-capitalized** (Go convention; matches the rest of the portfolio). `bail!("no /dev/kvm; nested virt off?")` not `bail!("No /dev/kvm...")`.
- **No design-doc or phase refs in code comments** — doc comments describe behavior; roadmap context belongs in commit messages and spec docs.
- **POC scope: anyhow is fine.** Structured errors (`FirecrackerError` enum) land in task #2 `harden-firecracker-control`. POC can use `anyhow::bail!` for now.

## Shipping features

Adapted from ship's workflow:

- Spec doc under `docs/features/<feature>/spec.md` — what + why + acceptance + scope (with PR sizing band).
- Branch (e.g. `prod-runner-contract`).
- Implement.
- PR. Request reviewers — Copilot, comment `@codex review`, comment `@claude review`.
- CI green (`make check` matrix).
- Address review comments (opinionated is fine; don't blindly accept).
- Repeat review cycle ~3× before reaching out.
- Merge when ready.

### Cloud runtime defaults

When firing `mcp__ship__ship` with `runtime: "cloud"` against this repo, use:

```js
cloud: {
  repos: [{ name: "rooms" }],
  env: { type: "cloud" },
  autoCreatePR: true,
  // skipReviewerRequest omitted → reviewers always requested (per the workflow above)
}
```

`autoCreatePR: true` means ship writes the PR body, so prefer local runtime when the PR needs heavy framing (architecture pivots, discovery logs). No `envVars` needed for normal substrate work; if a task actually needs a host secret inside the runner, surface it explicitly.

## PR sizing

Same bands as dossier / ship:

| Band    | Limit (weighted LOC) |
| ------- | -------------------- |
| amazing | < 500                |
| ideal   | < 700                |
| stretch | < 1000               |

Weights:

- production source (incl. doc comments): **1.0×**
- tests + fixtures: **0.5×**
- lockfiles, generated, configs (`Cargo.toml`, workflow YAML, etc.), docs: **0×**

## When you're stuck

- "Clippy warning I don't understand" → `make lint` locally, read the suggestion.
- "I want to add a runner that knows about the agent" → stop. The runner contract (#3) is substrate-side; agent specifics live in the runner script inside the rootfs (cursor-sdk-runner #4 is the pattern).
- "I want to add Docker support / devcontainer support / web preview" → stop. Per `feedback_opinionated_not_generic`, this is the Codespaces gravity well. Re-read the v0 spec's Non-goals.
