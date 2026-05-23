**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-23
**Related**: dossier task `harden-firecracker-control` (id: `tsk_01KSBE3RNJKH9HF7YP3HSW61X4`), [v0 spec](../rooms-v0/spec.md)

# Harden Firecracker control plane — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source (1×) | `src/firecracker.rs`, `src/main.rs` (doctor), `src/runner.rs` (lifecycle hooks), `src/transport.rs`, `src/rootfs.rs` | ~250 | 250 |
| Tests (0.5×) | `tests/control_failures.rs` (new), unit tests in src files | ~200 | 100 |
| Configs / docs (0×) | error-taxonomy comment block | ~30 | 0 |
| **Total weighted** | | | **~350** |

Band: **amazing**. If POC's `firecracker.rs` is already larger than expected, this could push to **ideal**; reassess after #2 starts.

## Goal

Replace the POC's happy-path Firecracker control with production-quality plumbing: structured errors, timeouts on every API call, cleanup on every failure path, and a `rooms doctor` that runs real checks (not just "exists" probes).

The POC proves *a* path works; this task proves *the failure paths* are bounded.

## Functional

**Structured errors.** Replace `anyhow::Error` returns in `src/firecracker.rs` with a `FirecrackerError` enum:
- `KvmUnavailable` — `/dev/kvm` missing or permission-denied
- `BinaryNotFound { path }` — `firecracker` not on PATH or at configured path
- `ApiSocketNeverAppeared { timeout_ms }` — socket file didn't materialize within timeout
- `ApiCallFailed { endpoint, status, body }` — HTTP error from Firecracker's REST API
- `ApiCallTimedOut { endpoint, timeout_ms }`
- `GuestUnreachable { reason }` — SSH never connected or vsock never opened
- `ProcessExitedEarly { exit_code, stderr_tail }` — Firecracker died before InstanceStart

Top-level `RoomsError` wraps `FirecrackerError`, `RootfsError`, `TransportError`, `RunnerError`. Display impl preserves the chain.

**Timeouts on every API call.** Default 30s per HTTP call to the Firecracker API socket; configurable via `RoomsConfig`. `InstanceStart` → 60s. Guest reachability probe → 120s with 2s poll interval.

**Lifecycle cleanup.** Every failure path in `create → exec → collect → destroy` MUST clean up:
- Kill the Firecracker process (SIGTERM then SIGKILL after 5s grace)
- Release the TAP interface (`ip tuntap del`)
- Unmount and remove the per-room rootfs overlay
- Remove `~/.local/state/rooms/<room_id>/` work dir

Implement via a Drop impl on a `RoomGuard` RAII type. The `--keep` flag suppresses cleanup for debugging.

**`rooms doctor` real checks.** Replace the POC's "exists / doesn't exist" with:
- `/dev/kvm` present AND user has rw access (`access(2)` check)
- `firecracker --version` runs and reports a supported version (>= 1.7)
- Base kernel image (`vmlinux`) exists at expected path AND parses (`file` check or ELF header read)
- Base rootfs image exists, is ext4, has min size
- `ANTHROPIC_API_KEY` present in env
- TAP networking configurable: probe `ip tuntap add` + `ip tuntap del` round-trip
- Nested virt enabled: probe `kvm-ok` (or read `/sys/module/kvm_amd/parameters/nested` / `/sys/module/kvm_intel/parameters/nested`)

Output is structured (JSON with `--json` flag) so future automation can consume.

## Tradeoffs

- **Error enum vs `anyhow` everywhere.** Enum is more code but lets the runner make decisions (retry GuestUnreachable, fail-fast on KvmUnavailable). The discipline this enforces is worth the extra 50 LOC.
- **Drop-based cleanup vs explicit cleanup calls.** Drop is harder to debug when it fires unexpectedly but guarantees no leak. Mitigation: log every Drop fire at debug level.
- **Doctor as separate code path vs sharing checks with `create`.** Some duplication is fine — doctor is for the human, create is for the machine. They check overlapping things but with different output formats.

## EDs (engineering decisions)

- **ED-1: SIGTERM → SIGKILL with 5s grace.** Firecracker has a fast shutdown path; 5s is generous. If we see hangs in practice, tune.
- **ED-2: 30s default API timeout, 60s for InstanceStart, 120s for guest reach.** Numbers from Firecracker's quickstart docs + empirical buffer. Surface as `RoomsConfig` for override.
- **ED-3: Min supported Firecracker version 1.7.** Locked because the API socket shape stabilized at 1.x. Bump as needed.
- **ED-4: Doctor's `--json` output is forward-compatible.** Fields can be added; existing fields stay. Versioned via a top-level `schema_version: 1`.
- **ED-5: No retry logic in v0.1 hardening.** Caller decides whether to retry; substrate only reports outcome. Retry policy is consumer-shaped (a `/work-driver` may retry; a one-shot CLI may not).

## Validation

Three failure-injection tests in `tests/control_failures.rs`:
- **`firecracker_exits_early_is_caught`**: stub a fake firecracker binary that exits with code 2 immediately. Assert: `RoomsError::Firecracker(ProcessExitedEarly { exit_code: 2, .. })`, cleanup ran (work dir gone), no orphan process.
- **`api_socket_never_appears`**: stub a binary that runs but never opens the API socket. Assert: `RoomsError::Firecracker(ApiSocketNeverAppeared { .. })` after timeout, cleanup ran.
- **`guest_unreachable`**: real Firecracker, real boot, but kernel cmdline disables network. Assert: `RoomsError::Firecracker(GuestUnreachable { .. })` after 120s, cleanup ran.

`rooms doctor` validation:
- Run on a healthy `rooms-host`: all checks green.
- Disable `/dev/kvm` permissions temporarily: KVM check reports actionable error.
- Remove `vmlinux`: kernel check reports missing-file with the expected path.

## Risks

- **Tests requiring real Firecracker are slow.** Mitigation: gate behind `#[cfg(feature = "e2e")]`; CI runs them only on nightly or on a manually-triggered workflow.
- **Drop firing during panic.** Rust's drop-during-panic semantics can mask the original error. Mitigation: log drops at debug; ensure cleanup is idempotent (calling it twice is a no-op).
- **Doctor false-greens.** A check that returns "OK" but the underlying capability is broken (e.g. `firecracker --version` works but the binary segfaults on real use). Mitigation: doctor includes a `--deep` flag that boots a minimal VM end-to-end.

## Out-of-scope

- Retry policies (consumer's call, not substrate's).
- Multi-VM concurrent control plane (v0.2+).
- Snapshot/fork control surface (v0.2+).
- Metrics export (Prometheus, OpenTelemetry) — add when a consumer needs it.
- Replacing shell-out with a Rust crate (`firec` / `firepilot`) — re-evaluate at v0.2 if shell-out hits friction.

## Implementation-plan

1. Extract a `FirecrackerError` enum from current `anyhow` usage. Existing callers continue to wrap into `RoomsError`; this is a refactor.
2. Add `RoomGuard` RAII type holding work dir + process handle + TAP name. Drop impl invokes cleanup. Replace all manual cleanup with guard ownership.
3. Add timeouts to every HTTP call against the Firecracker API socket. Configurable defaults in `RoomsConfig`.
4. Rewrite `rooms doctor` against the real-check list above.
5. Write the three failure-injection tests. Gate behind `e2e` feature flag.
6. Run `make check` + `cargo test --features e2e` + manual `rooms doctor` smoke.

PR shape: one PR, ~350 weighted LOC. "amazing" band. Reviewers: Copilot, `@codex review`, `@claude review`.
