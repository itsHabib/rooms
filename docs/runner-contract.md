# Runner contract

The substrate doesn't know about agents — it knows about the artifact layout and the
exit-code → status mapping. Anything that satisfies this contract can be a runner:
`claude -p`, a cursor SDK script, a plain shell command, a test suite.

## Artifact directory layout

Inside the guest at `/workspace/out/`, collected to the host as
`~/.local/state/rooms/<room_id>/out/`:

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

## `result.json` schema

Versioned singleton result file. `schema_version: 1` today; future bumps are additive
until v2.

```json
{
  "schema_version": 1,
  "status": "succeeded",
  "exit_code": 0,
  "started_at": "2026-05-23T22:14:00Z",
  "ended_at": "2026-05-23T22:18:42Z",
  "summary_path": "summary.md",
  "patch_path": "result.patch",
  "events_path": "events.ndjson",
  "command": ["claude", "-p", "..."]
}
```

| Field | Type | Notes |
|---|---|---|
| `schema_version` | integer | Must be `1`. Unknown major versions are rejected at collect. |
| `status` | string | `succeeded`, `failed`, `timed_out`, or `cancelled`. |
| `exit_code` | integer | Process exit code as reported by the guest exec layer. |
| `started_at` | RFC 3339 UTC | When the command began. |
| `ended_at` | RFC 3339 UTC | When the command finished (or was killed). |
| `summary_path` | string or null | Relative path under `out/`; null if absent. |
| `patch_path` | string or null | Relative path under `out/`; null if absent. |
| `events_path` | string or null | Relative path under `out/`; null if absent. |
| `command` | string array | argv of what was actually exec'd. |

Path fields (`summary_path`, `patch_path`, `events_path`) are relative to the `out/`
root. When present, the referenced file must exist on disk at collect time.

## Exit-code → status mapping

The substrate writes `result.json` based on the exec outcome:

| Outcome | `status` |
|---|---|
| Exit code `0` | `succeeded` |
| Non-zero exit code | `failed` |
| Killed by SIGTERM via `rooms` timeout | `timed_out` |
| Killed by SIGTERM via user cancel (Ctrl+C, `rooms destroy --force`) | `cancelled` |

Runner-internal failures (e.g. cursor SDK throws) are reported via `events.ndjson`
plus a non-zero exit code. The substrate does not introspect runner internals.

## `events.ndjson`

Free-form, runner-defined event stream:

- One JSON object per line.
- Convention (not requirement): `{ "ts": "...", "kind": "...", ...payload }`
- The substrate copies the file as-is; it does not parse events.

## Collect validation

`rooms collect --from <out-dir>` (or the future `rooms collect <room_id> --to <host-dir>`
after SCP) validates:

1. Required files present: `result.json`, `logs/stdout.log`, `logs/stderr.log`.
2. `result.json` parses and `schema_version == 1`.
3. Paths referenced in `result.json` exist on disk.
4. Missing required files → error naming the specific file.
5. Missing optional files → silent (field is `None` in `RunnerArtifacts`).

## Engineering decisions

- **Schema versioning from day one.** Cheap insurance against migrating receipts later.
- **Events path, not contents, in `RunnerArtifacts`.** Consumers stream large files.
- **Substrate does not parse `events.ndjson`.** Ferry only.
- **No JSON Schema file in the repo.** This doc is canonical for v0.
- **`cancelled` distinguished from `timed_out`.** Different operator intent; auditable separately.
