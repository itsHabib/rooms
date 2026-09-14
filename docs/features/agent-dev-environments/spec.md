# Agent development environments

**Status:** design proposal; merging this document does not implement its proposed APIs.
**Owner:** @itsHabib
**Related:** [vision](../../vision.md), [snapshot contract](../snapshot-fork-replay/spec.md), [hardening proposal](https://github.com/itsHabib/rooms/pull/119), [cloud experiments](https://github.com/itsHabib/rooms/pull/125).

## What exists and what remains

The original proposal preceded three merged changes: [#120](https://github.com/itsHabib/rooms/pull/120) added command repository inputs, CPU/memory sizing and cold-room scratch disks; [#121](https://github.com/itsHabib/rooms/pull/121) added sealed Nix toolstores; [#123](https://github.com/itsHabib/rooms/pull/123) added toolstore identity to snapshot/restore. These are the starting point, not work to repeat. This reconciliation is anchored to main `f702be4`; refresh the implementation before building a remaining part.

A useful next outcome is an agent completing a real repository task inside a room, with visible progress, a reviewable patch and repeatable validation. Reuse the existing run, snapshot, clone, artifact and teardown paths. Add an option only when that task needs it.

Remaining proposals are a Claude runner, live logs and shell access, explicit task inputs, cached project dependencies, and an HTTP credential broker. They can ship independently where their actual dependencies permit. No fixed line-count bands, new universal review stage or mandatory framework follows from this document.

## Lifetimes and requirements

| Input | Lifetime | Delivery |
| --- | --- | --- |
| Toolchain | Pinned flake inputs and architecture | Existing sealed toolstore, mounted read-only at `/nix` |
| Project dependencies | Complete build context | Proposed immutable environment layer; benchmark against the existing tmpfs snapshot first |
| Repository | Task revision | Existing pinned bundle; explicitly selected working-tree changes may follow |
| Profile | Operator selection | Allowlisted settings, skills and Git identity, supplied after resume |
| Credentials | Task | Fixed-upstream host broker where supported; explicit vsock delivery otherwise |
| Task | One run | Prompt file, ordinary environment values, selected files, sizing and wall clock |

A cache hit must represent the exact build inputs. Clones share immutable inputs but never a writable filesystem. Restoring does not reboot the guest or rerun its init script. A snapshot's memory, VM state and attached disks must describe one consistent machine.

Architecture-specific declarations may be shared between Lima and GCP; closures and snapshots are not portable merely because their declaration matches. Prove support on each architecture and the actual host CPU/kernel before claiming portability.

## Disk layout and snapshot sequence

Current cold runs can use a private scratch drive; current snapshots support a read-only toolstore and a tmpfs upper. Neither implements a persistent environment lower plus per-clone scratch restore. `scripts/lib/overlay-init.sh`, `src/firecracker.rs`, `src/snapshot.rs` and `src/restore.rs` own those contracts.

The proposed persistent layout has four drives: root image read-only, toolstore read-only, environment read-only, and scratch writable. Device IDs and jail paths are versioned metadata, not guessed from attach order. Mount the entire toolstore at `/nix`, including `store` and `var/rooms/env/bin`; mounting it at `/nix/store` is incorrect.

Building an environment and freezing the final machine are two different boots:

1. In a disposable build VM, attach a writable environment disk as the overlay upper. Supply the pinned repository and only the declared public registry network policy. Run dependency steps as the unprivileged user, without task credentials or sudo.
2. Shut that VM down; ensure the disk is quiescent, check filesystem consistency and validate the persisted content. Seal the environment disk. **Do not use this VM's memory snapshot for task restore:** its kernel still knows that disk as a writable upper.
3. Cold-boot a second VM with the final four-drive topology already present. Its overlay uses the environment above the base as read-only lowers and a separate scratch filesystem as upper/work. Warm it offline through the existing neutral-base process. Freeze it with all filesystem writes quiescent, recording the matching scratch backing bytes, memory, VM state and disk identities together.
4. Each restore receives a private byte-identical copy/reflink of that captured scratch template. The saved writable device must resolve to this private file through a supported Firecracker restore operation or the same relative jail path in a fresh per-room jail. Never attach a new device after `/snapshot/load`, switch a saved upper to a lower, or expect `overlay-init` to run again.

The exact jail-path mapping and scratch-template capture are **an unresolved implementation prototype**, not a claim about today's restore API. Before adopting this layout, extend the snapshot schema and prove two simultaneous clones can write different values to the same path without changing each other or any sealed input; crash and restore tests must cover the publication cuts. If the installed Firecracker API cannot support the mapping, keep the existing tmpfs snapshot path and defer persistent environment snapshots. Cold disk-backed environments can still be useful independently.

Record toolstore/environment/template hashes, image and kernel identities, architecture, CPU compatibility, device IDs, jail paths, filesystem format and snapshot schema version. Older readers must refuse a topology they do not understand. Reuse the existing snapshot publication transaction and local attestation: seal and verify the complete directory, metadata, VM state, memory and immutable disk inputs, not only `env.ext4`.

## Environment build and cache

An initial `.rooms/env.toml` proposal is deliberately limited:

```toml
version = 1
presets = ["rust", "node"]

[build]
run = ["cargo fetch --locked", "npm ci"]
egress = "registries"

[room]
cpus = 2
memory_mib = 2048
disk_gib = 8
```

Use the existing `--cpus`, `--memory` and `--disk` spellings and current validation. Do not invent `--vcpu`/`--mem` aliases or silently change current defaults. Build commands are intentional untrusted workload execution, not host shell commands.

The environment key covers the canonical spec, **complete exported build context** (paths, types, modes and bytes, including manifests, lockfiles, configuration and scripts), all selected flake sources and locks, toolstore digest, image/kernel identities, architecture, sizing, builder/schema version and any explicitly supplied build variables. Undeclared host state is not an input. A lockfile-only key is insufficient for arbitrary build scripts. Refuse missing inputs; a cache hit requires matching metadata and valid seals.

Toolstore keys likewise include full flake source contents and locks, preset selection, target system and builder version. Reuse the implemented builder rather than replacing its key or layout with this proposal. Nix builds as the invoking user; validate closure paths and the complete mounted layout.

Build in a private staging directory under a per-key lock. Write and fsync all content and metadata, rename into the final location while still holding the lock, apply seals at the final paths, verify them and publish readiness through the existing transaction contract. Readers take the same lock and require complete valid seals; a directory or `meta.json` alone is not readiness. Never `chattr +i` a temporary file before trying to rename it. A crash after rename but before sealing remains an incomplete transaction, never a cache hit.

GC acquires that same key lock before classifying or deleting incomplete builds and respects active snapshot/room references. Age or absent metadata does not justify deleting a live build. Failed builds preserve useful diagnostics and remove only their own disposable resources.

## Network and credential delivery

Two different HTTP mechanisms serve different purposes:

- A **forward proxy** supports public registry hostname egress. CONNECT carries opaque TLS bytes; it cannot inspect or inject HTTPS authorization headers.
- A **fixed-upstream reverse credential broker** terminates the application's request protocol and makes a separate TLS connection to a configured provider, adding the host-held credential there. It accepts no arbitrary destination or caller-supplied upstream URL. Base-URL support must be demonstrated for the selected SDK. No generic TLS interception is proposed.

Bind each endpoint to its room's gateway surface and authorize only that room, including isolation from sibling rooms. Validate every resolved upstream address, reject loopback/private/link-local/metadata destinations, pin the checked address for the connection and repeat validation on redirects/reconnects to avoid rebinding. The tap firewall allows only the chosen proxy route; DNS and bootstrap access must also be explicit. Use an authenticated protected guest-to-broker channel; transport, certificate trust and provider SDK compatibility are part of the broker prototype.

The public registry preset starts with specific required HTTPS endpoints, for example `index.crates.io`/`static.crates.io`, `registry.npmjs.org`, `pypi.org`/`files.pythonhosted.org`, and `proxy.golang.org`/`sum.golang.org`. Add package redirects or GitHub sources only when a workload demonstrates the need, recording the resulting allowlist. No arbitrary host/CIDR override is hidden inside `registries`. A custom network policy is a separately named operator choice and loses the registry-only claim.

Dependency scripts may read repository data and send it to an allowed registry. Having no injected credentials does not make that data public or make the build harmless. Only selected, appropriate build context enters this phase. A packet capture documents traffic; it does not establish absence of exfiltration.

The broker keeps a key out of guest memory; it does not prevent the guest using its authorized upstream capability or spending that key's allowance. Record allowed/denied destinations, request counts and lifecycle without headers, bodies or tokens. Proxy failure fails affected requests and remains visible in host-owned diagnostics.

For unsupported SDKs or non-HTTP secrets, retain explicit post-resume vsock delivery and state that the guest receives the secret. Never silently downgrade the broker guarantee. Snapshot creation after any secret delivery must be refused through host-owned provenance, regardless of guest reports.

## Inputs, profiles and runner

Ordinary `--env` / `--env-file` values are data: validate names and encode an environment vector for `execve`, or equivalent structured parsing with no shell evaluation. Never source a generated environment file. Tests include newlines, quotes, command substitutions and duplicate names. A secret source is separate; naming the same variable with `--secret` is an error, not an exemption that permits serializing an inline credential.

Profiles select known public settings, skill files and Git identity fields. Reject credential stores, arbitrary MCP configuration and unsupported paths; never copy an entire agent home directory. Validate the full selected content before transfer. Marker scanning is defense in depth and cannot prove arbitrary prose contains no secret. Operators select content appropriate for the guest; documentation must not promise profiles are "secret-free by construction."

File inputs initially land only beneath `/workspace/in`. Validate each relative path, refuse traversal, symlinks, special files and destination collisions. Tar creation does not use extraction-only ownership flags; the guest extractor applies ownership restrictions under the unprivileged user, and verifies members before writing. Reuse existing artifact validation where applicable. No arbitrary absolute-path overwrite is needed for the first use case.

Repository bundles pin a revision using existing code. Working-tree changes and untracked files require explicit selection, a size bound with offending paths reported, and the same sensitive-input review; ignored files and credentials are not silently swept into a patch. Credential-bearing remote URLs are refused before logs or metadata publication.

A proposed Claude runner supplies a **prompt file** after resume and collects `events.ndjson`, `result.json`, `result.patch` and logs through the existing runner contract. Confirm CLI invocation against the installed Claude version when implementing. The guest's reported success is workload evidence, not independent proof that its patch fixes the task.

Live view reuses a second SSH session. Tailing starts before workload launch, handles files appearing/rotating, stops when the workload ends, and survives ordinary keepalive intervals without extending the task's wall-clock deadline. `shell` uses the existing SSH identity and a TTY. Signal, timeout and disconnect tests must preserve partial logs and still reap the follower and VM. A reader must not own or delay teardown.

Potential APIs are `rooms env build/inspect/ls/gc`, a Claude runner, explicit environment/file/profile inputs and live log/shell access. Add them as their use cases are implemented; none is implied to exist by these examples. Existing exit meanings stay: 0 success, workload failures under the current runner contract, 2 generic error/refusal or indeterminate evidence, 3 diff lane escape, 4 pool full. Do not assign 3 to refusal or 4 to missing Nix.

## Delivery and evidence

1. Build the Claude runner and useful live progress on the current sizing, repository and toolstore features. Add only task inputs needed by the specimen.
2. Demonstrate the forward proxy's registry restrictions. Independently prototype the fixed-upstream broker and SDK support.
3. Benchmark dependency warm-up using current snapshots. Prototype persistent environment/scratch restore only if it improves the measured workload. The cached environment build depends on the registry proxy **and** a proven snapshot topology; there is no circular phase dependency.
4. Consider imports from devcontainers/Dockerfiles only when a real repository needs them.

For the agent trial, write the prompt to `task.md` and pass that path. Use a pinned repository fixture with a known failing test, preserve its initial failure, and independently apply and validate the emitted patch on a fresh checkout. Observe the live stream and mid-run shell, then verify exit, artifacts and cleanup. Exit zero plus a nonempty patch is insufficient. Record model, inputs, task cost, elapsed time and failed attempts alongside the baseline.

For toolchain portability, run the same declared task on each supported architecture and rebuild twice per architecture; compare closure inputs/results rather than expecting cross-architecture bytes to match. For snapshot topology, prove clone write isolation, identity refusal, crash recovery and cleanup on real KVM before enabling it as a task path.

Measure warm start at one room, readiness at eight, dependency build time, private memory and scratch bytes. Previous timing targets (3-second warm start, 5-minute dependency build) are hypotheses, not measurements or new merge requirements. Guides and raw receipts should state exactly which path ran and what remains unimplemented.
