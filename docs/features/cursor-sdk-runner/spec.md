**Status**: accepted
**Owner**: @michael (human:mh)
**Date**: 2026-05-23 (reconciled to the Alpine agent rootfs 2026-05-30)
**Related**: dossier task `cursor-sdk-runner` (id: `tsk_01KSBE46THH7TXHNKBP49X9AG3`), [v0 spec](../rooms-v0/spec.md), [runner-contract](../runner-contract/spec.md), [agent-rootfs-alpine-kernel](../agent-rootfs-alpine-kernel/spec.md)

# Cursor SDK runner inside microVM — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source (1×) | `src/runner.rs` (Runner enum, cursor exec, clone/stage/patch, SSH user, seed_entropy removal), `src/main.rs` (`--runner`/`--repo`/`--task`/`--model`/`--base-sha`), `src/error.rs` | ~250 | 250 |
| Production source (1×) | `scripts/rootfs/install-cursor.sh` (`--extend` hook; embeds `cursor-runner.js`) | ~210 | 210 |
| Tests (0.5×) | gated `tests/cursor_runner_e2e.rs`; clap + serde unit tests | ~90 | 45 |
| Docs (0×) | this spec, `docs/follow-ups.md` | ~40 | 0 |

Band: **ideal** (~505 weighted). Folds in two deferred runner fixes (SSH user, `seed_entropy`) per `docs/follow-ups.md`.

## Goal

A Node script baked into the agent rootfs that wraps `@cursor/sdk` — same `Agent.create() → send() → stream() → wait()` shape as ship's `LocalCursorRunner`, simplified for one-shot execution inside the microVM. `rooms run --runner cursor` becomes a real path, and ship's eventual `RoomCursorRunner` (#5) has something to invoke.

## The governing reconciliation: who owns `result.json`

The runner-contract (`docs/runner-contract.md`, `runner-contract/spec.md`) is canonical: the **substrate** writes `result.json` (via `artifacts::ResultJson::from_exec` + the `EXIT=<n>` marker), and a runner emits only the *optional* artifacts (`summary.md`, `events.ndjson`, `result.patch`). This spec's earlier draft had the runner write `result.json` too; that contradiction is resolved in favor of the contract.

**`cursor-runner.js` writes only `events.ndjson` + `summary.md` and signals outcome purely via its process exit code.** The substrate maps the exit code to `result.json.status` exactly as it does for `--command`. No change to `artifacts.rs` — its schema already carries `summary_path` / `patch_path` / `events_path`.

## Functional

### Rootfs additions — via `--extend`, not the base builder

The base builder (`scripts/build-rootfs-alpine.sh`) bakes **no Node** — the lean claude-only `agent-alpine.ext4` stays minimal (`agent-rootfs-alpine-kernel` ED). Node + the SDK land through the builder's `--extend <script>` chroot hook:

```sh
sudo ./scripts/build-rootfs-alpine.sh \
  --out images/agent-alpine-cursor.ext4 \
  --size 1G \
  --ssh-key ~/.ssh/id_rooms.pub \
  --extend scripts/rootfs/install-cursor.sh
```

This produces a **separate** `agent-alpine-cursor.ext4`. `scripts/rootfs/install-cursor.sh` (runs chroot'd as root after the baseline install):

- `apk add --no-cache nodejs npm` — Alpine 3.21 community ships Node 22 (≥20), musl-linked. No NodeSource/APT (the image is pure musl, no glibc).
- Compiles the vendored SDK: `@cursor/sdk` depends on `sqlite3` (a native addon with no musl prebuilt), so a virtual `.cursor-build` group (`python3 make g++ linux-headers`) is added for the `npm install`, then dropped.
- Vendors a **pinned** `@cursor/sdk@1.0.16` into `/opt/rooms/cursor-runner/node_modules/` (reproducible; ED-1).
- Writes `cursor-runner.js` to `/opt/rooms/cursor-runner/cursor-runner.js`, owned `rooms:rooms` (uid 1000).
- Adds `AcceptEnv CURSOR_API_KEY` to `/etc/ssh/sshd_config` (the base builder already adds `AcceptEnv ANTHROPIC_API_KEY`).
- Build-time smoke gate: `node --version` + `grep -qiE 'symbol not found|Error relocating'` (the same musl gate the base builder runs for claude) + `node --check cursor-runner.js`.

`cursor-runner.js` is single-sourced in the `--extend` hook's heredoc (the builder copies only the one hook script into the chroot), with `node --check` guarding syntax at build time.

### `cursor-runner.js` (Node ESM, one-shot)

Input from `/workspace/in/`:
- `task.md` — the prompt sent to the agent.
- `meta.json` — `{ base_sha, model_id }`. (`model_params` / `agent_name` are deferred; the script reads them defensively if present.)

Auth: reads `CURSOR_API_KEY` from env (forwarded host→guest by SSH `SendEnv` + the image's `AcceptEnv`). Empty/unset → structured `api_key` error, exit 2.

Execution (mirrors ship's `LocalCursorRunner.#startAgent` + `#runPipeline`):

```js
const agent = await Agent.create({
  apiKey: process.env.CURSOR_API_KEY,
  model: { id: meta.model_id, ...(meta.model_params && { params: meta.model_params }) },
  local: { cwd: "/workspace/repo", settingSources: ["project"] },
  ...(meta.agent_name && { name: meta.agent_name }),
});
const run = await agent.send(taskMd);
for await (const ev of run.stream()) appendEventNdjson(ev);
const result = await run.wait();   // RunResult; status ∈ finished | error | cancelled
writeSummary(result.result ?? "");
await agent[Symbol.asyncDispose]();
```

`/workspace/repo` and `/workspace/in/` are owned by `rooms` (uid 1000); the script runs as `rooms@`, not root.

**Invariant:** `events.ndjson` and `summary.md` are created (empty) before any early exit, so the substrate can set `events_path` / `summary_path` unconditionally without a probe and `RunnerArtifacts::load` never hits `DanglingReference`.

### Error taxonomy (mirrors ship's `LocalCursorRunner`)

| Trigger | `events.ndjson` error line | exit code | `result.json.status` (substrate) |
|---|---|---|---|
| `CURSOR_API_KEY` unset/empty | `phase:"api_key"`, `error:"CURSOR_API_KEY environment variable is not set"` | 2 | failed |
| `@cursor/sdk` import fails (musl) | `phase:"sdk_load"` | 2 | failed |
| `Agent.create` rejects | `phase:"agent_create"`, `error:"Agent.create failed"` | 2 | failed |
| `agent.send` rejects (dispose first) | `phase:"send"`, `error:"agent.send failed after Agent.create"` | 2 | failed |
| stream throws **and** `wait()` yields no terminal | `phase:"stream"`, `error:"stream errored without a terminal RunResult"` | 2 | failed |
| clean stream, then `wait()` rejects | `phase:"wait"`, `error:"run.wait() rejected after a clean stream"` | 2 | failed |
| terminal `RunResult.status === "error"` (not thrown) | `kind:"result"`, `status:"failed"` | 1 | failed |
| terminal `status === "cancelled"` | `kind:"result"`, `status:"cancelled"` | 1 | failed |
| terminal `status === "finished"` | `kind:"result"`, `status:"succeeded"` + `summary.md` | 0 | succeeded |

Exit-code design: **0 = succeeded, 1 = agent-level failure, 2 = runner/SDK error.** The substrate maps zero/non-zero only; the granular reason lives in `events.ndjson` (the substrate never parses it). The four `CursorRunFailedError` strings + the `MissingApiKeyError` string are copied verbatim from ship so the two stay legibly parallel; a stream error best-efforts `wait()` first and prefers a terminal `RunResult` if one exists.

### Substrate side (`src/runner.rs`, `src/main.rs`)

- `Runner` enum: `Command(String)` (the existing path) and `Cursor(CursorRequest)`. `runner::exec` dispatches; both route their guest command through the same `bash -c {quoted} … echo EXIT=$?` wrapper.
- `CursorRequest { repo_url, task_md, meta: CursorMeta { base_sha, model_id } }`.
- The cursor path: `git clone <repo_url> /workspace/repo && git checkout <base_sha>` → stage `/workspace/in/{task.md,meta.json}` → `node /opt/rooms/cursor-runner/cursor-runner.js < /dev/null` through the wrapper → generate `result.patch` (`git add -A && git diff --cached <base_sha>`) → write `result.json` with the cursor artifact paths set.
- `< /dev/null`: any guest process inherits the SSH session's stdin; closing it avoids the `claude -p`-style stdin-consume footgun. The runner reads its prompt from `task.md`, never stdin.
- CLI: `--runner {command|cursor}` (default `command`); `--repo`, `--task`, `--model`, `--base-sha` are `required_if_eq("runner","cursor")`; `--keep` conflicts with the exec paths.
- SSH user is `rooms@` (a `GUEST_USER` const), not `root@` — folds in follow-up #3 (`PermitRootLogin no`; claude-code refuses `--dangerously-skip-permissions` as root).
- `SendEnv CURSOR_API_KEY` alongside the existing `SendEnv ANTHROPIC_API_KEY`.
- `seed_entropy` removed — folds in follow-up #5 (see ED-8).

## EDs (engineering decisions)

- **ED-1: Vendor `@cursor/sdk` at a pinned version** in `/opt/rooms/cursor-runner/node_modules/`. Reproducibility > disk.
- **ED-2: Mirror ship's `LocalCursorRunner` error taxonomy** (api_key / agent_create / send / stream / wait phases), copying the category strings verbatim.
- **ED-3: Runner communicates with the substrate via the filesystem** (`/workspace/in`, `/workspace/out`) and the process exit code — not stdio. Keeps the "any command" substrate contract intact.
- **ED-4: `result.json` is substrate-owned.** `cursor-runner.js` writes only `events.ndjson` + `summary.md`; the substrate maps exit→status (0→succeeded, non-zero→failed) and owns `timed_out`/`cancelled`.
- **ED-5: No mid-run streaming runner→substrate.** The substrate sees terminal state; a live tail is `tail -f /workspace/out/events.ndjson` under `--keep`.
- **ED-6: musl, not glibc.** The image is pure musl (`ld-musl-x86_64.so.1`); Node + any native addon must be musl-linked. `sqlite3` (a transitive SDK dep) compiles from source under musl via a build-time-only toolchain. The build smoke gate guards `node --version` against relocation errors.
- **ED-7: SSH user is `rooms@`, not `root@`** (folds in follow-up #3).
- **ED-8: `seed_entropy` removed** (folds in follow-up #5). The FC CI 6.1.155 kernel has `CONFIG_HW_RANDOM_VIRTIO=y`, so the `/entropy` device firecracker attaches surfaces as `/dev/hwrng` and seeds the CRNG natively; Alpine has no python, so the old ioctl one-liner would fail. The `/entropy` attach in `firecracker.rs` stays.
- **ED-9: separate `agent-alpine-cursor.ext4` image.** Node + the compiled SDK push past the lean base; rather than fatten the shared base (or the 300 MB `test-rootfs-alpine.sh` ceiling), the cursor toolchain ships in its own variant built with a larger `--size`.

## Validation

- `make check` (CI gate): fmt + clippy `--all-features -D warnings` + unit tests. Covers the `Runner`/clap surface and the `CursorMeta` serde shape; the gated e2e target compiles under `--all-features` but does not run in CI (no KVM).
- Build the cursor image (`--extend scripts/rootfs/install-cursor.sh`), then `./scripts/test-rootfs-alpine.sh images/agent-alpine-cursor.ext4` (proves `rooms@` login, `/dev/hwrng`, musl). The build's own `node --version` + `node --check` gate proves Node links and the script parses.
- End-to-end dogfood on the rooms-host against `agent-alpine-cursor.ext4`, guest user `rooms@`:
  - **Success round-trip:** a fixture repo + `task.md` "append a line to README.md"; assert `events.ndjson` is valid NDJSON, `summary.md` present, `result.patch` shows the line, exit 0 / `result.json.status == succeeded`.
  - **Auth failure (must pass):** run with no `CURSOR_API_KEY`; assert non-zero exit and an `events.ndjson` line `{ kind:"error", phase:"api_key", message:/CURSOR_API_KEY/i }`.
  - The Rust orchestration (`rooms run --runner cursor`) is validated by its propagated exit code (0 / non-zero).

## Risks

- **`@cursor/sdk` musl native addons.** `sqlite3` compiles from source under musl; if it (or a future transitive addon) fails to build/link, the build's `node --version` gate won't catch an addon-level issue — only the first real `Agent.create`. Mitigation: pin the SDK version; an import-smoke (`node -e "import('@cursor/sdk')"`) can be added to the installer if it proves flaky.
- **Image size.** Node + a compiled SDK pushes the cursor image well past the lean base. Accepted for now (separate variant, larger `--size`); slimming (`apk del` more aggressively, prune `node_modules`) is a later optimization.
- **Drift from ship's `LocalCursorRunner`.** Mitigation: verbatim category strings; when ship's `RoomCursorRunner` (#5) lands, consider co-locating the script.

## Out-of-scope

- **Host-side artifact collection.** Pulling `/workspace/out` from the guest to a host dir (so `rooms collect --from` works end-to-end) is a separate transport concern; v0 inspects artifacts in-guest (e.g. under `--keep`).
- `model_params` / `agent_name` CLI surface — deferred; `meta.json` carries only `base_sha` + `model_id`.
- Cursor cloud runtime (ship's `CloudCursorRunner`); mid-run steering (`agent.send` twice); MCP passthrough into the room.

## Implementation-plan

1. Reconcile this spec to the Alpine rootfs; close follow-ups #3 + #5.
2. `src/runner.rs`: `GUEST_USER` const + `rooms@`; `SendEnv CURSOR_API_KEY`; remove `seed_entropy` (+ the now-dead `RunnerError::EntropySeed` and unused import); `Runner`/`CursorRequest`/`CursorMeta`; `exec` dispatcher; `exec_cursor_in_guest` (clone → stage → run → patch) over a shared `run_wrapped` + `write_guest_file`.
3. `src/main.rs`: `RunnerKind` + `--runner`/`--repo`/`--task`/`--model`/`--base-sha`; `RunArgs`/`Action` to stay under the arg-count cap; drop the `seed_entropy` call; clap tests.
4. `scripts/rootfs/install-cursor.sh`: Node + vendored pinned SDK (sqlite3 build toolchain) + embedded `cursor-runner.js` + `AcceptEnv CURSOR_API_KEY` + smoke gates.
5. Gated `tests/cursor_runner_e2e.rs`.
6. Build `agent-alpine-cursor.ext4`; dogfood success + auth-failure on the rooms-host.

PR shape: one PR, ~505 weighted LOC, **ideal** band. Reviewers: `@codex review`, `@claude review`, `@cursor`.
