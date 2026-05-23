**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-23
**Related**: dossier task `runner-contract` (id: `tsk_01KSBE3Z0WDF397EDMMP1N2FWX`), [v0 spec](../rooms-v0/spec.md)

# Runner contract — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source (1×) | `src/artifacts.rs`, `src/runner.rs` (light touch) | ~120 | 120 |
| Tests (0.5×) | `src/artifacts.rs` `mod tests`, round-trip + validation tests | ~120 | 60 |
| Docs (0×) | `docs/runner-contract.md` | ~150 | 0 |
| **Total weighted** | | | **~180** |

Band: **amazing**.

## Goal

Define the contract for any command executed inside a room. The substrate doesn't know about agents — it knows about the artifact layout and the exit-code → status mapping. Anything that satisfies this contract can be a runner: `claude -p`, the future cursor SDK script, a plain shell command, a test suite.

## Functional

**Artifact directory layout** (inside guest at `/workspace/out/`, collected to host as `~/.local/state/rooms/<room_id>/out/`):

```
out/
  result.json           required
  summary.md            optional (some commands have nothing to summarize)
  events.ndjson         optional (one JSON event per line; agents stream here)
  result.patch          optional (only if the command touched the repo)
  commits.txt           optional (only if the command made commits)
  logs/
    stdout.log          required (captured from the command)
    stderr.log          required (captured from the command)
```

**`result.json` schema** (versioned):

```json
{
  "schema_version": 1,
  "status": "succeeded" | "failed" | "timed_out" | "cancelled",
  "exit_code": 0,
  "started_at": "2026-05-23T22:14:00Z",
  "ended_at": "2026-05-23T22:18:42Z",
  "summary_path": "summary.md",       // null if absent
  "patch_path": "result.patch",        // null if absent
  "events_path": "events.ndjson",      // null if absent
  "command": ["claude", "-p", "..."]   // what was actually exec'd
}
```

**Exit-code → status mapping:**
- `0` → `succeeded`
- non-zero → `failed`
- killed by SIGTERM via `rooms` timeout → `timed_out`
- killed by SIGTERM via user cancel (Ctrl+C, `rooms destroy --force`) → `cancelled`

The substrate writes `result.json` itself based on the exec outcome. Runner-internal failures (e.g. cursor SDK throws) are reported via `events.ndjson` + non-zero exit; the substrate doesn't introspect.

**`events.ndjson` shape** (free-form, runner-defined):
- One JSON object per line.
- Convention (not requirement): `{ "ts": "...", "kind": "...", ...payload }`
- Substrate copies the file as-is; doesn't parse.

**Rust types** (`src/artifacts.rs`):

```rust
pub struct RunnerArtifacts {
    pub result: ResultJson,
    pub summary: Option<String>,
    pub patch: Option<String>,
    pub events: Option<PathBuf>,    // path, not contents (could be huge)
    pub stdout: PathBuf,
    pub stderr: PathBuf,
}

pub struct ResultJson { /* matches the schema above */ }
```

**`rooms collect` validation:**
- Required files present (`result.json`, `logs/stdout.log`, `logs/stderr.log`).
- `result.json` parses and `schema_version == 1`.
- Paths referenced in `result.json` exist on disk.
- Missing required → error with the specific file name.
- Missing optional → silent.

## Tradeoffs

- **Schema versioning from day one vs adding it when needed.** Adding it now is 5 LOC; adding it later means migrating existing receipts. Cheap insurance.
- **Free-form events vs typed events.** Free-form means any runner can emit any shape. Typed would couple the substrate to specific runners (cursor SDK events differ from claude-code events). Substrate stays runner-agnostic.
- **JSON for result vs NDJSON.** Result is a singleton; JSON is fine. Events are a stream; NDJSON is fine. Don't conflate.
- **`status` field separate from `exit_code`.** Some statuses (`timed_out`, `cancelled`) are substrate-known, not exit-code-derivable. Two fields let the substrate annotate.

## EDs (engineering decisions)

- **ED-1: `schema_version: 1` on `result.json`.** All future bumps are additive (new optional fields) until v2. v2 = breaking; substrate refuses unknown major versions.
- **ED-2: Events file path, not contents, in `RunnerArtifacts`.** Reading the whole file into memory at every collect would be wasteful; consumers stream it.
- **ED-3: Substrate does NOT parse events.ndjson.** The runner contract guarantees one-JSON-per-line, but the substrate's job is to ferry, not interpret.
- **ED-4: No JSON Schema file in the repo.** YAGNI for v0; if a third-party runner ever needs it, write it then. The spec in this doc is canonical.
- **ED-5: `cancelled` distinguished from `timed_out`.** Different operator intent; auditable separately.

## Validation

- `mod tests` round-trip: build a `ResultJson`, serialize to JSON, parse back, assert equality.
- Schema-version test: parse a `{ "schema_version": 99, ... }` blob; expect `UnsupportedSchemaVersion` error.
- Required-files test: build a fixture out-dir missing `result.json`; expect `MissingRequired("result.json")`.
- Required-files test: missing `logs/stdout.log`; expect `MissingRequired("logs/stdout.log")`.
- Optional files: build a fixture with `result.json` only; collect succeeds, `RunnerArtifacts.summary == None`.
- Path-on-disk test: `result.json` references `summary.md` but file is missing; expect `DanglingReference("summary.md")`.

## Risks

- **The contract under-specifies events.ndjson.** Real runners may emit conflicting event shapes that hurt downstream consumers (e.g. `/triage` skill expecting a specific kind). Mitigation: a follow-up "common event vocabulary" spec lands when the second runner does.
- **`status: "failed"` is coarse.** A test failure vs a SDK error vs a wrong-tree-fingerprint all map to `failed`. Mitigation: `events.ndjson` carries the granular reason; consumers parse if they care.

## Out-of-scope

- A typed events vocabulary (waiting for a second runner to prove what's shared).
- JSON Schema export for third-party validation.
- Streaming validation of events.ndjson during exec.
- Substrate-side rendering of `summary.md`.
- Multi-language SDK bindings.

## Implementation-plan

1. Write `docs/runner-contract.md` with the layout, schema, exit-code mapping, and event convention.
2. Add `ResultJson` and `RunnerArtifacts` structs in `src/artifacts.rs` with serde derives.
3. Implement `RunnerArtifacts::load(out_dir)` that walks the out-dir, validates required files, parses `result.json`, returns the struct or an error.
4. Wire `rooms collect` to call `RunnerArtifacts::load` and report validation errors with actionable messages.
5. Update the substrate's exec layer to write `result.json` (with `started_at`, `ended_at`, `command`) based on the exec outcome — including `status` for timeouts and cancellation.
6. Tests per the Validation section.
7. `make check` + smoke via a `rooms run` against a trivial shell command.

PR shape: one PR, ~180 weighted LOC. "amazing" band. Reviewers: Copilot, `@codex review`, `@claude review`.
