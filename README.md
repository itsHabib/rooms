# rooms

[![CI](https://github.com/itsHabib/rooms/actions/workflows/ci.yml/badge.svg)](https://github.com/itsHabib/rooms/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Disposable Firecracker microVMs with specified deps. You hand `rooms` a rootfs image, a repo, and a command; it boots an ephemeral microVM under the Firecracker jailer, SSHes a command into the guest, propagates the exit code, collects `/workspace/out` back to the host, and tears the VM down. One room, one command, one outcome — no state shared between runs. The first consumer is an LLM agent (`--runner cursor` drives a baked SDK runner against a cloned repo), but the substrate doesn't know that: it sees "exec a command," same as it would for a test suite or a shell script.

## Status

**v0.1.0 — tagged + public, dogfooded on the rooms-host.** Shipped today:

- `rooms run --image <ext4> --command <cmd>` — boot, SSH-exec one command, propagate exit code, auto-shutdown.
- `--runner cursor` — clone `--repo` at `--base-sha`, drive the baked `cursor-runner.js` against `/workspace/repo`, optionally `--push-branch` the result (needs `GH_TOKEN`).
- `--out <hostdir>` — collect the guest's `/workspace/out` (the runner-contract artifact tree) back to the host after the run.
- `rooms collect --from <hostdir>` — validate a collected artifact directory against the runner contract.
- `rooms doctor [--json]` — twelve host-environment checks (KVM, Firecracker + jailer version, dedicated user, TAP, kernel/rootfs, nested virt, checksum drift, `ANTHROPIC_API_KEY`).
- Firecracker runs under the **jailer** as a dedicated unprivileged `firecracker` user (chroot + bind-mounts); the Alpine agent rootfs boots to sshd in ~2 s.

In flight / not yet built: a Nix flake as the deps spec (`--flake`), ship's `backend: "rooms"` integration, snapshots/fork, and hard multi-room parallelism. See [`docs/vision.md`](docs/vision.md) for the roadmap and [`docs/features/rooms-v0/spec.md`](docs/features/rooms-v0/spec.md) for the v0 design.

> **Jailer requires root.** Because Firecracker runs under the jailer (it chroots, bind-mounts the kernel/rootfs, and drops privileges), `rooms run` must be invoked as `sudo -E rooms run …`. `-E` preserves `HOME`, `GH_TOKEN`, and `ANTHROPIC_API_KEY` from the operator shell.

## Why it exists

Every portfolio tool that needs isolation — an agent runner firing `claude -p`, crash recovery rebuilding a clean checkout, future replay comparing two runs — should not reinvent "boot a VM, run something, collect results." That belongs in one place. `rooms` owns Firecracker control, rootfs preparation, guest transport, command execution, and artifact collection. Callers own *what* runs inside the room; the substrate owns *how* the room exists.

The rest is another layer's job, on purpose — `rooms` stays focused on the microVM lifecycle:

- **Agent logic** — prompt format, SDK wiring, streaming events — lives in the runner script baked into the rootfs, not in the Rust binary. The binary selects a command shape; it does not introspect runners.
- **What "done" means** — the runner contract ([`docs/runner-contract.md`](docs/runner-contract.md)) defines the artifact layout and exit-code → status mapping; runners satisfy it.
- **Orchestration** — fan-out, scheduling, and review live in the consumer (ship / `/work-driver`), which calls `rooms`. `rooms` does not import them; dependency flows one way.

Where the focus ends today (full list + rationale in [`docs/vision.md`](docs/vision.md)): not Codespaces-but-local, no persistent dev workspace or interactive shell-as-product, no web preview / port forwarding, no Docker / devcontainer / generic container runtime, no multi-tenant control plane, no cross-host orchestration. Those are layers other tools own, or that `rooms` adds when a real need shows up — not permanent vetoes. Rooms are ephemeral — a room dies when the command finishes.

## CLI surface

Three verbs. `run` composes the create → exec → collect → destroy lifecycle for the common case; `collect` and `doctor` stand alone.

| Verb | What it does |
| --- | --- |
| `run` | Boot a microVM from `--image`, optionally exec into it, then shut down. `--command <cmd>` runs a literal command; `--runner cursor` clones `--repo` at `--base-sha` and drives the baked cursor runner; `--keep` holds the VM open for manual inspection; `--out <dir>` pulls `/workspace/out` back to the host; `--push-branch` pushes the agent's commits (cursor + `GH_TOKEN`). |
| `collect` | Validate a collected artifact directory (`--from <dir>`) against the runner contract: required files present, `result.json` parses at `schema_version 1`, referenced paths exist. |
| `doctor` | Run twelve host-environment checks and report pass/warn/fail. `--json` emits a versioned machine-readable report on stdout (logs stay on stderr). |

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

`--keep` and `--command` are mutually exclusive; `--keep` and `--out` conflict; `--push-branch` is cursor-only. clap enforces these at parse time.

## Prereqs

- **Host:** Linux with `/dev/kvm` (nested virt enabled if running inside a VM). The v0 dev loop uses an Ubuntu Server VM under Hyper-V on Windows (`rooms-host`).
- **Firecracker + jailer:** installed on the host (see [`scripts/setup-rooms-host.sh`](scripts/setup-rooms-host.sh)). Pinned versions are verified by sha256 against [`scripts/checksums.txt`](scripts/checksums.txt).
- **Images:** a Firecracker-tuned kernel (`vmlinux.bin`) + an agent rootfs (`.ext4`) as siblings under e.g. `~/rooms/images/`. Built on the host (gitignored), not committed.
- **SSH key:** `~/.ssh/id_rooms` baked into the rootfs ([`scripts/bake-rootfs-ssh.sh`](scripts/bake-rootfs-ssh.sh)). The agent runs as the unprivileged `rooms` user (`ssh -i ~/.ssh/id_rooms rooms@172.16.0.2`).
- **Network:** a TAP device ([`scripts/setup-tap.sh`](scripts/setup-tap.sh)).
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
                         │  runner ── SSH exec, artifacts   │
                         │   │                              │
                         │  firecracker / rootfs / transport│
                         │   │   boot, jail, overlay, bundle│
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
| `rootfs` | Image + kernel path resolution and validation. |
| `transport` | Repo bundle + SCP into/out of the guest. |
| `runner` | SSH exec, guest readiness probe, runner selection (`command` / `cursor`), artifact capture. |
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

### Building the rootfs

The agent guest image is built on the rooms-host (not committed to git). The base image is **Alpine** (musl/busybox/openrc) with the claude-code native binary, paired with a Firecracker-tuned virtio-rng kernel — it boots to sshd in ~2 s and is ~276 MB:

```sh
sudo ./scripts/build-rootfs-alpine.sh \
  --out images/agent-alpine.ext4 \
  --ssh-key ~/.ssh/id_rooms.pub
```

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
