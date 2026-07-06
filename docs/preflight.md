# Doctor preflight gate

**Every host / e2e run preflights on `rooms doctor` before it boots anything. A FAIL aborts the run at the door with the failing check's remediation; a WARN is allowed through but logged.**

Without this, a misprovisioned host (stale gateway IP, missing TAP, `ROOMS_FWD` not installed, nested-virt off, ...) fails deep in boot with a confusing error instead of loud at the door with the exact fix. The gate turns doctor's advisory report into a precondition.

## What counts as FAIL vs WARN

`rooms doctor` emits one `CheckResult { name, ok, message }` per check.

- **FAIL** — `ok: false`. Hard-aborts the run. The `message` *is* the remediation.
- **WARN** — `ok: true` and `message` starts with `warn:`. Allowed through, but recorded.
- **ok** — `ok: true`, no `warn:` prefix. Clean.

The `warn:` convention has one source: [`CheckResult::is_warning`](../src/doctor.rs).

## Gating a run

**Shell harness** — `rooms doctor` already exits non-zero on any FAIL, so gate on the exit code:

```sh
rooms doctor || { echo "preflight failed — fix the FAILs above before running"; exit 1; }
```

**Rust e2e harness** — use the [`preflight`](../src/preflight.rs) module for a structured decision:

```rust
use rooms::preflight;

let pf = preflight::run(rooms_bin, image)?;   // runs `rooms doctor --json` and parses it
if !pf.passed() {
    for line in pf.remediations() {
        eprintln!("preflight FAIL — {line}");
    }
    // abort before booting anything
}
```

`preflight::from_json(&json)` gates already-captured `--json` output (what the unit tests exercise against fixtures).

## Fail-safe

A report the gate **can't read** — unparseable output, or a `schema_version` this build doesn't understand — is a **failed** gate, never a silent pass (the same "couldn't verify ≠ clean" discipline [`rooms diff`](../src/main.rs) follows). A doctor that errored before emitting its JSON therefore aborts the run rather than waving it through.

## Scope

The gate wires the *existing* doctor checks; it adds none. New checks land with the surface that needs them (e.g. the pool adds its `ROOMS_FWD` / slots checks in its own work). Preflight reports, never fixes — no auto-remediation.
