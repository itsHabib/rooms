**Status**: draft
**Owner**: @mh
**Date**: 2026-07-27
**Model/effort**: opus / extra — the load-bearing security primitive; neutrality-by-construction is what keeps secrets out of `snapshot.mem`.
**Related**: dossier task `sealed-neutral-base` (id: `tsk_01KYGQWEGWMT8VS080BGG9BZ5Y`), design doc `docs/features/snapshot-fork-replay/spec.md` §4 D2, §5, §7A, §12

# Sealed neutral-base mode + authoritative provenance + `rooms base-create` — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `src/room.rs` (RoomMeta.provenance + transitions), `src/firecracker` (base-create boot shape), `src/main.rs` (CLI) | ~180 | 180 |
| Tests | provenance monotonicity, seal enforcement, base-create boot shape | ~140 | 70 |
| **Total** | | | **~250** |

Band: **ideal** per repo's PR sizing convention.

## Goal

A Full Firecracker snapshot's memory file is plaintext guest RAM — if a base holds a secret at snapshot time, that secret is baked into every clone. The v1 approach tried to *observe* a `secrets_delivered` lifecycle event, but `RoomMeta` has no such field, lifecycle output is non-authoritative, and a `--keep` room can be tainted over SSH/vsock *after* the check. Neutrality must instead be enforced **by construction**: a distinct sealed boot mode plus an authoritative, monotonic `provenance` field that the snapshot path (task `snapshot-create`) reads to gate freezing.

## Behavior / fix

Introduce a sealed neutral-base boot mode and an authoritative provenance marker. **Neutrality is a property of *how the base is created and quiesced*, recorded as durable monotonic state — never an observed lifecycle event.**

- **`rooms base-create --repo <r> [--warm <cmd>]`** — boot a room with:
  - the vsock **secrets payload unarmed** but `/vsock` **present** (the resume-apply agent in task `restore-single` needs the channel wired, just not loaded),
  - `exec` / interactive ingress **refused**,
  - the agent process **not started**.
  - **repo transfer via the host-side transport bundle**, never a guest-side authed clone — a private-repo credential inside the base would break neutrality by construction (design FR1, §7A; Fable P3). `rooms` drives toolchain-warm over SSH (the last legitimate interactive use).
- **Quiesce before sealing (design D2 v3/v4 — load-bearing).** After warm-up, seal the base:
  - a **detached guest-side quiesce script** stops `sshd` + every non-essential daemon, **waits for its own invoking `sshd` ancestor to exit** (`rc-service sshd stop` only stops the *listener* — the per-connection child servicing the stop command lives on, and would be captured live in the snapshot), and asserts the process table is exactly `{init, kworkers, resume-apply agent}`.
  - the resume-apply agent then flips a **"quiesced" beacon** the host reads over a single vsock connect. `provenance = neutral` is written **only after** that beacon — never on the bare exit of the stop command (Fable P1).
  - **host keys:** the canonical image bakes `ssh_host_*` at build (`build-rootfs-alpine.sh:281`); a snapshot-capable image must drop build-time keygen (or delete the keys during quiesce) so no shared host key is captured — the fresh per-clone key is generated on resume (task `restore-single`) (Fable P2).
- **`RoomMeta.provenance: neutral | tainted`** — additive `room.rs` field (v-bump the persisted schema per the additive-write convention). Semantics:
  - `neutral` is set **only** for a `base-create` room, **only after the quiesced beacon**.
  - flips to `tainted` **irreversibly and monotonically** on any secret-arm, workload/agent start, or interactive session. No path back to `neutral`.
  - authoritative and persisted — not derived from lifecycle output.
- **Decision (design §10 Q1):** distinct `base-create` mode vs. default boot + a `--seal` flag. **Lean distinct mode** — a separate verb keeps the neutral boot shape un-ambiguous and avoids a boot-flag matrix; implement `base-create` as its own mode.

## Acceptance

- `rooms base-create` yields a room with `provenance=neutral`, `/vsock` present, **no agent running**, and repo content delivered via the transport bundle (no guest credential present).
- `provenance=neutral` is written **only after** the quiesced beacon — the process table at seal time is `{init, kworkers, resume-apply agent}` with no live `sshd` (listener or session child).
- Arming a secret / starting the agent / opening an interactive session flips `provenance=tainted` **durably and irreversibly** (a subsequent read never returns `neutral`).
- The snapshot path (task `snapshot-create`) reads `provenance` and refuses a non-neutral room.
- Unit tests cover: provenance monotonicity (no neutral-after-tainted transition), seal/ingress enforcement, the `base-create` boot shape (vsock present, agent absent), and that the neutral write is gated on the beacon (not the stop-command exit).

## Test plan

Rust unit tests: provenance transition table (assert monotonicity, assert every taint trigger flips it), ingress refusal on a sealed room, `base-create` boot-config shape, beacon-gated neutral write. The real quiesce + beacon + no-live-`sshd` invariant is exercised at the phase intermediate gate on rooms-host (mem-file grep proves the host key + warm-up secret are absent).

## Open sub-decision (carry to implementation)

Design §10 Q6/Q7 — the exact per-daemon quiesce/reseed protocol and whether warm-up should move off SSH entirely so `sshd` never runs. Current favorite: SSH-then-detached-stop-then-beacon. Resolve in the phase-1 spike; don't block this task on it, but structure the seal so the beacon is the gate.

## Non-goals

- snapshot create (task `snapshot-create`).
- restore + guest hygiene (task `restore-single`).
- netns fan-out / N clones (phase P2 `fork-clones`).
