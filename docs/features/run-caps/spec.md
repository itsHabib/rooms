**Status**: implemented
**Owner**: @michael (human:mh)
**Date**: 2026-06-21
**Related**: `src/main.rs` (`post_boot`), `src/artifacts.rs` (`RunStatus`), [rooms-diff/spec.md](../rooms-diff/spec.md)

# `rooms run --max-wall` — a hard wall-clock cap on a run

## Goal

A runaway room spins forever with nobody watching: it holds `tap-fc0`, keeps a
microVM alive, and stalls the next run in a fleet. There is no upper bound on
how long a `rooms run` exec can take — the only stop is Ctrl-C (an operator at
the keyboard). That makes unattended / fleet use unsafe.

`RunStatus::TimedOut` already exists in `artifacts.rs` and is never produced.
Light it up: a hard wall-clock cap that, when reached, aborts the exec, records
a `TimedOut` `result.json`, and tears the room down — the precondition for
running agent fleets without a babysitter.

## Design

### CLI (`src/main.rs`)
`rooms run --max-wall <DURATION>` — optional; **omitted = no cap** (today's
behavior, unbounded). `DURATION` is an integer with an optional `s`/`m`/`h`
suffix (bare = seconds): `90s`, `30m`, `2h`, `1800`. Zero is rejected. The flag
**conflicts with `--keep`** (a `--keep` run intentionally holds the VM open and
never execs, so a cap is meaningless there).

### Where the cap lives — `RunArgs`, not `RoomsConfig`
The cap is **per-run policy** (set per invocation by the operator), not substrate
mechanism. `RoomsConfig` holds the substrate's operational timeouts (API,
guest-reach) — its own patience. The wall cap is the operator's run budget, so it
rides on `RunArgs` alongside `image`/`command`/`readonly_rootfs`, and threads to
`post_boot` as a parameter. (This is a deliberate deviation from the kickoff
sketch's "`Option<Duration>` on `RoomsConfig`" — same behavior, cleaner layering;
trivial to relocate if review prefers otherwise.)

### Mechanism (`post_boot`, `Action::Exec`)
The exec already runs inside a `tokio::select!` racing the work future against
`ctrl_c`. Add a third arm: a timeout future that sleeps for the cap, or
`std::future::pending()` (never resolves) when there's no cap — so an unset cap
adds no arm-behavior and needs no guard. The select is `biased` (work → ctrl_c →
cap), so a run that completes the same instant the cap fires keeps its real
result rather than a spurious 124. When the timeout wins it mirrors the existing
`Cancelled` arm exactly:
- ensure the guest artifact skeleton exists (so `rooms collect` validation passes),
- write a `result.json` with `RunStatus::TimedOut` and exit code **124** (the GNU
  `timeout` convention; distinct from 130 = cancelled, 2 = substrate error),
- return `Ok(124)`.

Both pre-teardown guest I/O steps — the `TimedOut`/`Cancelled` **record** and the
`--out` **collection** — are best-effort and **time-bounded** (`PRE_TEARDOWN_GRACE`):
a wall cap fires precisely on a runaway guest, which can accept TCP yet never
service a request (so SSH `ConnectTimeout` alone doesn't bound either). On grace
expiry the room is torn down *without* the artifact rather than letting a stalled
guest block teardown. This matters because both run *before* `vm.shutdown()`, so
without the bound a hung guest would leak the VM + `tap-fc0` — the exact runaway
the cap exists to reclaim.

Dropping the `work` future cascades `kill_on_drop` across the spawned SSH clients
(host side), and `run_room`'s `vm.shutdown()` — which runs regardless of the
record/collection outcome — tears the microVM down, so the cap actually reclaims
the room. `--out` collection still runs on the success path (the `TimedOut`
`result.json` + partial artifacts are collected like any other run).

## Acceptance
- `rooms run --command <c> --max-wall 1s` against a command that sleeps longer
  exits **124**, writes a `result.json` with `status: "timed_out"`, and the
  microVM is shut down.
- Even if the guest is **unresponsive** when the cap fires, the room is still torn
  down within a bounded grace — both the `timed_out` record and `--out` collection
  are time-capped, so teardown never waits on a hung guest.
- A run that finishes before the cap is unaffected (exit code + status unchanged),
  including the exact-tie case (completed work wins the `biased` select over the cap).
- `--max-wall` omitted = unbounded (no behavior change from today).
- `--max-wall 0` and a non-numeric value are rejected at parse time; `--max-wall`
  + `--keep` is rejected by clap.
- `90s` / `30m` / `2h` / bare-seconds all parse to the right `Duration`.
- `make check` green (build + clippy `-D warnings` + unit tests, Windows + Linux).
- **Host e2e (rooms-host):** a `--readonly-rootfs`/`--command` run that sleeps past
  a short `--max-wall` exits 124 with `TimedOut`, and `tap-fc0` is free afterward.

## Test plan
- Unit: the duration parser (suffixes, bare seconds, zero-rejection, junk); the
  `--max-wall`/`--keep` clap conflict; `--max-wall` parses onto `RunArgs`.
- E2e (host-only): the sleep-past-cap run above; assert exit 124 + `timed_out`
  status + room torn down.

## Out of scope (later, if asked)
- A *default* cap (today: opt-in only; a fleet driver can pass one).
- CPU / memory / disk caps (this is wall-clock only).
- Per-phase caps (boot vs exec) — one cap over the whole exec for v0.
- Killing the guest *process* gracefully before VM teardown — teardown is the cap.
- Honoring `--max-wall` on the no-exec **Idle** path (a `rooms run` with neither
  `--command` nor `--runner cursor`): that path is a 3s POC placeholder, so the
  cap is a no-op there (no exec to bound). The flag is meaningful on `--command`
  and `--runner cursor` runs.
