# rooms

[![CI](https://github.com/itsHabib/rooms/actions/workflows/ci.yml/badge.svg)](https://github.com/itsHabib/rooms/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Disposable Firecracker microVMs with specified deps. The cold path takes a rootfs image, a repo, and a command; it boots an ephemeral microVM under the Firecracker jailer, SSHes the command into the guest, propagates the exit code, collects `/workspace/out` back to the host, and tears the VM down. The warm path creates one credential-free neutral base, snapshots it, then restores one room or forks up to eight isolated clones from that shared state. The first consumer is an LLM agent (`--runner cursor` drives a baked SDK runner against a cloned repo), but the substrate doesn't know that: it sees "exec a command," same as it would for a test suite or a shell script.

## Status

**v0.1.0 — tagged + public, dogfooded on the rooms-host.** The cold-room path remains intact:

- `rooms run --image <ext4> --command <cmd>` — boot, SSH-exec one command, propagate exit code, auto-shutdown.
- `--runner cursor` — clone `--repo` at `--base-sha`, drive the baked `cursor-runner.js` against `/workspace/repo`, optionally `--push-branch` the result (needs `GH_TOKEN`).
- `--out <hostdir>` — collect the guest's `/workspace/out` (the runner-contract artifact tree) back to the host after the run.
- `rooms collect --from <hostdir>` — validate a collected artifact directory against the runner contract.
- `rooms doctor [--json]` — host-environment checks (KVM, Firecracker + jailer version, dedicated user, TAP, kernel/rootfs, nested virt, checksum drift, `ANTHROPIC_API_KEY`).
- Firecracker runs under the **jailer** as a dedicated unprivileged `firecracker` user (chroot + bind-mounts); the Alpine agent rootfs boots to sshd in ~2 s.

**Phase 2 is implemented on this branch; its exact-head rooms-host killer, review, and Gate remain before it lands.** It adds:

- `base-create` → `snapshot` → `restore` for a credential-free warm base and one hygiene-gated restored room.
- `clone <snapshot> -n 1..8` for bounded concurrent fan-out. Every clone gets its own host network namespace, veth/NAT identity, post-resume identity, fresh SSH host key, and exact teardown; command mode can add per-clone egress enforcement and witness custody.
- A Linux immutable-inode lifecycle for snapshot backing state: the rootfs builder seals its output, snapshot publication seals `snapshot.vmstate`, `snapshot.mem`, `snapshot.json`, and their directory, and restore revalidates the exact immutable inodes before use.
- Crash-recoverable snapshot/restore intents, a persistent snapshot slot reservation with bounded clone leases, and deterministic JSON batch records.

Still separate work: a Nix flake as the deps spec (`--flake`), ship's `backend: "rooms"`, and the Ship `/work-driver` adapter that maps distinct tasks onto one warm fleet. `rooms clone --command` currently broadcasts the **same command** to every clone; eight independently assigned `/work-driver` tasks are not yet a claim this repository makes. See [`docs/features/snapshot-fork-replay/spec.md`](docs/features/snapshot-fork-replay/spec.md) for the Phase-2 contract and [`docs/vision.md`](docs/vision.md) for the wider roadmap.

> **Jailer requires root.** Because Firecracker runs under the jailer (it chroots, bind-mounts the kernel/rootfs, and drops privileges), VM lifecycle commands are normally invoked as `sudo -E rooms …`. `-E` preserves the operator `HOME` and any explicitly requested runtime credentials; neutral-base creation itself admits no secrets.

## Why it exists

Every portfolio tool that needs isolation — an agent runner firing `claude -p`, crash recovery rebuilding a clean checkout, future replay comparing two runs — should not reinvent "boot a VM, run something, collect results." That belongs in one place. `rooms` owns Firecracker control, rootfs preparation, guest transport, command execution, and artifact collection. Callers own *what* runs inside the room; the substrate owns *how* the room exists.

The rest is another layer's job, on purpose — `rooms` stays focused on the microVM lifecycle:

- **Agent logic** — prompt format, SDK wiring, streaming events — lives in the runner script baked into the rootfs, not in the Rust binary. The binary selects a command shape; it does not introspect runners.
- **What "done" means** — the runner contract ([`docs/runner-contract.md`](docs/runner-contract.md)) defines the artifact layout and exit-code → status mapping; runners satisfy it.
- **Orchestration** — fan-out, scheduling, and review live in the consumer (ship / `/work-driver`), which calls `rooms`. `rooms` does not import them; dependency flows one way.

Where the focus ends today (full list + rationale in [`docs/vision.md`](docs/vision.md)): not Codespaces-but-local, no persistent dev workspace or interactive shell-as-product, no web preview / port forwarding, no Docker / devcontainer / generic container runtime, no multi-tenant control plane, no cross-host orchestration. Those are layers other tools own, or that `rooms` adds when a real need shows up — not permanent vetoes. Rooms are ephemeral — a room dies when the command finishes.

## CLI surface

`run` remains the cold create → exec → collect → destroy path. `base-create` → `snapshot` produces a reusable warm source; `restore` consumes it one room at a time, while `clone` fans it out concurrently.

| Verb | What it does |
| --- | --- |
| `run` | Boot a microVM from `--image`, optionally exec into it, then shut down. `--command <cmd>` runs a literal command; `--runner cursor` clones `--repo` at `--base-sha` and drives the baked cursor runner; `--keep` holds the VM open for manual inspection; `--out <dir>` pulls `/workspace/out` back to the host; `--push-branch` pushes the agent's commits (cursor + `GH_TOKEN`). |
| `base-create` | Boot an always-read-only, no-egress base; stage a host-created repo bundle; run an optional credential-free `--warm` command; quiesce to neutral provenance; leave it running for `snapshot`. |
| `snapshot` / `snapshot-recover` | Consume a sealed neutral base into a recoverable Full snapshot, or list/resume an interrupted indexed snapshot transaction. |
| `restore` | Restore one room from `<snapshot-dir>` plus its hash-pinned `--image`. Exactly one of `--keep` or `--command` is required; command mode can collect output, witness egress, deliver named secrets post-resume, and enforce an egress policy. |
| `clone` | Restore `-n 1..8` isolated rooms concurrently. With no `--command`, keep the complete ready batch; with `--command`, broadcast that one command to every clone concurrently and tear the batch down. `--out` collects beneath `<dir>/<room-id>`. |
| `collect` | Validate a collected artifact directory (`--from <dir>`) against the runner contract: required files present, `result.json` parses at `schema_version 1`, referenced paths exist. |
| `diff` | Verify and show the overlay change set collected from a read-only-rootfs run. An indeterminate result is not treated as clean. |
| `ls` / `gc` / `kill` | Inspect liveness, reap only orphaned-dead rooms, or terminate one live/kept room by id. Global `gc` also reconciles stale clone-network resources. |
| `doctor` | Run the host-environment checks and report pass/warn/fail. `--json` emits a versioned machine-readable report on stdout (logs stay on stderr). |

```sh
# boot, run one command in the guest, propagate exit code, shut down (works today)
sudo -E rooms run \
  --image ~/rooms/images/agent-alpine.ext4 \
  --command 'echo "hello from $(uname -srm)"' \
  --out ./run-out
# guest stdout/stderr land in run-out/logs/, exit code propagates to the host,
# result.json records the outcome, microVM destroyed. Add --out to collect logs;
# without it only the exit code surfaces.

# drive an agent against a repo and collect the result.patch (the upper bar — works today)
# --runner cursor needs the cursor variant image (Node + baked cursor-runner.js);
# build it with `--extend scripts/rootfs/install-cursor.sh` (see "Building the rootfs").
sudo -E rooms run \
  --image ~/rooms/images/agent-alpine-cursor.ext4 \
  --runner cursor \
  --repo https://github.com/itsHabib/rooms \
  --task task.md --model composer-2.5 --base-sha <sha> \
  --out ./run-out
rooms collect --from ./run-out   # validate the artifact tree

sudo -E rooms doctor --json      # host readiness, machine-readable
```

Warm snapshot/fork path:

```sh
base_id="$(sudo -E rooms base-create \
  --image ~/rooms/images/agent-alpine.ext4 \
  --repo https://github.com/itsHabib/rooms \
  --warm 'git -C /workspace/repo status --short' \
  --json | jq -r .room_id)"

sudo -E rooms snapshot "$base_id" --out ./rooms.snapshot --json

# One restored workload; the room tears down after the command.
sudo -E rooms restore ./rooms.snapshot \
  --image ~/rooms/images/agent-alpine.ext4 \
  --command 'git -C /workspace/repo fsck --no-progress --strict' \
  --out ./restore-out --json

# Eight isolated copies, all running this same command concurrently.
sudo -E rooms clone ./rooms.snapshot \
  --image ~/rooms/images/agent-alpine.ext4 \
  -n 8 \
  --command 'git -C /workspace/repo fsck --no-progress --strict' \
  --out ./clone-out --json
```

Omit `clone --command` to keep the complete batch alive; the returned JSON gives the room ids for `rooms kill <id>`. Command mode is deliberately a broadcast primitive today. A consumer that needs eight different task commands, outputs, and lifecycle streams must supply the still-follow-up Ship `/work-driver` fleet adapter.

On Linux, the Alpine builder publishes its rootfs with `FS_IMMUTABLE_FL`; snapshot publication applies the same kernel flag to all three artifacts and the snapshot directory. Restore refuses mutable backing and detects inode substitution across its admission and custody boundaries. There is intentionally no snapshot delete/unseal verb yet: published snapshots are operator-retained evidence, and rerunning the rootfs builder is the one supported exact-output replacement path.

`--keep` and `--command` are mutually exclusive on `run`/`restore`; kept modes cannot collect output or witness traffic; `--push-branch` is cursor-only. clap enforces these combinations at parse time.

## Prereqs

- **Host:** Linux with `/dev/kvm` (nested virt enabled if running inside a VM). The v0 dev loop uses an Ubuntu Server VM under Hyper-V on Windows (`rooms-host`).
- **Firecracker + jailer:** installed on the host (see [`scripts/setup-rooms-host.sh`](scripts/setup-rooms-host.sh)). Pinned versions are verified by sha256 against [`scripts/checksums.txt`](scripts/checksums.txt).
- **Images:** a Firecracker-tuned kernel (`vmlinux.bin`) + an agent rootfs (`.ext4`) as siblings under e.g. `~/rooms/images/`. Built on the host (gitignored), not committed. Snapshot/restore requires a builder-published, immutable rootfs whose hash matches `snapshot.json`.
- **SSH key:** `~/.ssh/id_rooms` must match the public key passed to the rootfs builder. The agent runs as the unprivileged `rooms` user (`ssh -i ~/.ssh/id_rooms rooms@172.16.0.2`).
- **Network:** install the host substrate with `sudo bash scripts/setup-tap.sh --host`. It creates the flat-room `ROOMS_FWD` path and the clone `ROOMS_VETH_FWD` path used by per-clone namespaces/veths.
- **Immutable inode support:** snapshot/fork needs a Linux filesystem that implements `FS_IMMUTABLE_FL`, plus `chattr`/`lsattr` and sufficient privilege to set and verify it. Admission fails closed when the flag cannot be enforced.
- **API key (for agent runs):** `ANTHROPIC_API_KEY` (or `CURSOR_API_KEY` for the cursor runner) in the operator shell; `sudo -E` forwards it into `rooms`.
- **Build:** Rust stable (`rustup`); `make check` passes.

Full host bootstrap: [`scripts/provision-hyperv.ps1`](scripts/provision-hyperv.ps1) (Windows) → [`scripts/setup-rooms-host.sh`](scripts/setup-rooms-host.sh) (in-VM).

## Architecture

Strict one-directional layering; consumers compose the binary, the binary does not import consumers.

```
                        ┌──────────────────────────────────┐
  ship / work-driver ──▶│  rooms  (this repo, Linux+KVM)    │
  (callers; not          │                                  │
   imported back)        │  main ── clap CLI, wiring        │
                         │   │                              │
                         │  runner / snapshot_exec /        │
                         │  restore_exec ── cold + warm flow│
                         │   │                              │
                         │  firecracker / rootfs / transport│
                         │  clonenet / egress / witness     │
                         │   │   jail, overlay, bundle, net │
                         │  domain ── plain types, no I/O   │
                         └───┬──────────────────────────────┘
                             ▼
                    Firecracker microVM (ephemeral, one per room)
                      /workspace/repo  — git checkout from host bundle
                      /workspace/out   — artifacts collected back
```

| Module | Responsibility |
| --- | --- |
| `domain` (`config`, `error`) | Plain types, config defaults, error enums; no I/O. |
| `firecracker` | Process spawn under the jailer, API socket, VM config, boot/shutdown, cleanup guard. |
| `rootfs` / `inode_seal` | Image + kernel admission; Linux immutable-inode set/verify mechanics for snapshot backing state. |
| `transport` | Repo bundle + SCP into/out of the guest. |
| `runner` | Namespace-aware SSH exec, guest readiness probe, runner selection (`command` / `cursor`), artifact capture. |
| `snapshot` / `snapshot_exec` | Snapshot metadata plus the recoverable neutral-base → immutable Full-snapshot transaction. |
| `restore` / `restore_exec` | Snapshot admission, exact-inode revalidation, jail staging, restore hygiene gate, and custody transfer/teardown. |
| `slot` / `indexed_claim` / `clonenet` | Persistent snapshot leases and exact-owner allocation/reconciliation of per-clone namespaces, veths, routes, and NAT. |
| `egress` / `witness` | Per-room host-side egress enforcement and pcap/receipt evidence, including namespace-scoped clones. |
| `artifacts` | Runner-contract `result.json` + artifact-tree load/validation. |
| `doctor` | Host environment checks. |
| `main` | clap CLI; wires the layers; dispatch + signal handling. |

Don't introduce a downward import. If a feature needs a new dependency direction, lift the shared concern into `domain`.

## Develop

```sh
make check        # fmt-check + clippy --all-targets --all-features -- -D warnings + test
make fmt          # apply rustfmt
make lint         # clippy strict (no fix)
make test         # unit tests only (no Firecracker required)
make build        # debug build
make release      # release build
```

`make check` is the single command CI runs and you run before push. E2e tests (`cargo test --features e2e`) require Firecracker + KVM + images on the rooms-host; CI intentionally skips them.

On the rooms-host, the normal privileged integration gate exercises the pool and one real neutral-base → snapshot → clone path and checks for host leaks:

```sh
sudo -E make e2e
```

The Phase-2 killer is the stricter eight-clone acceptance gate. Run it as the ordinary rooms-host operator (it uses passwordless `sudo -n` only for privileged operations):

```sh
./scripts/phase2-killer.sh
```

It builds the current source and a fresh immutable rootfs under a unique proof `HOME`, publishes one neutral snapshot, measures the one-clone baseline, demands eight workload-ready clones in literally less than one second with aggregate PSS below twice that baseline, then verifies namespace/NAT isolation, post-resume hygiene, eight witnessed workloads, exact teardown, and zero proof-owned leaks. The preserved proof root under `~/.r2/` contains `summary.json`, hashes, pcaps, and logs. A pass proves the Rooms broadcast-fleet substrate; it explicitly does not prove distinct-task `/work-driver` integration.

### Building the rootfs

The agent guest image is built on the rooms-host (not committed to git). The base image is **Alpine** (musl/busybox/openrc) with the claude-code native binary, paired with a Firecracker-tuned virtio-rng kernel — it boots to sshd in ~2 s and is ~276 MB:

```sh
sudo ./scripts/build-rootfs-alpine.sh \
  --out images/agent-alpine.ext4 \
  --ssh-key ~/.ssh/id_rooms.pub
```

The builder seals the finished image immutable and verifies the flag before returning. Re-running it for the same regular-file `--out` is the explicit replacement path: it clears only that exact output's flag, rebuilds atomically, and seals the new inode. Do not manually unseal an image while a snapshot or restore may reference it.

The base image carries no Node and no cursor runner. `--runner cursor` needs the cursor variant, built by adding the `--extend` hook (which installs Node + a pinned `@cursor/sdk` and bakes `cursor-runner.js` at `/opt/rooms/cursor-runner/`):

```sh
sudo ./scripts/build-rootfs-alpine.sh \
  --out images/agent-alpine-cursor.ext4 \
  --size 1G \
  --ssh-key ~/.ssh/id_rooms.pub \
  --extend scripts/rootfs/install-cursor.sh
```

Boot-test with [`scripts/test-rootfs-alpine.sh`](scripts/test-rootfs-alpine.sh). The older Ubuntu-noble debootstrap builder ([`scripts/build-rootfs.sh`](scripts/build-rootfs.sh)) remains available. See [`scripts/README.md`](scripts/README.md) for prereqs, the kernel, sha256 verification, and the `--extend` hook.

**PR conventions:** request Copilot review; comment `@codex review`, `@claude review`, and `@cursor review`. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the contributor onramp and [`CLAUDE.md`](CLAUDE.md) for sizing bands and lint discipline.

## CI

GitHub Actions, every PR:

- `fmt` and `clippy -D warnings` on `ubuntu-latest`.
- `test` matrix on `ubuntu-latest` + `windows-latest` (no `--features e2e` — e2e needs real Firecracker on the rooms-host).
- `audit` via [`rustsec/audit-check`](https://github.com/rustsec/audit-check) on `Cargo.lock`.
- Bot reviews: `@claude review` triggers [`.github/workflows/claude.yml`](.github/workflows/claude.yml); Cursor Bugbot runs automatically.

Manually dispatched via `workflow_dispatch`: [`coverage.yml`](.github/workflows/coverage.yml) (cargo-llvm-cov), [`mutants.yml`](.github/workflows/mutants.yml) (cargo-mutants).

Locally, `make check` mirrors the PR jobs.

## Docs

| Doc | Purpose |
| --- | --- |
| [`docs/vision.md`](docs/vision.md) | What / why / non-goals / roadmap — operator-facing. |
| [`docs/features/rooms-v0/spec.md`](docs/features/rooms-v0/spec.md) | v0 contract: lifecycle, host layout, crate layers — read first. |
| [`docs/runner-contract.md`](docs/runner-contract.md) | Artifact layout + `result.json` schema + exit-code → status mapping. |
| [`docs/features/<slug>/spec.md`](docs/features/) | One spec per productionization task. |
| [`docs/follow-ups.md`](docs/follow-ups.md) | Out-of-scope discoveries deferred from in-progress work. |

## License

MIT.
