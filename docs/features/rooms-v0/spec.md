# rooms v0

## Status

Active. Drafted 2026-05-23 from the firecracker-ideas spike + design conversation. Successor to `pers/firecracker-ideas/tower-room-v0-codex.md` — same problem space, sharper framing, new repo (not a tower extension).

## Predecessors

- `pers/firecracker-ideas/tower-room-v0-codex.md` — Codex's tower-extension framing of the v0. Superseded; same lifecycle pattern survives, packaging + framing differ.
- `pers/firecracker-ideas/firecracker-codex.md` — Codex's top-5; #1 = Tower Rooms.
- `pers/firecracker-ideas/firecracker-claude.md` — Claude's top-5; #1 = Tower as snapshot substrate.

Both shortlists converged on "this is the foundation move; everything else downstream." This doc is the build plan for that foundation under a new name.

## Thesis

The primitive is:

> **Spawn a clean Firecracker microVM with specified deps, run a command in it, collect artifacts, destroy it.**

An LLM agent is the first consumer. Other consumers (ship's `RoomCursorRunner`, `/work-driver` crash recovery, future replay, future human interactive use) compose this primitive without it knowing about them. Agent invocation is `rooms exec <id> -- claude -p < task.md`; the substrate sees `exec a command`, not `run an agent`.

This is a sharper layering than the predecessor doc, which baked `--agent` into the substrate. Substrate vs. consumer stays clean here.

## Why a new repo (not tower)

Tower is mid-deprecation as a worktree tracker (`feedback_use_worktree_skills`, 2026-05-19). Reusing the name would muddy both: the worktree code is fading, the rooms code is new and unrelated. A fresh repo:

- skips tower's deprecated worktree paths
- names what the thing actually is (the primitive, not the predecessor)
- starts with current portfolio conventions (dossier-style lints + CI, no inherited drift)
- lets tower keep passively deprecating per the existing plan

Cost: one more dir in `pers/`. Worth it.

## Why Rust

The destination is Firecracker + (eventually) snapshot/fork primitives + a content-addressable rootfs store. Firecracker is Rust; `rust-vmm` crates are Rust; the adjacent ecosystem (Cloud Hypervisor, crosvm, Kata pieces) is Rust. Starting in Rust avoids a future rewrite. Go would ship v0 faster but pay a rewrite tax at the substrate-deepening step.

Lint and test discipline mirror `pers/dossier` (which is also Rust and recently set the portfolio bar). See "Code conventions" below.

## Goals (v0)

1. Boot a Firecracker microVM from a Rust binary running on a Linux+KVM host.
2. Land a git repo at `/workspace/repo` inside the microVM.
3. Run a command in the microVM (POC consumer: `claude -p < task.md`).
4. Capture stdout/stderr, exit code, and a `result.patch` describing what the command changed in the repo.
5. Tear the microVM down cleanly.
6. All of the above driven from one `rooms` CLI invocation.

## Non-goals (v0)

- Snapshot / fork / replay — deferred to v0.1+.
- Multiple deps profiles — v0 hardcodes one prebuilt rootfs.
- Nix flake input — deferred to v0.1+. v0 uses a handbuilt Ubuntu rootfs.
- Ship backend integration (`backend: "rooms"` in `mcp__ship__ship`) — deferred to v0.1.
- Cursor SDK runner inside the microVM — deferred to v0.1+. POC uses `claude -p` (Claude Code CLI), one binary + one env var.
- Human-facing UX: interactive shell, port forwarding, web preview, persistent rooms across reboots, multi-user. Resist; this is where the Codespaces gravity well lives.
- Cross-host control plane — `rooms` runs in the same Ubuntu VM that hosts Firecracker. No SSH-from-Windows-to-VM control logic in v0.
- Backend trait abstraction — single concrete `Firecracker` backend. A trait can land when there is a second consumer of the seam (e.g. a fake for tests, a non-Firecracker target). Not before.
- Replay receipts in the Codex sense (memory state). Artifact collection only.

## Architecture

```
Host: Windows 11 Pro
└── Hyper-V (built-in, free)
    └── Ubuntu Server 24.04 VM — "rooms-host"
        ├── nested virt on, /dev/kvm present
        ├── Firecracker binary + quickstart kernel + rootfs
        ├── Rust toolchain
        ├── git, node, claude-code CLI in the rootfs
        ├── rooms binary (this repo) — dev happens here
        │
        └── Firecracker microVM (per room, ephemeral)
            ├── kernel + per-room rootfs overlay (CoW)
            ├── /workspace/repo  (git clone from bundle)
            ├── /workspace/out   (artifact dir collected back to host)
            └── command being executed (POC: claude -p < task.md)
```

`rooms` runs **inside** the Ubuntu VM, not on Windows. The dev loop is `ssh user@rooms-host`, then `cargo run` in the rooms repo checked out there. Cross-machine control (rooms-on-Windows talking to firecracker-in-VM) is a v1+ concern.

## Host setup (one-time)

On Windows host:

```powershell
# 1. Enable Hyper-V (may require reboot)
Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V -All

# 2. Create the VM (script in this repo: scripts/provision-hyperv.ps1)
.\scripts\provision-hyperv.ps1 -VMName rooms-host -MemoryGB 8 -VCpus 4 -DiskGB 80

# 3. Attach Ubuntu Server 24.04 ISO, boot, walk through installer (~15 min)

# 4. Shut down VM, enable nested virt
Set-VMProcessor -VMName rooms-host -ExposeVirtualizationExtensions $true

# 5. Boot VM, get its IP, SSH in
```

Inside the Ubuntu VM (script in this repo: `scripts/setup-rooms-host.sh`):

```sh
# Verify nested virt landed
ls /dev/kvm

# Install Firecracker (single binary release)
# Download quickstart kernel (vmlinux) + rootfs (ubuntu.ext4)
# Install Rust via rustup
# Install Node + npm
# Install claude-code: npm install -g @anthropic-ai/claude-code
# Set ANTHROPIC_API_KEY in ~/.bashrc

# Clone this repo
git clone <rooms-repo-url> ~/rooms
cd ~/rooms
make check  # confirms toolchain works
```

The Hyper-V VM provisioning is ~30 minutes of human time (mostly Ubuntu installer). Inside-VM setup is ~10 minutes of script time.

## CLI surface (v0)

Primitive verbs — substrate primitives, consumer-agnostic:

```
rooms create --image <name> --repo <path>     → prints room_id
rooms exec   <room_id> -- <command...>        → captures stdout/stderr/exit
rooms collect <room_id> --to <host-dir>       → extracts /workspace/out
rooms destroy <room_id>
rooms doctor                                  → check /dev/kvm, Firecracker, image
```

Convenience verb — composes the primitives, for the POC happy-path:

```
rooms run --repo <path> --task <spec.md>      → create + exec(claude -p) + collect + destroy
```

`rooms run` is sugar. The primitives are the canonical interface; `run` exists so the POC end-to-end is one command instead of four.

## Per-room lifecycle

1. `rooms create`
   - Allocate `room_id` (ULID).
   - Per-room work dir on the host: `~/.local/state/rooms/<room_id>/`.
   - Prepare rootfs overlay (copy base ext4 → per-room overlay).
2. Prepare repo payload
   - On the rooms-host: `git -C <repo> bundle create <work-dir>/repo.bundle <base-ref>`.
   - Mount the per-room overlay; drop `repo.bundle` + task doc at known paths.
   - Unmount.
3. Boot Firecracker
   - Write VM config JSON.
   - `firecracker --api-sock <sock>`.
   - POST kernel, rootfs, network, vcpu, mem config to the API socket.
   - InstanceStart.
4. Wait for guest reachability
   - POC: SSH (well-trodden, debuggable).
   - v0.1+: consider vsock JSON-RPC for the command channel.
5. Inside guest, on first contact
   - `git clone /mnt/payload/repo.bundle /workspace/repo`.
   - Checkout the requested ref.
6. `rooms exec`
   - SSH (or vsock) into the guest, run the command, redirect stdout/stderr to `/workspace/out/logs/`.
   - Capture exit code.
7. Artifact export inside guest
   - `git -C /workspace/repo diff --binary <base-sha>...HEAD > /workspace/out/result.patch`.
   - `git -C /workspace/repo log --oneline <base-sha>..HEAD > /workspace/out/commits.txt`.
8. `rooms collect`
   - scp (or vsock-copy) `/workspace/out` → `~/.local/state/rooms/<room_id>/out/` on the host.
9. `rooms destroy`
   - Send InstanceHalt to Firecracker.
   - Reap Firecracker process.
   - rm the per-room overlay + work dir (unless `--keep` was passed).

`rooms run` calls all of these in sequence with auto-cleanup on error.

## POC scope (tonight — upper bar)

Definition of done:

> `rooms run --repo <path> --task <task.md>` produces:
> - microVM boots cleanly inside the Ubuntu Hyper-V VM
> - repo lands at `/workspace/repo` inside the guest
> - `claude -p < task.md` runs to completion
> - exit code captured on the host
> - `result.patch` retrievable on the host
> - microVM destroyed cleanly
> All driven by one CLI invocation, end-to-end.

If we hit this tonight, v0 is real. Productionization is the post-POC work-driver-prep batch below.

## Open implementation choices (decide as we hit them)

**Firecracker control.** Two paths:
- Shell out to the `firecracker` binary + write JSON config files + POST to the API socket via `reqwest` over Unix-socket. Simple, debuggable, no dependency on a less-maintained crate.
- Use `firec` or `firepilot` Rust crates. Typed, fewer moving parts, but neither is dominant in the ecosystem yet.

POC: shell out. Re-evaluate if the API surface gets unwieldy.

**Repo transport into guest.** Three options:
- git bundle on a second virtio-block disk, mounted in guest.
- vsock file copy.
- TAP network interface + `scp`.

POC: TAP + `scp` is simplest because we need network anyway (claude-code needs egress to api.anthropic.com).

**Command channel.** SSH vs vsock JSON-RPC vs serial console.

POC: SSH. Well-trodden, easy to debug from the host with the same key.

**Networking.** TAP interface bridged to the rooms-host's network, full egress. v0-acceptable. Hardening (egress allowlist, separate bridge per room) is v0.1+.

## Code conventions (mirror dossier)

Lint discipline:

- `unsafe_code = "forbid"`
- `clippy::all`, `pedantic`, `nursery`, `cargo` all `warn` (priority -1)
- Restriction lints (selective): `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `indexing_slicing`, `dbg_macro`, `print_stdout`, `str_to_string`, `get_unwrap`
- `clippy.toml`: cognitive 20, lines 100, args 6
- `rustfmt.toml`: edition 2021, max_width 100

Build/test:

- `Makefile` with `make check` = `fmt-check + lint + test`
- CI on `ubuntu-latest`: fmt-check, clippy `-D warnings`, test
- Claude review workflow mirroring dossier's `.github/workflows/claude.yml`

Repo conventions:

- Errors lowercase (`bail!("no /dev/kvm; nested virt off?")`)
- No `#[allow(...)]` without a one-line justification comment
- Test modules can `#![allow(...)]` the panicky lints at the top of `mod tests`
- No design-doc / phase refs in code comments — doc comments describe behavior; roadmap context belongs in commit messages and spec docs

PR conventions:

- Reviewers per PR: Copilot, `@codex review`, `@claude review`
- PR sizing bands: amazing < 500, ideal < 700, stretch < 1000 weighted LOC
- Weights: source 1.0×, tests 0.5×, configs/lockfiles/docs 0×
- Spec doc declares the budget; > 700 must split or justify inline

## Crate layout

```
rooms/
  Cargo.toml
  Makefile
  clippy.toml
  rustfmt.toml
  CLAUDE.md
  README.md
  scripts/
    provision-hyperv.ps1       — Windows side: create the rooms-host VM
    setup-rooms-host.sh        — In-VM: install firecracker + toolchain
  src/
    main.rs                    — clap CLI entry
    lib.rs                     — re-exports
    domain.rs                  — RoomId, RoomImage, RunOutcome plain types (no I/O)
    firecracker.rs             — Firecracker process + API control
    rootfs.rs                  — overlay/CoW management
    transport.rs               — bundle the repo, scp into guest
    runner.rs                  — exec a command in the guest, capture artifacts
    artifacts.rs               — host-side result dir layout
  docs/
    features/
      rooms-v0/
        spec.md                — THIS DOC
  tests/
    smoke.rs                   — end-to-end against real Firecracker (env-gated)
  .github/
    workflows/
      ci.yml
      claude.yml
```

Strict layered dependency direction (same shape as dossier / tower):

```
domain → firecracker / rootfs / transport → runner → main
```

`domain` has no I/O. Helpers don't import upward. If a feature wants a new dependency direction, lift the shared concern into `domain`.

## Work-driver-prep tasks (post-POC productionization)

Once the POC's upper bar is met, these become spec docs under `docs/features/<slug>/spec.md` and fed to `/work-driver-prep`:

| # | slug | what | depends on |
|---|---|---|---|
| 1 | `ci-and-claude-workflow` | full `make check` matrix + ci.yml + claude.yml (mirrors dossier) | — |
| 2 | `harden-firecracker-control` | error handling, timeouts, lifecycle cleanup on every failure path, `rooms doctor` real checks | — |
| 3 | `runner-contract` | `result.json`, `events.ndjson`, `summary.md`, exit-code → status mapping, runner contract spec | — |
| 4 | `cursor-sdk-runner` | Node script inside microVM wrapping `@cursor/sdk` `Agent.create() → send() → stream() → wait()` | 3 |
| 5 | `ship-rooms-backend` | implement `RoomCursorRunner` in `ship/packages/cursor-runner`, wire `backend: "rooms"` in `mcp__ship__ship` | 4 |
| 6 | `rootfs-builder` | `debootstrap`-based script for repeatable Ubuntu rootfs, no Nix yet | — |
| 7 | `nix-flake-input` | accept `--flake <flake.nix>` as deps spec; flake builds rootfs | 6 |
| 8 | `docs-vision-and-readme` | `docs/vision.md`, README, CLAUDE.md with dev-workbench block | — |

#1, #2, #3, #6, #8 are parallel-safe (different files, no overlap). #4 depends on #3. #5 depends on #4 (and lives in the ship repo, not this one). #7 depends on #6.

`/work-driver-prep` consumes this table, emits one spec doc per row + a batched driver manifest. `/work-driver` fans the parallel-safe ones out in one pass.

## What v0 does NOT prove

Honest about what this v0 *doesn't* demonstrate, so the bar is clear:

- **Snapshot/fork as differentiator.** v0 looks superficially like "a microVM-shaped worktree." The snapshot story is the actual killer (see firecracker-claude.md #2 "PR snapshots in ship") and lands in v0.2.
- **Hard parallelism.** v0 runs one room at a time. Multi-room concurrent execution comes when there's a real consumer (probably `/work-driver` fanning out).
- **Reproducibility across machines.** v0 uses one handbuilt rootfs; "the same room on a different host" only becomes meaningful with Nix in v0.1.

Calling out the gap explicitly so the v0 demo isn't oversold. The point of v0 is *the substrate exists and a real agent ran inside it*, not *the substrate is yet better than what you had before*.

## Decision log

- **Reframed from "agent rooms" → "microVM-with-deps primitive"** (2026-05-23): substrate is consumer-agnostic; agent invocation is the first consumer, not a substrate concern. Primitive verbs (`create / exec / collect / destroy`) are headline; `run` is convenience sugar.
- **New repo, not tower extension** (2026-05-23): tower is mid-deprecation as worktree tracker; reusing the name would muddy. Fresh repo, current conventions.
- **Rust, not Go** (2026-05-23): destination is Firecracker + rust-vmm-adjacent code; start in the right language, pay one-time velocity cost on v0 to skip a future rewrite.
- **Firecracker only (no local-process backend)** (2026-05-23): "if this is a Firecracker thing, Firecracker IS the thing." `local-process` would be a glorified worktree-in-a-temp-dir; not worth shipping or maintaining.
- **No premature trait abstraction** (2026-05-23): single concrete `Firecracker` backend. Trait extracted when a second consumer of the seam exists.
- **`rooms` binary runs inside the Ubuntu VM** (2026-05-23): skips cross-machine control plane for v0.
- **POC agent: `claude -p`, not cursor SDK** (2026-05-23): one binary + one env var vs ~100 lines of Node wrapper + `@cursor/sdk` install. Cursor SDK comes in v0.1's `cursor-sdk-runner` task.
- **Nix as canonical deps spec, deferred to v0.1** (2026-05-23): the primitive's input shape is "a deps spec"; Nix is the right format; v0 uses handbuilt rootfs because Nix-builds-non-NixOS-rootfs is its own learning curve and not on tonight's critical path. v0.1 flips the input.
- **Use `/work-driver-prep` for productionization, not the POC spike** (2026-05-23): POC is discovery work; speccing it ahead freezes design before Firecracker has misbehaved. Productionization is parallel-safe and benefits from one-spec-per-task. Use the right tool at the right step.
