# Secret injection via vsock — first-read-then-delete

**Status:** v0 spec — the artifact we build from.
**Owner:** @itsHabib (human:mh)
**Date:** 2026-07-20
**Dossier:** `rooms` / `01-productionization` / `tsk_01KSDN6A98KBD08R9P0VZP9DRC`
**Related:** [`host-witness/spec.md`](../host-witness/spec.md) (the composition this
completes — see §2), [`cursor-sdk-runner/spec.md`](../cursor-sdk-runner/spec.md)
(today's consumer), [`readonly-rootfs-with-overlay/spec.md`](../readonly-rootfs-with-overlay/spec.md)
(why nothing the guest writes persists).

> **Reviewers — focus areas:** §5 (the delivery protocol and its ack — is the
> "delivered" signal sound?), §6 (fail-closed matrix — any path where a workload
> starts without its secret?), §7 (layering — mechanism vs policy), §9 phase
> ordering (image compatibility during migration).

## 1. Problem

API keys reach the guest today via SSH environment forwarding: the host runs
`ssh -o SendEnv=CURSOR_API_KEY …` (`src/runner.rs`), the guest's sshd has a
matching `AcceptEnv` (`scripts/rootfs/install-cursor.sh`), and the workload
reads `process.env.CURSOR_API_KEY`. This already avoids the classic footguns —
the key is not on the kernel cmdline, not in host `ps` argv, not baked into the
image. What it cannot avoid:

- **The secret is ambient in the workload's environment.** It sits in
  `/proc/<pid>/environ` of the runner and of every child the agent spawns, for
  the whole life of the room. An agent that runs `env`, crashes with an
  environment dump, or is prompted into exfiltrating its own environment hands
  the key over. Everything executed in the room inherits it by default.
- **The transport is entangled with the workload channel.** SendEnv rides the
  same SSH session that runs the workload, so "which secrets exist" leaks into
  every place that builds an ssh command line, and the sshd config of every
  image must enumerate `AcceptEnv` names to stay in sync with the host.
- **Nothing scopes delivery in time.** The env is delivered on every SSH
  session, for as long as the room lives — there is no "handed over once,
  then gone."

The fix is a dedicated, per-room, one-shot delivery channel that exists only
between boot and workload start: **virtio-vsock**, Firecracker's host↔guest
socket primitive with no IP layer and no relation to the workload's SSH path.

## 2. Why now (composition with the witness)

`--witness` (#77) gave a room unforgeable **egress** evidence: every frame the
guest sends is captured on the host side of its private tap. This feature
closes the complementary gap on the **ingress of trust**: after it, a room runs
its workload with *no ambient secret in the environment* — the key exists only
inside the runner process that needs it — *and* every byte it sends out is
witnessed. That pair is what the portfolio's verification plane needs rooms to
guarantee: a workload that can neither quietly hold a credential nor quietly
talk to the network.

## 3. Threat model

In scope — after this feature, for a run with vsock-delivered secrets:

| # | Property | Enforced where |
| --- | --- | --- |
| T1 | The secret never appears on the kernel cmdline or in host `ps` argv | already true today; preserved (vsock adds no argv) |
| T2 | The secret never persists in the guest: no disk write (rootfs is a RO overlay), the tmpfs staging file is deleted before the workload's agent starts | guest fetch hook + runner |
| T3 | The secret is absent from the workload's environment (`/proc/<pid>/environ` of the agent process tree) | runner reads the staging file into process memory, never exports |
| T4 | The secret cannot land in collected artifacts or `result.patch` by default flow | staging file lives under `/run` (tmpfs, outside `/workspace`), deleted pre-agent; patch is `git diff` of `/workspace/repo` |
| T5 | Delivery is per-room and one-shot: the channel is torn down after the first successful read; a later guest process cannot re-fetch | host listener unbinds after first delivery |
| T6 | No secret ⇒ no workload: any failure to deliver fails the room closed — `workload_started` is never emitted | host-side gate |

Out of scope (unchanged trust assumptions):

- **The host is trusted.** Secrets originate in the host process environment
  (`sudo -E rooms run`); a compromised host was always game over.
- **The runner process itself holds the secret in memory.** The agent needs the
  key to talk to its API; T3 narrows exposure from "everything in the room,
  ambient, forever" to "one process's heap, deliberately". A debugger inside
  the guest reading the runner's memory is accepted residual risk on a
  disposable VM.
- **Channel encryption.** The vsock path is host↔guest within one machine,
  scoped to a per-room socket inside that room's jail chroot; there is no
  network segment to eavesdrop.
- **Secret rotation, leases, multi-tenant policy.** Rooms is a substrate; this
  is delivery, not a secrets manager.

## 4. Requirements

Functional:

- **FR1** `rooms run --secret <NAME>` (repeatable) requests vsock delivery of
  the named variables. Values are read from the host process environment at
  admission; a named variable that is unset or empty fails the run *before any
  slot is claimed* (fail closed, fail early).
- **FR2** The boot configures a per-room virtio-vsock device (Firecracker
  `PUT /vsock`, pre-boot) whose UDS lives inside the room's jail directory —
  scoping and teardown ride the existing per-room jail lifecycle.
- **FR3** A host-side one-shot listener serves the secrets blob to the first
  guest connection, confirms delivery via the protocol ack (§5), then closes
  and unlinks the socket. One connection, ever.
- **FR4** The guest fetches at boot via a baked one-shot hook, stages secrets
  at `/run/rooms/secrets.env` (tmpfs, `0600`, owned by the runner user), and
  acks. The hook is inert when no vsock device is present (images stay usable
  for secretless runs).
- **FR5** The workload is gated on delivery: the run proceeds past readiness
  to `workload_started` only after the host has observed the ack. No ack
  within the readiness window ⇒ the room is torn down having never started
  the workload, with a distinct lifecycle event (§6).
- **FR6** The cursor runner (`cursor-runner.js`) consumes the staging file —
  read, parse, **delete**, then construct the SDK client from memory — and
  falls back to `process.env` only when the file is absent (migration
  compatibility, removed in the last phase).
- **FR7** For every name delivered via vsock, the SSH `SendEnv` forwarding of
  that name is suppressed on all workload sessions of that run.
- **FR8** Lifecycle stream gains `secrets_delivered` and
  `secrets_failed{error}` events; consumers can distinguish "never delivered"
  from boot/workload failures without message-matching.

Non-functional:

- **NFR1** Layering per CLAUDE.md: `firecracker`/`transport` own the vsock
  *mechanism* (device config, listener, byte moving); *policy* — which names,
  admission checks, the workload gate — lives in the layers above (`runner`,
  `main`). Nothing agent-specific enters `src/`.
- **NFR2** The listener holds the blob in memory only; it is zeroized/dropped
  immediately after delivery (best-effort `zeroize`-style overwrite; no swap
  concerns beyond what the host already accepts for its own env).
- **NFR3** Secretless runs are byte-for-byte unaffected: no vsock device is
  added, no listener starts, no new events are emitted.
- **NFR4** `rooms doctor` learns a check for guest-kernel vsock support
  (`CONFIG_VIRTIO_VSOCKETS`) so a bad kernel fails preflight, not mid-run.

## 5. Design

### 5.1 Channel mechanics (Firecracker hybrid vsock)

Firecracker exposes vsock as a UDS on the host ("hybrid" model). For
**guest-initiated** connections — the only direction this feature uses — the
guest connects `AF_VSOCK` to `(cid=2, port=P)` and Firecracker hands the
stream to whatever is listening on the host at `<uds_path>_<P>`.

- `PUT /vsock` is called during boot config (alongside `/boot-source`,
  `/drives`, …, strictly before `InstanceStart`), body
  `{ "guest_cid": 3, "vsock_id": "vsock0", "uds_path": "./v.sock" }`. The path
  is chroot-relative under the jailer, so the socket materializes inside the
  room's jail root — per-room by construction, reaped with the jail.
- The delivery port is fixed: **`ROOMS_SECRETS_PORT = 5000`**. The host binds
  `<jail_root>/v.sock_5000` *before* `InstanceStart`, so the guest can never
  race the listener.
- `guest_cid` is constant (3): with the hybrid UDS model there is no host-wide
  CID namespace to collide in; isolation comes from the per-jail socket path.

### 5.2 Protocol (one round-trip, then gone)

```
guest                                   host (one-shot listener)
  │  connect AF_VSOCK cid=2 port=5000     │  accept (first connection only)
  │ ◀──────────────────────────────────── │  write blob; shutdown(WR)
  │  read to EOF                          │
  │  write /run/rooms/secrets.env         │
  │  (tmpfs, 0600, runner-owned)          │
  │ ────────────────────────────────────▶ │  read "OK\n"  → delivered
  │  close                                │  close; unlink socket; drop blob
```

- **Blob format:** `NAME=value\n` per secret, UTF-8, nothing else. Values are
  host-env strings; a value containing `\n` is rejected at admission (FR1) —
  the format stays trivially parseable by a shell-free reader.
- **The ack is the delivery signal.** A successful socket write proves nothing
  (buffers); the guest acks only after `secrets.env` is durably staged with
  its final mode/owner. The host marks the run "delivered" only on reading the
  ack.
- **First-read-then-delete, both sides:** the host unlinks the listener socket
  and drops the blob after the first delivery — a second connect finds nothing
  to talk to. The guest runner deletes `secrets.env` after parsing it (FR6),
  so during workload execution no secret material exists outside the runner's
  heap.

### 5.3 Guest fetch hook

A boot-time one-shot (ordered before sshd in the image's init sequence, same
bake seam as `overlay-init`): if `/dev/vsock` exists, connect to port 5000
with a short timeout, stage the file, ack. If `/dev/vsock` is absent — a
secretless run, or an image booted outside rooms — exit 0 silently (FR4);
the authoritative gate is host-side, so the hook stays dumb (NFR1 in guest
form). Client binary: `socat` (`VSOCK-CONNECT:2:5000`), added to the image
bake — small, packaged in Alpine, no custom compiled fetcher to maintain.
The hook runs before sshd so that by `ssh_ready` the ack has normally already
landed; the host's wait (§5.4) is a bounded formality, not a race.

### 5.4 Host sequencing (where the gate sits)

```
slot_allocated → [bind listener] → vmm_started → guest_ready → ssh_ready
      → [await ack ≤ secrets timeout]
            ├─ ack     → secrets_delivered → workload_started → …
            └─ no ack  → secrets_failed    → (no workload) → cleanup_done
```

The listener task starts before `InstanceStart` and runs concurrently with
boot. After `ssh_ready`, the run awaits the delivery signal with a bounded
timeout (default 10s — generous; the fetch normally completes before sshd is
even up). `secrets_failed` is terminal for the room: teardown follows the
same path as `guest_unreachable`, and `result.json` is written with a distinct
failure so the collected room says *why* nothing ran.

### 5.5 Runner consumption (cursor)

`cursor-runner.js` (baked, `install-cursor.sh`): at startup, if
`/run/rooms/secrets.env` exists — read, parse `NAME=value` lines, `unlink` the
file, use `CURSOR_API_KEY` from the parsed map; else fall back to
`process.env.CURSOR_API_KEY` (migration only; the fallback and sshd
`AcceptEnv` lines are removed in phase 4). The parsed values are never
assigned into `process.env`, so children of the agent inherit nothing (T3).

## 6. Failure modes (fail-closed matrix)

| Failure | When | Outcome |
| --- | --- | --- |
| `--secret NAME` with unset/empty host env | admission | usage error; no slot claimed, no boot |
| value contains newline | admission | usage error (blob format integrity) |
| listener bind fails (jail dir missing, perms) | pre-boot | boot aborted; `boot_failed` |
| guest kernel lacks vsock | boot | fetch hook never runs → no ack → `secrets_failed`; prevented earlier by the doctor check (NFR4) |
| old image without the fetch hook | post-`ssh_ready` | no ack within timeout → `secrets_failed`; error text names the likely cause ("image predates vsock secrets?") |
| guest fetch/stage fails mid-write | post-boot | no ack (guest acks only after staging) → `secrets_failed` |
| second connection attempt to the listener | any | connection refused — socket already unlinked |
| ack after timeout fires | race | run already failing closed; listener closed; delivery irrelevant |
| workload crashes before deleting `secrets.env` | workload | file is tmpfs in a disposable VM torn down at cleanup; never collected (outside `/workspace`) |

The invariant reviewers should try to break: **there is no path to
`workload_started` in which a requested secret was not confirmed staged.**

## 7. Layering

| Concern | Layer | Notes |
| --- | --- | --- |
| `PUT /vsock` device config, UDS path in jail layout | `firecracker` | mechanism; mirrors how drives/net are configured |
| one-shot listener (bind, accept-once, serve, ack, unlink) | `transport` | mechanism; knows bytes, not names or meaning |
| secret names, admission validation, env harvesting | `main` (CLI) → `runner` | policy |
| workload gate on delivery, lifecycle emission | `runner`/`main` run flow | policy |
| fetch hook + runner file consumption | rootfs bake (`scripts/`) | guest-side; nothing agent-specific in `src/` |

Dependency direction is preserved: `firecracker`/`transport` gain no imports
from above; the run flow composes them.

## 8. Explicitly out of scope

- Host-initiated pushes over vsock (guest pulls, once).
- A general guest⇄host RPC channel — this is delivery of a static blob; any
  future control channel is its own design.
- Encrypting the blob on the channel (§3).
- Rotating/refreshing secrets mid-run; a room's secrets are fixed at admission.
- `GH_TOKEN` on the push step: it rides a separate post-workload SSH session
  (`push_branch_in_guest`) today, is never exposed to the agent, and moving it
  is a follow-up once the primary keys are migrated.
- Non-cursor runners; `--command` runs can request `--secret` (mechanism is
  runner-agnostic) but wiring their consumption is up to the command.

## 9. Rollout plan

| Phase | Delivers | Gate |
| --- | --- | --- |
| **P1 — mechanism** | vsock device in boot config behind `--secret`; jail-scoped UDS; one-shot listener in `transport` with ack; `secrets_delivered`/`secrets_failed` events; doctor vsock-kernel check; unit tests (listener one-shot-ness, blob format, admission validation) | `make check` green; no image change needed — `secrets_failed` path e2e-testable against the current image (old-image row of §6) |
| **P2 — guest hook** | fetch hook + `socat` in the alpine bake (base builder, so every image variant gets it); rebuilt cursor image on the rooms-host | manual e2e: `--secret` run reaches `secrets_delivered`; staging file present pre-workload with right mode/owner |
| **P3 — consumption + gate** | `cursor-runner.js` file-read + delete + env fallback; workload gate wired; `SendEnv` suppression for delivered names (FR7) | full e2e (§10) passes on the rooms-host |
| **P4 — retire the old path** | remove the `process.env` fallback, the `AcceptEnv CURSOR_API_KEY` bake line, and `SendEnv` of migrated names from `runner.rs` | one dogfood run (pool, cursor) on vsock-only delivery |

P1+P2+P3 can land as one implementation PR if it fits the size band (the
listener and hook are small); P4 is deliberately separate — it deletes the
fallback only after a real run proves the new path.

## 10. Validation gate (e2e, rooms-host only)

One cursor room completes a real task with `--secret CURSOR_API_KEY`, and:

- guest `/proc/cmdline` does not contain the value (T1);
- the agent process tree's `/proc/<pid>/environ` does not contain it (T3);
- `/run/rooms/secrets.env` is gone once the workload is running (T2);
- collected artifacts (`result.json`, `events.ndjson`, `summary.md`,
  `result.patch`) do not contain the value (T4);
- host `ps auxww` does not contain the value (T1);
- a second connect to the vsock port from inside the guest is refused (T5);
- the same invocation against a pre-P2 image fails closed:
  `secrets_failed` emitted, `workload_started` absent, `cleanup_done` present,
  zero leaks (T6);
- `--witness` composes: the same run also yields `witness.pcap`/`witness.json`
  (§2's pairing, observed once, recorded in the PR).

## 11. Open questions

1. **Doctor check depth (NFR4):** parse guest kernel config from the image
   (`ikconfig`), or boot-probe once and cache? Leaning config-parse at
   `doctor` time — static, no boot cost.
2. **`--secret` on `--command` runs:** deliverable now (mechanism is
   runner-agnostic), but is a bare `secrets.env` the right contract for
   arbitrary commands, or should `--command` wait for a consumer?
3. **Default-on for cursor runs:** after P4, should `--runner cursor` imply
   `--secret CURSOR_API_KEY`? Leaning yes (the runner cannot work without
   it), as a separate flip once vsock-only delivery has dogfood mileage.
