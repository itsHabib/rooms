**Status**: draft
**Owner**: @mh
**Date**: 2026-07-27
**Model/effort**: opus / extra — new Firecracker API surface; the pause→create sequence + the neutral-guest security precondition are correctness-critical.
**Related**: dossier task `snapshot-create` (id: `tsk_01KYGMJCQW6GT0GKTS3NNGJY2X`), design doc `docs/features/snapshot-fork-replay/spec.md` §4 D2, §5, §6, §7A

# snapshot module + `rooms snapshot` — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | new `src/snapshot` module (policy over `transport`), `src/main.rs` (CLI) | ~180 | 180 |
| Tests | metadata shape, neutral-guest refusal, pause/create call sequence (mocked transport) | ~140 | 70 |
| **Total** | | | **~250** |

Band: **ideal** per repo's PR sizing convention.

## Goal

There is no way to snapshot a warm room. Fork (phase P2) needs a **Full** Firecracker snapshot — vmstate + memory file — of a **neutral (secret-free)** paused guest, plus metadata to gate restore compatibility. This task adds the snapshot primitive and its metadata, gated on the authoritative `provenance` marker from task `sealed-neutral-base`.

## Behavior / fix

New `snapshot` module (policy layer sitting **over** `transport`):

- **`rooms snapshot <room-id> [--out <dir>]`** performs, in order:
  1. `PATCH /vm {state:"Paused"}`
  2. `PUT /snapshot/create {snapshot_type:"Full", snapshot_path, mem_file_path}`
  3. write `snapshot.json` metadata:
     `{schema_version, snapshot_id, created_at, fc_version, rootfs_hash, base_room_id, slot_index, guest_ip, base_repo_sha, secrets_delivered:false}`
- **Neutral-guest invariant (design D2):** refuse the snapshot if the room is not neutral — read the authoritative `RoomMeta.provenance` (from task `sealed-neutral-base`) and **refuse when `provenance != neutral`** with a named error. A post-secret snapshot would bake the secret into plaintext guest RAM on disk. Expose the assertion as a `--neutral` guard.
- **No-active-vsock precondition (design D7 v4):** assert there is no active vsock connection at snapshot time — Firecracker does not preserve active vsock connections across snapshot (transport reset on resume), so a held connection would be silently severed and could wedge the resume-apply agent. The quiesced beacon connect (task `sealed-neutral-base`) must have closed before the pause.
- **Transfer the slot, don't free it (design D8 v4 — Fable P1).** `rooms snapshot` only *pauses* the base, so its slot stays `TargetTaken` and a later `rooms restore --slot k` could never reclaim it. After a successful create, **terminate the base and transfer its slot to a snapshot-owned reservation token** — a slot-file shape `parse_token` classifies as **never-reclaim**, so `reconcile` (`slot.rs:297-329`) does not judge the dead `rooms snapshot` process's claim reclaimable. Do **not** bare-free the slot: the walk allocator refills the lowest freed hole first (`slot.rs:99-104`), so on a busy host a concurrent `rooms run` would steal `k` and starve every future restore permanently (the frozen guest IP is baked into `snapshot.mem`, no fallback slot). Name the crash window: a crash between create-success and teardown leaves the slot live-claimed → restore fails `TargetTaken` until `rooms gc` reaps the dead base (recoverable). Expose the teardown as `--consume` (or an explicit follow-on teardown).
- **Store as a credential:** the `snapshot.vmstate` + `snapshot.mem` pair and `snapshot.json` live under the room state dir at `0700` — treat the mem file as a secret.
- `fc_version` comes from `firecracker --version`; `rootfs_hash` is the hash of the mounted RO base.

## Acceptance

- `rooms snapshot` on a warm **neutral** room produces `snapshot.vmstate` + `snapshot.mem` + `snapshot.json` with correct metadata (all fields populated, `secrets_delivered:false`).
- A room whose `provenance` is `tainted` → snapshot **REFUSED** with a named error, nothing written.
- After a successful snapshot, the base's slot is held by a **never-reclaim reservation token** (not bare-freed and not left `TargetTaken` under the dead base) — a concurrent `rooms run` cannot walk-claim it, and `reconcile` leaves it held.
- Unit tests: metadata shape, neutral-guest refusal, pause/create call sequence (mocked transport), reservation-token transfer + `reconcile` never reclaims it, no-active-vsock precondition.

## Test plan

Rust unit tests against a mocked FC API — assert the exact `Paused`→`snapshot/create` call sequence, the metadata shape, and the refusal branch. The real `snapshot/create` is exercised by the phase intermediate gate alongside restore.

## Non-goals

- `restore()` (task `restore-single`).
- fork / netns / N clones (phase P2 `fork-clones`).
- checkpoint-receipt-as-artifact (phase P3 `checkpoint-receipts-harden`).
