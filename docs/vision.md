# rooms — vision

What `rooms` is, why it exists, what it focuses on (and what it leaves to other layers), and where it sits in the portfolio.

## What rooms is

The primitive:

> **Spawn a clean Firecracker microVM with specified deps, run a command in it, collect artifacts, destroy it.**

`rooms` is a Rust CLI that turns that lifecycle into an isolation substrate. A consumer passes a deps spec (today: a prebuilt rootfs image), a repo, and a command. The cold `run` path boots an ephemeral microVM, lands the repo inside, runs the command, collects stdout/stderr/exit code and any output artifacts back to the host, and tears the VM down.

The warm path creates a credential-free base, snapshots it, and restores one room or up to eight isolated clones from the shared immutable state. That Phase-2 Rooms substrate is implemented on the current branch. One retained exact-head host run reached terminal audit with every named hard check green, but it exited 1 because both performance gates failed; the code is not landed until review and Gate pass. Its distinct-task consumer adapter also remains open. Snapshot artifacts persist as explicit operator-retained evidence. Live rooms remain ephemeral.

**Lifecycle (canonical interface):**

- `run` — cold boot, execute, collect, and tear down one room.
- `base-create` → `snapshot` — produce one reusable neutral snapshot.
- `restore` — consume that snapshot for one kept room or one command.
- `clone -n 1..8` — restore a bounded fleet; command mode broadcasts the same command to every clone.
- `collect`, `ls`, `kill`, and `gc` — validate artifacts and manage live-room custody.

There is no public generic `create` / `exec` / `destroy` split today, and no `rooms exec <retained-id>` primitive. Kept rooms are an inspection and custody boundary, not yet a distinct-task scheduler.

## Why

Every portfolio tool that needs isolation — ship firing an agent, `/work-driver` recovering from a crash, future replay comparing two runs — should not reinvent "boot a VM, run something, collect results." That story belongs in one place.

`rooms` is that place. It owns Firecracker control, rootfs preparation, guest transport, command execution, and artifact collection. Callers own *what* runs inside the room; the substrate owns *how* the room exists.

## First workload: LLM agents

The first real workload is an LLM agent: `rooms run --image <rootfs> --runner cursor --repo <url> --task <task.md> --model <id> --base-sha <sha> --out <dir>`. The agent runs inside an isolated microVM with a real git checkout, makes changes, and leaves a `result.patch` on the host.

The substrate does not know about agents. It sees `run a command` — same as it would for a test suite, a linter, or a human shell script. Agent-specific logic (prompt format, SDK wiring, streaming events) lives in the runner script inside the rootfs, not in the Rust binary. That layering is intentional: manual operator use works today; ship integration and replay compose the substrate later.

## What rooms focuses on (and what it leaves to other layers)

`rooms` does one thing well — disposable microVMs — and stays out of the way of jobs that belong elsewhere. These aren't forbidden forever; they're where the work sits today, revisited when a real need shows up.

- **Codespaces-but-local** — `rooms` is execution substrate, not a persistent dev workspace with editor integration or "open in browser." That's a different product shape.
- **Dev workspace UX** — no interactive shell-as-product, no multi-tab terminal, no real-time file-watcher sync. Rooms run a command and collect the result.
- **Web preview** — no tunneling guest ports to the operator's browser today.
- **Multi-tenant** — one operator and one host. Phase 2 adds a bounded eight-clone fleet, not a shared control plane or multi-user scheduler.
- **Port forwarding** — guest network is for egress (API calls), not for exposing services.
- **Persistent rooms across reboots** — live rooms are ephemeral by design. Phase 2 persists immutable snapshot artifacts for later restore; it does not turn a running room into a durable workspace.
- **Docker / devcontainer / generic container runtime** — the isolation primitive is Firecracker microVMs. If a container is the better fit for a job, reach for a container tool; `rooms` doesn't try to be one.
- **Cross-host control** — v0 runs `rooms` on the same Linux+KVM host as Firecracker. Remote orchestration is a later concern, sequenced when a consumer needs it.

These are lines about where a job is best done, drawn deliberately and revisited as the work demands — not a standing veto. The opinion holds: **your-laptop-first ephemeral microVMs, Nix-described deps, portfolio tool not protocol.**

## Where this sits in the portfolio

```
dossier          — project memory; tracks rooms work and specs
ship             — workflow execution; future rooms fleet consumer
/work-driver     — productionization orchestration; future distinct-task adapter
rooms (this repo) — the isolation substrate
```

- **ship** does not yet expose a `backend: "rooms"` fleet path.
- **work-driver** already fans out spec-doc tasks, but does not yet map distinct tasks onto one warm Rooms fleet. `rooms clone --command` is a broadcast primitive, not that adapter.
- **dossier** holds the task graph, decision log, and cross-repo context.

`rooms` does not import ship or dossier. Dependency flows one way: consumers call `rooms`, not the reverse.

**Host layout:**

```
macOS or Windows
└── Lima (Apple silicon) or Hyper-V
    └── Ubuntu Server ("rooms-host") — /dev/kvm, Firecracker, rooms binary
        └── Firecracker microVM (ephemeral, one per room)
            ├── /workspace/repo   — git checkout from host bundle
            ├── /workspace/out    — artifacts collected back
            └── command under exec
```

Dev and privileged proof happen on the Ubuntu host (`limactl shell rooms-host` or SSH, then `cargo run`). The desktop side provisions the VM and edits the mounted checkout.

## Roadmap (light)

Check README status and spec docs for the exact landed boundary. In particular, "implemented" below does not mean merged.

| Milestone | Scope |
| --- | --- |
| **Cold substrate (landed)** | `run`: jailer boot, SSH command/runner execution, artifact collection, exact teardown, runner contract, rootfs builder, and host diagnostics. |
| **Warm Rooms substrate (implemented; hard-check evidence retained)** | `base-create` → immutable `snapshot` → `restore` / `clone -n 1..8`; namespace/NAT isolation, restore hygiene, bounded leases, witness custody, and exact teardown. One retained run passed every named hard check but failed both performance gates; review and Gate still precede landing. |
| **Replay evidence (future)** | Run receipts and comparison semantics that make two restored executions meaningfully replayable, beyond the state-local compatibility attestation, witness, and custody substrate. |
| **Consumer adoption (future)** | Ship backend plus a `/work-driver` fleet adapter that assigns distinct commands, outputs, and lifecycle streams to the warm clones. |
| **Deps (future)** | Nix flake as the deps spec (`--flake`). |

See [v0 spec](features/rooms-v0/spec.md) for the cold design, [snapshot/fork/replay spec](features/snapshot-fork-replay/spec.md) for Phase 2, and [productionization driver](features/01-productionization/driver.md) for the post-POC task manifest.

## Further reading

- [v0 spec](features/rooms-v0/spec.md) — v0 architecture, CLI surface, lifecycle, conventions.
- [snapshot/fork/replay spec](features/snapshot-fork-replay/spec.md) — Phase-2 contract, evidence gates, and remaining consumer boundary.
- [`README.md`](../README.md) — how to run it today.
- [`CLAUDE.md`](../CLAUDE.md) — notes for agents working in this repo.
