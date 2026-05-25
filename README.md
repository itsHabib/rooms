# rooms

Disposable Firecracker microVMs with specified deps — the substrate that turns "run a command in a clean isolated env with a real repo" into one CLI invocation. First consumer is an LLM agent; the substrate doesn't know that.

## Status

**v0 POC — in flight.** Shipped today: boot a microVM from a prebuilt rootfs, SSH in, run a single `--command`, propagate exit code, shut down. The m4 milestone demonstrated outbound HTTPS from inside the guest (Anthropic API via curl). Not yet shipped: primitive verbs (`create` / `exec` / `collect` / `destroy`), `rooms run --repo --task`, artifact collection, and a real `doctor`. See [`docs/features/rooms-v0/spec.md`](docs/features/rooms-v0/spec.md) for the target design and [`docs/features/01-productionization/driver.md`](docs/features/01-productionization/driver.md) for the post-POC work plan.

## Prereqs

- **Host:** Linux with `/dev/kvm` (nested virt enabled if running inside a VM). v0 dev loop uses an Ubuntu Server VM under Hyper-V on Windows (`rooms-host`).
- **Firecracker:** installed on the host (see [`scripts/setup-rooms-host.sh`](scripts/setup-rooms-host.sh)).
- **Images:** quickstart kernel (`vmlinux.bin`) + rootfs (`rootfs.ext4`) as siblings under e.g. `~/rooms/images/`.
- **SSH key:** `~/.ssh/id_rooms` baked into the rootfs ([`scripts/bake-rootfs-ssh.sh`](scripts/bake-rootfs-ssh.sh)).
- **Network:** TAP device ([`scripts/setup-tap.sh`](scripts/setup-tap.sh)).
- **API key (for agent runs):** `ANTHROPIC_API_KEY` in the operator shell for Claude Code / curl POC; future cursor SDK runner will use `CURSOR_API_KEY`.
- **Build:** Rust stable (`rustup`), `make check` passes.

Full host bootstrap: [`scripts/provision-hyperv.ps1`](scripts/provision-hyperv.ps1) (Windows) → [`scripts/setup-rooms-host.sh`](scripts/setup-rooms-host.sh) (in-VM).

## Quickstart

On a configured `rooms-host`:

```sh
# build
git clone https://github.com/itsHabib/rooms ~/rooms
cd ~/rooms && make check

# boot, run one command in the guest, shut down (works today)
export ANTHROPIC_API_KEY=sk-ant-...
cargo run -- run \
  --image ~/rooms/images/rootfs.ext4 \
  --command 'curl -s https://api.anthropic.com/v1/messages -H "x-api-key: $ANTHROPIC_API_KEY" ...'

# expected: JSON response on stdout, exit code 0, microVM destroyed
```

**POC upper bar (target, not yet one command):**

```sh
rooms run --repo ~/my-project --task task.md
# → microVM boots, repo at /workspace/repo, claude -p runs, result.patch on host
```

## CLI surface

| Verb | Description | Status |
| --- | --- | --- |
| `run` | Convenience: create + exec + collect + destroy. Today: `--image` + optional `--command` or `--keep`. Target: `--repo` + `--task`. | partial |
| `create` | Allocate a room, prepare rootfs overlay, boot microVM; prints `room_id`. | planned |
| `exec` | Run a command in an existing room; capture stdout/stderr/exit. | planned |
| `collect` | Pull `/workspace/out` from guest to host. | planned |
| `destroy` | Halt microVM, reap process, remove work dir. | planned |
| `doctor` | Check `/dev/kvm`, Firecracker binary, image paths, TAP. | stub |

```sh
rooms run --help
rooms doctor
```

## Develop

```sh
make check        # fmt-check + clippy --all-targets -- -D warnings + test
make fmt          # apply rustfmt
make lint         # clippy strict
make test         # unit tests (no Firecracker required)
make build        # debug build
```

Specs live at [`docs/features/<slug>/spec.md`](docs/features/). One spec per productionization task; read [`docs/features/rooms-v0/spec.md`](docs/features/rooms-v0/spec.md) first.

**PR conventions:** request Copilot review; comment `@codex review` and `@claude review` on the PR. See [`CLAUDE.md`](CLAUDE.md) for sizing bands and lint discipline.

## CI

GitHub Actions on `ubuntu-latest`: `cargo fmt --check`, `clippy -D warnings`, `cargo test` (no `--features e2e` — e2e needs real Firecracker on the rooms-host). Comment `@claude review` on a PR to trigger [`.github/workflows/claude.yml`](.github/workflows/claude.yml).

Locally, `make check` mirrors CI.

## Architecture

High-level vision and non-goals: [`docs/vision.md`](docs/vision.md).

v0 design (lifecycle, host layout, crate layers): [`docs/features/rooms-v0/spec.md`](docs/features/rooms-v0/spec.md).

## License

MIT.
