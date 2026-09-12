# Agent-dev environments — Technical Design Document

**Status:** draft / proposal — NOT a build commitment. The artifact we decide from.
**Owner:** @itsHabib
**Date:** 2026-09-11
**Related:** [vision](../../vision.md) · [v0 spec](../rooms-v0/spec.md) · [snapshot/fork/replay](../snapshot-fork-replay/spec.md) · [readiness profile](../snapshot-fork-replay/p2-readiness-profile.md) · [nix-flake-input (parked)](../nix-flake-input/spec.md) · [disposable hosts (#117, open)](https://github.com/itsHabib/rooms/pull/117) · sibling TDD [local hardening & formal methods](../local-hardening/spec.md) · dossier project `rooms`

> **Reviewers — focus areas:** §4 D1 (three disks, not one image), §4 D3 (Nix presets as the toolchain layer), §4 D5 (one host proxy for both hostname egress and secret injection), §7.2 (the env-build flow relaxes the offline warm-up rule — check that neutrality still holds), §9 (what is committed vs gated).

## 1. Problem & hypothesis

rooms does the hard part well: jailed Firecracker boots, exact teardown, a sealed
credential-free base that forks into eight clones with fresh identity, secrets
that arrive only after resume, a network witness. What it cannot do is host a
real development task. Every room is 1 vCPU / 256 MiB
(`src/firecracker.rs:1595`), its writable layer is a RAM tmpfs
(`scripts/lib/overlay-init.sh:9`), the agent image is Alpine with git, curl,
ripgrep and claude-code and no language toolchain
(`scripts/build-rootfs-alpine.sh:164-173`), there is no `--env`, no file
injection, no repo input for `--command` runs, Claude is not a runner
(`src/main.rs:371-376`), output is only visible after the run ends, and the
warm-up that builds a snapshot runs with no network, so dependencies cannot be
pre-installed (`src/main.rs:1341`, `scripts/lib/rooms-provision-agent.sh:98-111`).
The deps spec the vision promises ("Nix-described deps") is a parked doc.

**Hypothesis.** The environment question splits by *lifetime*, and each layer has
one right delivery mechanism:

| Layer | Lifetime | Delivery |
| --- | --- | --- |
| Toolchain (rustc, go, node, chromium, postgres) | per lockfile, months | Nix preset → immutable read-only disk ("toolstore"), shared by every room and clone |
| Project deps (`cargo fetch`, `npm ci`) | per lockfile change, days | built once with registry-only egress → immutable env disk + memory snapshot |
| Repo | per task | git bundle at a SHA plus the working-tree diff |
| Agent profile (settings, skills, git identity) | per operator | secret-free directory copied in at start |
| Secrets | per task, never in a snapshot | host-side proxy injects them into outbound requests; the vsock secrets file stays as fallback |
| Task | per task | prompt, env vars, files, sizing, egress policy, wall clock |

If that holds, a task starts from a warm snapshot that already has the toolchain
and deps, runs on a room sized for the job, and rooms keeps its two invariants:
the snapshot never holds a secret, and every clone shares immutable state
copy-on-write.

**Non-goals (unchanged):** a persistent workspace, editor integration, web
preview / port forwarding, Docker as the isolation primitive, multi-tenant
scheduling. **Two v0 non-goals are relaxed by this design** and the reviewer
should weigh in: (a) env build gets *registry-only* network access — still no
secrets, still no sudo; (b) watching and shelling into a running room stops
being a non-goal. Both are argued in §4.

## 2. Requirements

**Functional**

- FR1 A repo declares its environment in `.rooms/env.toml` (§5); rooms builds it into a cached snapshot keyed by its inputs and invalidates it when they change.
- FR2 Toolchains come from rooms-maintained Nix presets (`rust`, `go`, `node`, `python`, `browser`, `services`) or the repo's own flake output; no image rebuild per stack.
- FR3 `rooms run` accepts `--vcpu`, `--mem`, `--disk`, `--env K=V`, `--env-file`, `--file host:guest`, and a `--repo` for every runner.
- FR4 `--runner claude` runs `claude -p` on a prompt file and emits the same artifact set as the cursor runner (`events.ndjson`, `result.json`, `result.patch`).
- FR5 `rooms run --follow` streams guest stdout/stderr and agent events live; `rooms shell <id>` opens an SSH session into any live room; `rooms logs <id> [-f]` tails.
- FR6 Secrets reach the guest only after resume and never enter a snapshot (unchanged), with the host proxy as the preferred path.
- FR7 The same `env.toml` produces an equivalent environment on the aarch64 Lima host and an x86 box (`scripts/box.sh`), because snapshots do not travel but declarations do.

**Non-functional**

| Concern | Target |
| --- | --- |
| Warm task start (env snapshot → agent's first tool call), n=1, Lima | ≤ 3 s (today's restore ACK is 1.3–2.2 s at n=1; THP-staged) |
| Env build (`cargo fetch` for dossier) | ≤ 5 min, then cached until a lockfile changes |
| Toolstore build (`rust` preset, warm Nix cache) | ≤ 2 min; content-addressed, shared across envs |
| Room sizing | up to host limits; default 2 vCPU / 2 GiB / 8 GiB scratch for agent runners |
| Security | env build has no secrets and no sudo; snapshot neutrality check unchanged; toolstore and env disks are `ImmutableFileIdentity`-sealed like `snapshot.mem` |
| Clone fit | eight clones share toolstore + env disk + `snapshot.mem`; per-clone private state is only the scratch disk and CoW pages |
| Operability | `rooms env ls/gc`, `rooms toolstore ls/gc`; every artifact under `~/.local/state/rooms/` with a hash-named dir |

## 3. Architecture overview

```
host (rooms-host)                                   guest
─────────────────────────────────────────           ────────────────────────────────
presets/<name>/flake.nix ──nix build──▶ toolstores/<h>.sqfs  ─▶ /dev/vdb ─▶ /nix/store (ro)
.rooms/env.toml + lockfiles ──env build─▶ envs/<key>/env.ext4 ─▶ /dev/vdc ─▶ overlay lower #2
                                         envs/<key>/snapshot/  (mem + vmstate, sealed neutral)
per room: rooms/<id>/scratch.ext4 (sparse) ─────────────────▶ /dev/vdd ─▶ overlay upper+work
agent image (unchanged Alpine, ro) ─────────────────────────▶ /dev/vda ─▶ overlay lower #1
rooms-proxy (per room, on tap gateway) ◀── HTTPS_PROXY / ANTHROPIC_BASE_URL ── agent
```

New: `src/env.rs` (env spec, key, build), `src/toolstore.rs` (preset build,
squashfs pack, identity), `src/proxy.rs` (per-room forward proxy), a `claude`
runner in `src/runner.rs`, `--follow`/`shell`/`logs` verbs, `presets/` in the
repo, and a three-layer `overlay-init`. Reused unchanged: jailer boot, slot
pool, `ImmutableFileIdentity`, snapshot/restore/clone/matrix, vsock secrets,
egress chains, witness. **The seam:** everything below the "layer" table in §1
is a disk attached at boot; rooms never learns what a toolchain is.

## 4. Key decisions & trade-offs

**D1 — Three disks, not one bigger image.** Toolstore (ro), env disk (ro at task
time), scratch (rw, per room). *Alternative:* keep the single-image model and
grow the tmpfs. *Why:* the tmpfs upper is captured in `snapshot.mem`, so every
byte of deps would be memory, and memory is the phase-2 gate that is already
over budget (156 MiB vs 115 MiB for eight clones). Disks are shared
copy-on-write across clones for free — a clone attaches the same immutable
files — and only the sparse scratch disk is private. Overlayfs takes multiple
`lowerdir`s, so the env disk written as the upper during build becomes a lower
at task time; that is exactly how image layers work.

**D2 — Restore must attach the same disks.** A Firecracker snapshot does not
carry disk contents; restore needs the same backing files. `SnapshotMeta` gains
`toolstore` and `env_disk` identities and restore refuses a mismatch, the same
refusal shape as the non-neutral check (`src/restore.rs:56`). *Trade-off:* an env
snapshot is pinned to one toolstore hash; changing a preset rebuilds the env.
Correct — a toolchain change *is* an environment change.

**D3 — Nix presets for the toolchain layer.** rooms maintains `presets/<name>`
flakes producing a `buildEnv`; the host runs `nix build` (binary cache, so
mostly downloads), packs the closure into a squashfs, seals it. *Alternatives:*
(i) `apk add` via `--extend` and rebuild the image — today's path, one image per
stack, musl breaks prebuilt binaries such as Playwright's Chromium; (ii) OCI
images flattened to ext4 — standard elsewhere, but `RUN apt-get` is not
reproducible and a flattened image cannot be shared across stacks. *Why Nix:*
content-addressed cache key, packages carry their own libc so they run on the
Alpine image unchanged, one preset serves every env, and the same flake builds
an aarch64 closure on Lima and an x86 closure on a GCP box, which is what makes
FR7 true. *Costs:* Nix on the rooms-host (`setup-rooms-host.sh` installs it;
never on the Mac), a squashfs-capable guest kernel (§10 Q1), and the operator
maintaining presets. The parked `nix-flake-input` spec wanted the flake to
produce the *whole rootfs*; this narrows it to the toolchain and keeps the Alpine
image as the boot layer — smaller blast radius, no new boot path. The repo's own
flake is accepted too (`flake = "./#rooms"`), so a repo that already has one
(bifrost) skips presets.

**D4 — Env build gets registry egress; still no secrets, no sudo.** Today
`base-create` forces `--egress none`. Deps cannot be pre-warmed without a
network, and the alternative (vendor everything) is not how anyone works. The
build phase gets the `registries` egress preset (crates, npm, PyPI, Go proxy,
GitHub over HTTPS) via the proxy in D5, keeps the scrubbed environment and the
credential-marker refusal (`src/runner.rs:799-823`), and the neutrality seal is
checked exactly as before. *What could go wrong:* a dependency's build script
phones home with something it should not have — it has nothing, because the
build has no secrets. A registry serving a malicious package is the same risk
`cargo fetch` has on the laptop; the witness pcap records it.

**D5 — One host-side proxy does both egress-by-hostname and secret injection.**
Today's allowlist resolves each hostname to one IPv4 at launch
(`src/egress.rs:125-298`), which breaks on rotating CDNs and makes "GitHub" a
CIDR-maintenance chore. A small forward proxy on the tap gateway, one per room,
allows by hostname (CONNECT for TLS, plain forward for HTTP) and, for named
upstreams, rewrites `Authorization` / `x-api-key` from a key it holds — the guest
sees only a sentinel value. The iptables chain stays and shrinks to "guest may
reach only the proxy" (belt and braces). *Alternative:* keep vsock secrets as the
only path. *Why:* keys never enter the VM, so a prompt-injected `env | curl` has
nothing to exfiltrate, and the same component fixes the hostname problem. The
vsock path stays for secrets that are not HTTP credentials. *Open:* whether the
Cursor SDK honours `HTTPS_PROXY` (§10 Q3); Claude Code honours
`ANTHROPIC_BASE_URL` and `HTTPS_PROXY`.

**D6 — Claude runner mirrors the cursor runner's contract, in-guest script.**
Agent logic lives in the rootfs (`scripts/rootfs/claude-runner.sh`), the Rust
side only ships the prompt and collects the same artifacts. `claude -p` runs with
`--output-format stream-json --verbose`, stdin from `/dev/null`
(`docs/follow-ups.md`, 2026-05-29), and `--dangerously-skip-permissions` under
the non-root `rooms` user. Turn and token caps are runner flags, wall clock is
rooms' `--max-wall`.

**D7 — Live view is a second SSH session, not a protocol.** `--follow` and
`logs -f` are `tail -F` over SSH on the guest log files; `shell` is the existing
SSH path with a TTY. No new transport, no daemon. Kept rooms already allow this
by hand; the change is to allow it *during* an exec, which only requires not
tearing down until the exec ends — already true. `--keep` remains exclusive with
`--command`; `shell` replaces the reason people reached for it.

**D8 — Profile directory, secret-free by construction.** `--profile <dir>`
(default `~/.rooms/profile/`) is copied to the guest's `~rooms/.claude` and
`~/.gitconfig` before the exec. Files matching the credential-marker scan are
refused, not skipped, so a profile can never smuggle a token past D5.

## 5. Data model

**`.rooms/env.toml`** (in the repo; absent = image defaults, no env snapshot):

```toml
version = 1
presets = ["rust", "node"]          # rooms-maintained; or:
# flake = "./#rooms"                # repo's own flake output (a buildEnv)

[build]                             # runs once per key; registry egress; no secrets, no sudo
run   = ["cargo fetch --locked", "npm ci"]
egress = "registries"               # preset, or ["host", "cidr", ...]
inputs = ["Cargo.lock", "package-lock.json"]   # default: known lockfiles + flake.lock

[room]
vcpu = 2
mem_mib = 2048
disk_gib = 8

[refresh]                           # optional; per task, before the agent, offline
run = ["cargo build --offline --tests"]
```

**Env key** = `sha256(image identity ‖ kernel identity ‖ toolstore hash ‖ canonical env.toml ‖ blob hashes of build.inputs)`. Any change → new key → rebuild; old envs are `gc`'d by age.

**On-disk layout** under the state base (`src/config.rs`):

```
toolstores/<h>/toolstore.sqfs        chattr +i, ImmutableFileIdentity
toolstores/<h>/meta.json             {presets|flake, flake.lock sha, arch, closure paths, built_at}
envs/<key>/env.ext4                  sealed after build
envs/<key>/snapshot/{snapshot.mem,vmstate,meta.json}   THP-staged (follow-up 2026-09-02) — default here
envs/<key>/meta.json                 EnvMeta below
rooms/<id>/scratch.ext4              sparse; created at claim, deleted at teardown
```

**Rust shapes**

```rust
pub(crate) struct EnvSpec { version: u8, toolchain: Toolchain, build: BuildSpec, room: RoomSize, refresh: Option<RefreshSpec> }
pub(crate) enum Toolchain { Presets(Vec<PresetName>), Flake(FlakeRef) }
pub(crate) struct EnvKey([u8; 32]);
pub(crate) struct EnvMeta { key: EnvKey, toolstore: ImmutableFileIdentity, env_disk: ImmutableFileIdentity, base_repo_sha: String, built_at: SystemTime, inputs: Vec<(PathBuf, [u8; 32])> }
pub(crate) struct RoomSize { vcpu: u8, mem_mib: u32, disk_gib: u16 }   // Default: 1/256/0 for command runs (today), 2/2048/8 for agent runners
// SnapshotMeta gains: env_key: Option<EnvKey>, toolstore: Option<ImmutableFileIdentity>, env_disk: Option<ImmutableFileIdentity>; base_repo_sha becomes Some (closes follow-up 2026-08-04)
```

**Runner artifacts** (claude, in `/workspace/out`): `events.ndjson` (stream-json verbatim), `result.json` (runner-contract schema + `runner: "claude"`, `turns`, `cost_usd` when reported), `result.patch`, `logs/`.

## 6. API contract

```
rooms run [--image I] [--repo PATH|URL] [--env-spec .rooms/env.toml|--preset NAME,..|--no-env]
          [--vcpu N] [--mem MIB] [--disk GIB]
          [--env K=V]... [--env-file F] [--file HOST:GUEST]... [--profile DIR]
          [--runner command|cursor|claude] [--command C | --prompt FILE] [--model M] [--max-turns N]
          [--egress none|registries|allowlist:..|proxy] [--secret NAME]... [--out DIR] [--follow] [--max-wall D]
rooms env    build  --repo PATH [--env-spec F]        → prints EnvKey; exit 0 built / 0 cached / 1 build failed / 3 refused (credential marker, tainted)
rooms env    ls | gc [--older-than D] | inspect KEY
rooms toolstore build --preset NAME,.. | --flake REF → prints hash;  ls | gc
rooms shell  <id>                                   → interactive SSH into a live room (exit = ssh exit)
rooms logs   <id> [-f] [--events]                    → guest logs / events.ndjson
rooms restore|clone|matrix ... [--env KEY]           → attaches that env's disks; refuses identity mismatch
```

Rules: `--env`/`--env-file` values go to the guest via a generated
`/run/rooms/env` file sourced by the runner (not SSH `SendEnv`, whose accept
list is baked into sshd — `scripts/build-rootfs-alpine.sh:309-313`); names must
match `[A-Z_][A-Z0-9_]*`; values containing a credential marker are refused
unless the name was also passed as `--secret`. `--file` sources are tarred on
the host with `--no-same-owner`, landed under `/workspace/in/` (relative
targets) or an absolute guest path; symlinks are not followed. `--repo` for
`command` and `claude` runners uses the `base-create` bundle path
(`src/runner.rs:825-878`) plus a `working-tree.patch` of uncommitted and
untracked files, applied after checkout; URLs with embedded credentials are
refused (today the cursor runner stores them in `room.json`).

**Errors (new variants, `src/error.rs`):** `EnvBuildFailed{step, exit}`,
`EnvInputsMissing{path}`, `ToolstoreArchMismatch{want, have}`,
`SnapshotDiskMismatch{which, want, have}`, `ProfileCredentialMarker{path}`,
`ScratchTooSmall{need, have}`, `ProxyUnavailable`. Exit codes keep the existing
map (0 / 1 workload / 2 indeterminate / 3 refused / 4 error).

## 7. Key flows

**7.1 Toolstore build** — `rooms toolstore build --preset rust,node`:
1. Resolve presets to `presets/<name>/flake.nix` in the installed rooms share dir; compute `h = sha256(arch ‖ flake.lock shas ‖ preset names)`; if `toolstores/<h>` exists and seals, print and exit 0.
2. `nix build presets#rust presets#node --out-link tmp/` on the host as the invoking user (not root), then `nix-store -qR` the closure; refuse if any path is not under `/nix/store`.
3. `mksquashfs` the closure plus a generated `/nix/var/rooms/env/bin` (the merged `buildEnv` bin dir) into `toolstore.sqfs.tmp`; `chattr +i`; rename into place; write `meta.json`; record `ImmutableFileIdentity`.
4. Failure: nix build error → exit 1 with the last 40 lines; partial dirs are removed; never leaves an unsealed file under `toolstores/`.

**7.2 Env build** — `rooms env build --repo ~/dev/dossier`:
1. Parse `env.toml`; hash inputs; compute key; cached → exit 0.
2. Ensure toolstore (7.1). Create `env.ext4` (sparse, `disk_gib`), `mkfs.ext4`.
3. Boot a **base** (existing `base-create` path: `rooms.base=1`, no sshd, vsock only, sealed neutral gate) with vda=image ro, vdb=toolstore ro, vdc=env.ext4 **rw as overlay upper**, `--egress registries` routed through the room's proxy with **no** credentials loaded. Land the bundle (vsock 5001) at the pinned SHA.
4. Run `build.run` steps in order under the `rooms` user, no sudo; each step is bounded by the provisioning timeout (`src/config.rs:66`); any nonzero exit fails the build.
5. Guest-side credential scan of the upper (same markers as `rooms-provision-agent.sh:254-285`); seal (`Neutral`), snapshot memory with THP staging, shut down.
6. Host: `chattr +i env.ext4`, identity → `EnvMeta`; write `meta.json` last (the presence of `meta.json` is the commit point); anything else present without it is garbage `env gc` removes.
7. Failure modes: registry unreachable → step fails with the proxy's denied-host log attached; disk full → `ScratchTooSmall` naming `disk_gib`; neutrality seal fails → exit 3, nothing written.

**7.3 Task run from an env** — `rooms run --repo . --runner claude --prompt task.md --follow --out out/`:
1. Load env by key (rebuild if inputs changed, unless `--no-env-build`); claim slot; create `scratch.ext4`; start the room proxy with the task's egress policy and the secrets it may inject.
2. Restore the env snapshot with vda/vdb/vdc identical (D2) and vdd = scratch. `overlay-init` builds `lowerdir=/oldroot:/env,upperdir=/scratch/upper`.
3. Post-resume hygiene as today (reseed, identity, hostkeys, sshd); secrets file via vsock if any `--secret`; env file, profile, working-tree patch, `--file`s over SSH; `refresh.run` offline.
4. Exec `claude-runner.sh`; with `--follow`, a second SSH session tails `logs/stdout.log`, `logs/stderr.log` and `events.ndjson` to the host's stdout until the exec ends.
5. Collect `/workspace/out` (as today, then `chown` to `SUDO_UID` — hardening TDD H1), stop the proxy, delete scratch, teardown, `cleanup_done`.
6. Cancel/timeout: SIGINT *and* SIGTERM run the same teardown (hardening TDD H1); partial logs are preserved, not truncated.

**7.4 Clone fleet from an env** — `rooms clone -n 8 --env KEY --command 'cargo test'`: as today, plus per-clone scratch disks and the shared ro toolstore/env disks; the memory file is THP-staged. Private per-clone bytes = CoW pages + scratch writes.

## 8. Concurrency / consistency / failure model

- **Immutability is the consistency model.** Toolstore, env disk and `snapshot.mem` are sealed and identity-checked at every attach; a changed inode → refuse, never "use anyway". Two concurrent `env build`s for the same key take the per-key lock (reuse the snapshot intent lock shape, `src/snapshot.rs`); the loser observes `meta.json` and returns cached.
- **Commit points** are single renames: `toolstore.sqfs`, `env.ext4` seal, `meta.json`. Crash before the commit point leaves garbage that `gc` removes by the absence of `meta.json`; crash after leaves a complete artifact.
- **Scratch disks** are owned by the room dir and reaped by the existing `gc` path when the room is dead.
- **Proxy failure** during a task fails the *request* in the guest (connection refused), never the room; `result.json` records `proxy_denied: [host...]` from the proxy log so a blocked fetch is diagnosable.
- **Degraded mode:** no Nix on the host → presets unavailable, `--preset` exits 4 with the install hint; `--extend`-built images keep working. No squashfs in the kernel → toolstore refused at doctor time (new `toolstore_fs` check).

## 9. Rollout / implementation plan

| Phase | Goal | High-level tasks | Depends on | Band (weighted LOC) | Gate |
| --- | --- | --- | --- | --- | --- |
| **A — a usable room** | A room big enough for a real task, with inputs | `--vcpu/--mem/--disk` + scratch disk + 3-layer overlay-init (image rebuild); `--env/--env-file/--file`; `--repo` + working-tree patch for command runs; THP staging default for snapshots; bug fixes land in the hardening TDD (H1), not here | — | ideal (~600) | — |
| **B — Claude runner + live view** | Claude as a first-class runner; watch and step in | `claude-runner.sh` + `--runner claude --prompt`; `--follow`; `rooms shell`, `rooms logs`; `--profile` with marker scan | A | ideal (~550) | **G1 (§11)** |
| **C — toolstores** | Nix presets as the toolchain layer | `setup-rooms-host.sh` installs Nix; `presets/{rust,go,node,python}`; `rooms toolstore build/ls/gc`; squashfs attach + doctor check; `SnapshotMeta` disk identities | A, G1 | stretch (~800) | **G2 (§11)** |
| **D — project envs** | `.rooms/env.toml` → cached env snapshot | `src/env.rs` (spec, key, inputs); `rooms env build/ls/gc/inspect`; registry egress for build; `refresh.run`; `restore/clone/matrix --env` | C, G2 | stretch (~900) | — |
| **E — secret proxy** | Keys never enter the VM | `src/proxy.rs` (hostname allow + header injection); `--egress proxy`; `registries` preset; custody on Linux/macOS is a **workbench** task, out of this repo | A | ideal (~650) | — |
| **F — import** | Meet repos where they are | `Dockerfile` / `devcontainer.json` → toolstore + env.toml suggestion; `browser` and `services` presets | D | stub | — |

A and B are the commitment. C is gated on G1 (the room must be usable before
the toolchain layer matters). D and E are gated on G2. F is a stub until a real
repo needs it.

## 10. Open questions

1. **Guest kernel filesystems.** The Firecracker CI kernel (`v1.15`, `setup-rooms-host.sh:29`) — does its config include `squashfs` and multi-lower `overlay`? erofs would be better (smaller, faster random reads) but is less likely to be built in. Check `/proc/config.gz` on the rooms-host before Phase C; a custom kernel config is a fallback, not a plan.
2. **Where the env disk is written when the upper is a disk during build but memory at task time.** D1 says env disk is the build's upper. Alternative: build's upper is tmpfs (as today) and the deps therefore live in `snapshot.mem`. The disk route keeps memory small but every clone's first read of a dep is a stage-2 fault either way (`p2-readiness-profile.md`). Measure both on the n=8 path before committing D1's layout.
3. **Cursor SDK and proxies.** Does `@cursor/sdk` honour `HTTPS_PROXY` / a base-URL override? If not, cursor runs keep the vsock secret path and lose D5's guarantee.
4. **Working-tree patch and large untracked files.** Cap size (reuse the artifact size cap follow-up, 2026-05-30) or require a clean tree for `clone`/`matrix`? Proposal: cap at 32 MiB, refuse over it with the offending paths listed.
5. **Preset ownership.** Who bumps `flake.lock` in `presets/`? Proposal: a monthly PR from CI (Dependabot does not do flakes), reviewed like code.
6. **Should `command` runs default to the agent sizing?** Today's 1/256 is right for `true` and wrong for `cargo test`. Proposal: size comes from `env.toml` when present, else runner default.

## 11. Validation plan

**G1 (after B) — "rooms can host a real task".** On the Lima host, with a
hand-extended image (`--extend` adding rust): `rooms run --repo ~/dev/dossier
--runner claude --prompt "run cargo test and fix one failing test" --vcpu 4 --mem 4096
--disk 8 --follow --out out/`. Pass = `cargo test` completes inside the room,
`result.patch` is non-empty and applies on the host, the live stream showed the
agent's tool calls while it ran, `rooms shell` attached mid-run, and `result.json`
records exit 0. Binary; no baseline needed.

**G2 (after C) — "the toolchain layer is Nix, and it travels".** The `rust`
preset builds on Lima (aarch64) and on a `box.sh` GCP box (x86_64) from the same
`presets/` commit; the same `rooms run` from G1 passes on both with the *stock*
Alpine image (no `--extend`), and the toolstore is byte-identical across two
builds on the same arch. Fail → stop at C; envs can still be built on
`--extend` images and D proceeds on that basis.

**Standing measure** (not a gate): warm task start at n=1 and fleet readiness at
n=8, recorded per phase in `p2-readiness-profile.md`, so this work is seen to
help or hurt the phase-2 number rather than hide it.
