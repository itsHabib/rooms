# Local hardening and useful formal checks

**Status:** design proposal, not an implementation claim or a new mandatory delivery process.
**Owner:** @itsHabib
**Related:** [follow-ups](../../follow-ups.md), [diff contract](../rooms-diff/spec.md), [snapshot contract](../snapshot-fork-replay/spec.md), [environment proposal](https://github.com/itsHabib/rooms/pull/118), [disposable hosts](https://github.com/itsHabib/rooms/pull/117).

## Purpose and current baseline

Rooms should safely execute an untrusted workload, retain useful evidence and clean up after crashes. Fix demonstrated failures directly. Use a model or proof when it helps expose a difficult interleaving or validate a specific invariant; do not build a ladder of tools that every change must climb.

The original audit predates merged sizing/repository/scratch support (#120), toolstores (#121) and snapshot toolstore identity (#123). Re-anchor implementation work to current code (this reconciliation used main `f702be4`), not the original audit's line numbers. The issues below are investigation/fix targets until a regression demonstrates that they remain present. This document itself fixes none of them.

## Threat model and ownership

The operator and installed Rooms binary are trusted. A guest workload is untrusted, including when it has root inside its VM. Network peers are untrusted. Concurrent Rooms commands may race. Assets are the host filesystem, sibling rooms, process identities, credentials, immutable snapshot inputs, accurate evidence and resource cleanup.

Jailer isolation, egress rules, sealed snapshot identities and artifact path validation defend different boundaries. Guest stdout, exit markers, JSON and changeset enumeration remain guest claims. A path-safe archive can still lie about its content or consume excessive resources. Keeping a credential behind a host broker prevents direct key extraction but does not prevent authorized requests or spend.

Authoritative registry and snapshot metadata stay **root-owned and non-writable by ordinary users**. Non-root `ls` can consume a redacted read-only projection. `gc` and `kill` use a narrowly authorized privileged operation that validates ownership, process start identity, path containment and symlink handling against authoritative state. Do not chown the state tree to make non-root commands work: an unprivileged writer could otherwise redirect a later privileged cleanup.

Only the requested output directory and validated artifacts may be assigned to the invoking user's numeric UID/GID. They must not become an input the privileged cleaner trusts. Missing or malformed invoking-user identity must not trigger a broad recursive chown.

Credentials supplied after resume taint the room through host-owned provenance. A snapshot request after injection must be rejected, including memory capture; guest marker scans cannot establish that an arbitrary disk or `snapshot.mem` contains no secret. Previously persisted credential-bearing URLs need a read/redaction compatibility path and a targeted cleanup recommendation, not only a new-write rejection.

## Concrete hardening targets

| Target | Required evidence when implemented |
| --- | --- |
| SIGTERM, SIGINT and deadline teardown | Spawn the actual Rooms child running a long workload, signal it from a parent, verify its exit and all owned VM/network/disk/process cleanup; do not signal the unit-test runner |
| Credential-bearing repository URLs | Refuse before persistence/logging; load a legacy record and verify redaction without echoing its credential |
| Read-only execution image | Verify shared sealed images work concurrently; reject unsupported custom images before launch; test any deliberate maintenance path separately |
| Output ownership and state access | User can read exported output/status; cannot modify privileged registry inputs or steer privileged deletion |
| Partial logs | A timed-out or cancelled task retains the bytes already produced; collection does not truncate them |
| Kill identity and exit contract | PID reuse/missing start identity cannot signal a replacement process; missing/dead/live cases have documented outcomes |
| Doctor correctness | Probe jailer file access as UID 999, tap openability with read/write access, and dirty image recovery state; unsupported inspection is not green |
| Egress cleanup failure | Report incomplete teardown with residual identity; do not emit a successful cleanup receipt |

Tests should reproduce the actual old behavior at the relevant revision and pass with the fix. Do not assume every item fails at one historical main revision or claim "six bugs" when the batch contains a different set. Keep an explicit per-item disposition and reuse already-landed fixes.

The current CLI has `--readonly-rootfs`, and a cold `--disk` implies it; **`--writable-image` does not currently exist**. A proposed read-only default must preserve a clear custom-image admission check for the expected overlay init and kernel support. Unsupported images are refused with a concrete migration path before allocating a VM. If a maintenance write mode is needed, introduce and document it explicitly, refuse sealed files, and keep it outside the default execution path. Do not imply a compatibility flag already exists.

## Host-derived filesystem changes

Host inspection can remove reliance on the guest's enumeration only when the host has the actual writable backing state. Current cold disk-backed runs have that state; tmpfs-based runs and current warm snapshots do not expose an ext4 scratch upper to the host. Persistent scratch restore depends on the separately proven topology in the environment proposal. Never invent `scratch.ext4` for a path that ran with tmpfs.

After the VM is stopped and no writer remains, check the scratch filesystem consistency before inspection. A dirty superblock (`needs_recovery`), pending journal, failed consistency check or unknown consistency makes the result incomplete/exit 2 even if directory listings look readable. Read-only `debugfs` does not replay a journal and can otherwise return stale metadata without an error. The first implementation refuses these inputs; any future recovery must operate on an isolated disposable copy with a defined recovery-and-recheck procedure before inspection can assert completeness. Inspect only a confirmed-consistent retained private scratch disk with a read-only userspace filesystem parser. Avoid mounting guest-controlled ext4 in the host kernel. `debugfs` is one candidate, still an untrusted-input parser: run it without privilege, with CPU/memory/output/time bounds and only the staged disk accessible. Validate tool/version capability before claiming complete enumeration.

The algorithm must:

1. Recursively walk each directory explicitly; `debugfs ls` is not recursive. Parse names/inodes without treating names as commands or following guest symlinks. Record regular files, directories, symlinks and special entries; detect malformed or repeated traversal.
2. Read overlay whiteout and opaque-directory metadata. A char device 0:0 is a whiteout where supported by the selected overlay format. Enumerate the matching lower tree and expand opaque-directory hiding into the affected deleted descendants.
3. Compare upper entries against the effective immutable lower tree to distinguish additions, content/metadata changes and unchanged copy-ups. The upper alone cannot distinguish added from modified. Respect the recorded lower-layer order and handle type replacements.
4. Apply lane rules to the complete derived changeset and retain source identities and parser diagnostics. Preserve the evidence before deleting scratch through ordinary teardown.

Missing `ea_get`/xattr support, unsupported overlay encodings, truncated traversal, a parser timeout or unavailable lower data means **incomplete**. Do not label the result trusted or claim no lane escape. Report indeterminate (exit 2) while still cleaning up. A userspace parser does not magically close all enumeration gaps; tests need adversarial names, whiteouts, opaque nested directories, symlinks, special files and malformed images.

Use a **versioned changeset v2** for required evidence fields: `source` (`host-scratch` or `guest`), completeness, and explicit trust scope. Existing v1 documents deserialize through a compatibility path as guest/unknown, never trusted by default. Consumers must reject unsupported schema versions and treat unknown/incomplete evidence as indeterminate when deciding absence of a lane escape. Exit 3 retains its existing lane-escape meaning; host-derived filesystem changes do not validate the guest's test results.

## Formal work that earns its cost

Start with one recurring race: slot ownership during registry/snapshot GC. The model covers the transaction, lease and sweep together. Replaying only a toy allocator misses the shipping race.

| Seam | Candidate tool | Useful question |
| --- | --- | --- |
| Slot claim, registry publication, snapshot transaction and GC | Quint/Apalache plus Rust replay | Can a sweep reap state held by a live transaction? |
| Room lifecycle and kill identity | Small state model if ordinary regression tests miss interleavings | Can a stale record target a replacement process? |
| Snapshot provenance | State model plus serialization tests | Can serialized guest state or post-injection capture manufacture neutrality? |
| Teardown crash cuts | TLA+/TLC if crash fixtures leave an unresolved ordering problem | Can GC identify every resource left by a dead owner? |
| Disposable host ownership | Deterministic concurrency tests first | Can concurrent creation overwrite the ownership token? |
| Pure parsing/path decisions | Existing properties, targeted fuzzing; Kani where useful | Can hostile inputs escape containment or panic? |

Anchor code maps to real symbols at the reviewed revision. Current seams include `artifacts::safe_join`, `ensure_inside_out_dir`, runner tar-member validation, and the private slot `indexed_claim` path. There is no `artifacts::validate_path` or public `slot::Pool` to wrap. Any extraction of a pure seam is a small explicit implementation change tested against existing callers, not an assertion that an API already exists.

A proposed replay harness decodes an ITF trace and invokes actual Rust operations against a temporary state base. Narrow in-module test adapters may expose private transitions; they must call shipping allocator, registry and snapshot transaction code, including their locks and publication operations. Observations are compared after each action. If a process race cannot be reproduced by the adapter, use controlled subprocess barriers rather than claim coverage from a separately reimplemented model.

## One reproducible model experiment

Store only the artifacts this experiment needs under `formal/slot-gc/`: model, claims and finite bounds, source map, fixture, an owned mutation patch and a replay command.

1. Identify the exact historical fault and confirm the current shipping transition. Preserve the original source revision or a checked-in patch that reintroduces that specific ordering bug in a disposable checkout.
2. Produce a counterexample under explicitly recorded finite process, slot and step bounds; save the ITF fixture. Comparing serialized fixture bytes is only a stability check, not a test of Rust behavior.
3. Replay the fixed fixture against current shipping Rust: it must preserve the invariant. Apply the owned mutation patch in isolation and run the **same Rust replay**: it must fail for the intended reason. If the patch no longer applies, fail the fixture-maintenance check rather than silently skip it.
4. Run additional generated traces against model and shipping adapters, comparing observations step by step. Preserve generator seed and trace on divergence.
5. Record what was reproduced, what was bounded, what was abstracted and what was not checked. Add the cheap deterministic regression to normal CI once it works; run expensive model exploration only where its findings justify the cost.

The historical pre-#108 state may guide the mutation, but naming that PR alone is not an executable mutant. No permanent extra nightly/PR jobs or tool dependencies are committed by this design.

For a teardown TLC experiment, explicitly choose finite bounds, for example two rooms, two resource identities per class and a bounded sequence of allocation/crash/GC steps. State fairness assumptions for any eventual-cleanup claim. Exhausting that model proves neither unbounded liveness nor Firecracker/kernel behavior. Reproduce any counterexample against real resource ownership and cleanup before claiming a Rooms defect or fix.

Kani harnesses must compile against the actual target and `cfg(unix)` boundaries on the supported Linux CI runner. Do not claim listed functions are cfg-free without inspecting them, and do not remove production branches to make a proof pass. Unsupported constructs or unaffordable bounds are a documented limitation; a normal regression or property test may be the better solution.

## Delivery and validation

Land direct correctness fixes as they are ready. Host-derived diff can follow on the cold scratch path without waiting for every agent-environment feature; warm restore coverage waits for the proven persistent scratch contract. Keep v1 compatibility and the indeterminate path visible to consumers.

Run the single slot/GC model experiment alongside useful implementation work only when resources permit. Continue to additional seams if it finds a defect, reproduces a meaningful historical bug or provides useful bounded assurance at reasonable maintenance cost. A tool producing no counterexample is not proof of no bugs, and it need not be deleted merely because it did not discover something new.

For each implementation PR, run the documented checks and the relevant regression on the affected platform. KVM and process/resource tests need a real Linux host; local parser tests cannot stand in for them. Publish commands, revisions, inputs, raw results and limitations in the PR and reusable guide. Keep claims narrower than the evidence, and avoid adding a mandatory proof, model or review stage to unrelated work.
