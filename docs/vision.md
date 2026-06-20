# rooms — vision

What `rooms` is, why it exists, what it focuses on (and what it leaves to other layers), and where it sits in the portfolio.

## What rooms is

The primitive:

> **Spawn a clean Firecracker microVM with specified deps, run a command in it, collect artifacts, destroy it.**

`rooms` is a Rust CLI that turns that lifecycle into substrate verbs. A consumer passes a deps spec (today: a prebuilt rootfs image; v0.1+: a Nix flake), a repo, and a command. `rooms` boots an ephemeral microVM, lands the repo inside, runs the command, collects stdout/stderr/exit code and any output artifacts back to the host, and tears the VM down.

One room, one command, one outcome. No shared state across runs.

**Lifecycle (canonical interface):**

1. `create` — allocate a room ID, prepare a rootfs overlay, boot the microVM.
2. `exec` — run a command in the guest; capture stdout, stderr, exit code.
3. `collect` — pull artifact dir from guest to host.
4. `destroy` — halt the VM, reap the Firecracker process, delete work dir.

`run` composes all four for the common case. Primitives are the contract; `run` is sugar.

## Why

Every portfolio tool that needs isolation — ship firing an agent, `/work-driver` recovering from a crash, future replay comparing two runs — should not reinvent "boot a VM, run something, collect results." That story belongs in one place.

`rooms` is that place. It owns Firecracker control, rootfs preparation, guest transport, command execution, and artifact collection. Callers own *what* runs inside the room; the substrate owns *how* the room exists.

## First consumer: LLM agents

The first real consumer is an LLM agent: `rooms exec <id> -- claude -p < task.md` (or, at the POC convenience layer, `rooms run --repo <path> --task <task.md>`). The agent runs inside an isolated microVM with a real git checkout, makes changes, and leaves a `result.patch` on the host.

The substrate does not know about agents. It sees `exec a command` — same as it would for a test suite, a linter, or a human shell script. Agent-specific logic (prompt format, SDK wiring, streaming events) lives in the runner script inside the rootfs, not in the Rust binary. That layering is intentional: ship's `RoomCursorRunner`, manual operator use, and future replay all compose the same primitive.

## What rooms focuses on (and what it leaves to other layers)

`rooms` does one thing well — disposable microVMs — and stays out of the way of jobs that belong elsewhere. These aren't forbidden forever; they're where the work sits today, revisited when a real need shows up.

- **Codespaces-but-local** — `rooms` is execution substrate, not a persistent dev workspace with editor integration or "open in browser." That's a different product shape.
- **Dev workspace UX** — no interactive shell-as-product, no multi-tab terminal, no real-time file-watcher sync. Rooms run a command and collect the result.
- **Web preview** — no tunneling guest ports to the operator's browser today.
- **Multi-tenant** — one operator, one host, one room at a time in v0. A control plane / shared pool lands if and when parallel demand is real.
- **Port forwarding** — guest network is for egress (API calls), not for exposing services.
- **Persistent rooms across reboots** — rooms are ephemeral by design. Snapshot/fork lands in v0.2; until then a room dies when the command finishes or the host reboots.
- **Docker / devcontainer / generic container runtime** — the isolation primitive is Firecracker microVMs. If a container is the better fit for a job, reach for a container tool; `rooms` doesn't try to be one.
- **Cross-host control** — v0 runs `rooms` on the same Linux+KVM host as Firecracker. Remote orchestration is a later concern, sequenced when a consumer needs it.

These are lines about where a job is best done, drawn deliberately and revisited as the work demands — not a standing veto. The opinion holds: **your-laptop-first ephemeral microVMs, Nix-described deps, portfolio tool not protocol.**

## Where this sits in the portfolio

```
dossier          — project memory; tracks rooms work and specs
ship             — workflow execution; will call rooms as a backend (v0.1)
/work-driver     — orchestrates productionization batches; will use rooms for crash recovery
rooms (this repo) — the isolation substrate
```

- **ship** integrates via `backend: "rooms"` in `mcp__ship__ship` and `RoomCursorRunner` (task #5, lives in the ship repo).
- **work-driver** fans out spec-doc tasks; when a cloud agent crashes mid-run, recovery reuses the same room lifecycle instead of ad-hoc process cleanup.
- **dossier** holds the task graph, decision log, and cross-repo context.

`rooms` does not import ship or dossier. Dependency flows one way: consumers call `rooms`, not the reverse.

**Host layout (v0):**

```
Windows 11
└── Hyper-V
    └── Ubuntu Server ("rooms-host") — /dev/kvm, Firecracker, rooms binary
        └── Firecracker microVM (ephemeral, one per room)
            ├── /workspace/repo   — git checkout from host bundle
            ├── /workspace/out    — artifacts collected back
            └── command under exec
```

Dev happens on the Ubuntu host (`ssh rooms-host`, `cargo run`). The Windows side only provisions the VM.

## Roadmap (light)

Nothing here is a shipped claim — check README status and spec docs for what exists today.

| Milestone | Scope |
| --- | --- |
| **v0** | POC + productionization. Boot microVM, SSH exec, artifact collection, `make check` CI, runner contract, rootfs builder, docs. POC upper bar: `rooms run --repo --task` end-to-end with `claude -p`. |
| **v0.1** | Nix flake as deps spec (`--flake`), ship backend integration, cursor SDK runner inside the guest. |
| **v0.2** | Snapshots + fork, replay receipts, hard parallelism for `/work-driver` fan-out. |

See [v0 spec](features/rooms-v0/spec.md) for the design and [productionization driver](features/01-productionization/driver.md) for the post-POC task manifest.

## Further reading

- [v0 spec](features/rooms-v0/spec.md) — v0 architecture, CLI surface, lifecycle, conventions.
- [`README.md`](../README.md) — how to run it today.
- [`CLAUDE.md`](../CLAUDE.md) — notes for agents working in this repo.
