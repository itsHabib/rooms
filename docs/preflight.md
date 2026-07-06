# Doctor preflight gate

**Every host / e2e run preflights on `rooms doctor` before it boots anything. A FAIL aborts the run at the door with the failing check's remediation; a WARN is allowed through but logged.**

Without this, a misprovisioned host (stale gateway IP, missing TAP, `ROOMS_FWD` not installed, nested-virt off, ...) fails deep in boot with a confusing error instead of loud at the door with the exact fix. The gate turns doctor's advisory report into a precondition.

## What counts as FAIL vs WARN

`rooms doctor` emits one `CheckResult { name, ok, message }` per check.

- **FAIL** — `ok: false`. Hard-aborts the run. The `message` *is* the remediation.
- **WARN** — `ok: true` and `message` starts with `warn:`. Allowed through, but recorded.
- **ok** — `ok: true`, no `warn:` prefix. Clean.

The `warn:` prefix has one source — the `WARN_PREFIX` constant — checked by [`CheckResult::is_warning`](../src/doctor.rs) and emitted by the producing checks; the human `rooms doctor` renderer keys on `is_warning` too.

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

## Host-e2e harness

`scripts/e2e.sh` (`sudo -E make e2e`) is the one command that runs the full host-e2e loop on the rooms-host and reports a clean PASS/FAIL:

1. **build** the `rooms` binary,
2. **preflight** on `rooms doctor` — a FAIL aborts here, before anything boots,
3. **run** `cargo test --features e2e` (each test self-isolates its own scratch state base, so a crashed run never poisons the next),
4. **assert zero host-global leak** — no new `tap-fc*` interface, no orphaned firecracker process, a clean `rooms ls` — capturing diagnostics on any leak,
5. print a one-line PASS/FAIL and a log dir.

It's idempotent and safe to re-run. The leak-assertion logic — parse `rooms ls --json`, reject a report it can't trust (fail-safe), treat any leftover room as a leak — lives in [`registry::parse_ls_report`](../src/registry.rs) + [`ListReport::is_clean`](../src/registry.rs), unit-tested against fixtures and reused by the concurrency rig. Preconditions (named by the preflight if missing): `/dev/kvm`, firecracker + jailer, the `ROOMS_FWD` chain (`sudo bash scripts/setup-tap.sh --host`), and the guest images.

## Fail-safe

A report the gate **can't read** — unparseable output, or a `schema_version` this build doesn't understand — is a **failed** gate, never a silent pass (the same "couldn't verify ≠ clean" discipline [`rooms diff`](../src/main.rs) follows). A doctor that errored before emitting its JSON therefore aborts the run rather than waving it through.

## Scope

The gate wires the *existing* doctor checks; it adds none. New checks land with the surface that needs them (e.g. the pool adds its `ROOMS_FWD` / slots checks in its own work). Preflight reports, never fixes — no auto-remediation.
