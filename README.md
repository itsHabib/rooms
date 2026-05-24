# rooms

Disposable Firecracker microVMs with specified deps — the substrate that turns "run a command in a clean isolated env with a real repo" into one CLI invocation. First consumer is an LLM agent; the substrate doesn't know that.

> **Status: v0 scaffold.** POC in flight. See [`docs/features/rooms-v0/spec.md`](docs/features/rooms-v0/spec.md) for the current design, [`docs/features/01-productionization/driver.md`](docs/features/01-productionization/driver.md) for the post-POC work plan.

A full README (vision, quickstart, CLI walkthrough, architecture) lands as part of [task #8 `docs-vision-and-readme`](docs/features/docs-vision-and-readme/spec.md). This is the holding stub.

## Develop

```sh
make check        # fmt-check + clippy --all-targets -- -D warnings + test
make fmt          # apply rustfmt
make lint         # clippy strict (no fix)
make test         # cargo test
make build        # debug build
make release      # release build
```

## CLI (POC)

```sh
rooms run --image ~/rooms/images/rootfs.ext4          # boot + auto-shutdown after 3s
rooms run --image ~/rooms/images/rootfs.ext4 --keep   # boot until Ctrl-C
rooms run --image ~/rooms/images/rootfs.ext4 \
    --command 'curl -s https://example.com'           # boot, ssh in, run cmd, shut down
rooms doctor                                          # host env check (stub)
```

`--command` and `--keep` are mutually exclusive. The command runs in the guest's bash via SSH; stdout / stderr flow to the host's stdout / stderr and the guest's exit code becomes rooms's exit code. `ANTHROPIC_API_KEY` is forwarded into the guest via SSH's `SendEnv` if set in the operator's shell.

## Prereqs

`rooms` is Linux + KVM only. v0 host setup: an Ubuntu Server VM under Hyper-V with nested virtualization enabled (`rooms-host`).

Bootstrap on Windows:

```powershell
.\scripts\provision-hyperv.ps1 -VMName rooms-host -IsoPath <path-to-ubuntu-server.iso>
# walk through the Ubuntu installer, SSH in, then:
```

Inside the Ubuntu VM:

```sh
bash scripts/setup-rooms-host.sh         # firecracker, kernel, rootfs, rust, node, claude-code
bash scripts/bake-rootfs-ssh.sh          # bake SSH pubkey into rootfs (one-time, needs sudo)
bash scripts/setup-tap.sh                # TAP device + NAT + IP forwarding (one-time, needs sudo)
```

The first installs Firecracker, the quickstart kernel + rootfs, Rust, Node + claude-code CLI, and verifies `/dev/kvm`. The second loop-mounts the quickstart rootfs and bakes an ed25519 pubkey into `/root/.ssh/authorized_keys` so a booted microVM accepts pubkey SSH at `172.16.0.2` (generates `~/.ssh/id_rooms` on first run; shut down any running microVM first). Use the dedicated key explicitly when connecting:

```sh
ssh -i ~/.ssh/id_rooms -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=accept-new root@172.16.0.2
```

The `-i` flag is mandatory because the key isn't at the default `~/.ssh/id_*` paths OpenSSH tries automatically; `UserKnownHostsFile=/dev/null` avoids accumulating host-key entries (the microVM regenerates host keys each boot until task #6 ships a proper rootfs builder).

The third creates the `tap-fc0` interface that microVMs use for networking (POC: one shared TAP; per-room dynamic TAPs are task #2 hardening). Teardown via `bash scripts/teardown-tap.sh`.

## Where things live

- [`docs/features/rooms-v0/spec.md`](docs/features/rooms-v0/spec.md) — v0 design (read first).
- [`docs/features/01-productionization/driver.md`](docs/features/01-productionization/driver.md) — post-POC work plan (consumed by `/work-driver`).
- [`docs/features/<task>/spec.md`](docs/features/) — one spec per productionization task.
- [`scripts/`](scripts/) — host setup + (eventually) rootfs builder.
- [`src/`](src/) — Rust source (clap CLI + Firecracker control + rootfs + transport + runner + artifacts).

## License

MIT.
