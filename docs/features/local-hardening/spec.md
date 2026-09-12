# Local hardening & formal methods — Technical Design Document

**Status:** draft / proposal — NOT a build commitment. The artifact we decide from.
**Owner:** @itsHabib
**Date:** 2026-09-11
**Related:** [follow-ups](../../follow-ups.md) · [rooms-diff](../rooms-diff/spec.md) · [vsock-secrets](../vsock-secrets/spec.md) · [multi-room-pool / slot allocator](../multi-room-pool/pool-slot-allocator.md) · [snapshot/fork/replay §4](../snapshot-fork-replay/spec.md) · [going-public proptest / mutants plans](../going-public/) · portfolio `formal-methods` (model-checking modules 7, 8, 12 are anchored in rooms) · sibling TDD [agent-dev environments](../agent-dev-environments/spec.md) · dossier project `rooms`

> **Reviewers — focus areas:** §3 threat model (is the adversarial guest the right principal to design around?), §4 D2 (host-side truth for `rooms diff` via the scratch disk, parsed in userspace), §4 D4 (the ladder: which rung each seam gets and why), §9 gate G-F (formal artifacts must be load-bearing or we stop climbing).

## 1. Problem & hypothesis

rooms is about to become a daily sandbox for agents (sibling TDD) and then run on
cloud boxes (`scripts/box.sh`). Before either, the local substrate needs two
things it does not have:

1. **A written threat model and a hardening pass against it.** `docs/follow-ups.md`
   holds ~40 open items; several are correctness holes a hostile guest or a
   plain crash can exploit today. Verified in code during the 2026-09-11 review
   (not yet run): `rooms run` handles SIGINT but not SIGTERM
   (`src/main.rs:4228` vs the clone path at `:1587-1594`), so runway's deadline
   cancel skips teardown; a cursor `--repo` URL with embedded credentials is
   persisted to `room.json` (`src/main.rs:4633`); the default `--command` boot is
   read-write against a bind-mounted shared image (`src/firecracker.rs:1553`)
   which the builder now marks immutable (`build-rootfs-alpine.sh:481`); the
   `--out` tar lands root-owned under sudo; a timed-out run truncates the partial
   logs it is about to collect. And the biggest one, already recorded: `rooms
   diff` trusts enumeration the guest performs about itself (follow-up
   2026-06-20, "adversarial-guest hole").
2. **Checked claims about the concurrent core.** The slot pool, gc/snapshot
   sweep, kill identity re-probe, provenance seal and clone-fleet teardown are
   where the follow-ups cluster and where a test samples but cannot exhaust.
   The portfolio's `formal-methods` track already models the gc/snapshot race
   (modules 7–8) and replays model traces against real code (module 12); none of
   that is asserted in this repo's CI.

**Hypothesis.** Hardening is a threat model plus a fix batch with a regression
test per fix — cheap and committed. Formal methods pay off on exactly the seams
where the follow-ups keep reopening, *if* the artifact is load-bearing: a
checker either reproduces a known bug as a deterministic fixture the fixed code
passes or finds a new one. We climb the ladder one rung at a time and stop the
moment a rung stops paying.

**Non-goals:** a full proof of rooms; Lean unless a universal law appears (§10);
modelling Firecracker or the kernel; the cloud control plane (that is the box
work).

## 2. Requirements

**Hardening (H)**

- H-FR1 A threat model in `docs/threat-model.md`: principals, assets, trust boundaries, what each existing control defends, what it does not.
- H-FR2 Every verified bug above fixed with a regression test (`make check` or e2e) that fails on the pre-fix code.
- H-FR3 `rooms diff` derives its changeset from host-readable state, not from the guest's own report.
- H-FR4 Doctor has no known false-greens (`jailer_file_access` probes as uid 999; `tap_openable` checks `O_RDWR`; dirty-image `needs_recovery` check).
- H-FR5 Non-root `rooms ls/gc/kill` work against root-created state; `--out` is owned by the invoking user.

**Formal (F)**

- F-FR1 A `formal/` directory in the repo with one sub-dir per seam: `model.qnt` (or `.tla`), `CLAIMS.md`, `SOURCE_MAP.md`, `fixtures/`, `judge.sh` — the `formal-methods/entries` shape.
- F-FR2 CI runs `quint typecheck` + `quint run` (sampled) on every PR in seconds; `quint verify` (Apalache, exhaustive-to-bound) runs nightly and on a label.
- F-FR3 Model traces (ITF JSON) replay against the Rust state machines in `tests/formal_replay.rs`; a divergence fails CI.
- F-FR4 Pure decision functions get bounded proofs with Kani (`cargo kani`), under the existing proptests, not instead of them.

**Non-functional**

| Concern | Target |
| --- | --- |
| PR-time cost | `formal` CI job ≤ 90 s (typecheck + sampled run); Kani ≤ 5 min, nightly |
| Drift detection | every `judge.sh` exits nonzero if a claim's fixture no longer reproduces |
| Reviewability | each model ≤ 300 lines with a `SOURCE_MAP.md` naming `file:line` for every abstracted transition |
| Honesty | `CLAIMS.md` has a "Not proved" section, always |

## 3. Threat model (summary; full doc is task H1.1)

**Principals.** *Operator* (trusted). *rooms binary* — runs as root under sudo, trusted but the thing most likely to be wrong. *Guest workload* — the agent plus everything it fetches; **untrusted from its first tool call**, with NOPASSWD sudo inside the VM. *Network peers* — untrusted. *Concurrent rooms invocations* — honest but racing.

**Assets.** Host filesystem outside the state base; other rooms' state and slots; secrets (host env, custody, `--secret` values); snapshot neutrality (no secret in `snapshot.mem`); exact teardown (no leaked VM, tap, netns, mount, disk); evidence integrity (`result.json`, `changeset.json`, witness pcap, lifecycle log).

**Boundaries and today's controls.** Jailer chroot + uid drop (firecracker as 999); virtio only (no shared fs); egress chain per tap; secrets after resume over vsock; `ImmutableFileIdentity` on snapshot inputs; artifact path validation on `--out` tar (`src/artifacts.rs`, proptested); `refs/rooms/base` pin.

**Where the guest crosses the boundary today.** (a) `changeset.json` — produced in-guest by the same root-capable user the agent is; (b) `result.json` / `EXIT=` marker — parsed from guest stdout; (c) `/workspace/out` tar — validated for path escape, unbounded in size (follow-up 2026-05-30, tabled); (d) any `SendEnv` secret in the guest environment is readable by the agent; (e) a guest with the API key can spend it — the proxy in the sibling TDD (D5) is the fix.

**Where the host crosses its own boundary.** Root-owned state that non-root verbs cannot read; a writable shared image; signal paths that skip teardown; doctor checks that probe as the wrong principal.

## 4. Key decisions & trade-offs

**D1 — Threat model before fixes; fixes before formal.** Order matters: the
model names the invariants, the fixes are cheap and remove noise, the models
then check what is left. *Alternative:* start with the formal work since it is
the interesting part. *Why not:* half the follow-ups are plain bugs; modelling
around them wastes the bound.

**D2 — Host-side truth for `rooms diff`, parsed in userspace.** Once the
writable layer is a scratch disk (sibling TDD Phase A), the host can enumerate
the overlay upper *after shutdown* without asking the guest. Do it with
e2fsprogs (`debugfs -R 'ls -l'`, `stat`, `ea_get` for `trusted.overlay.opaque`)
rather than a kernel `mount -o loop`: mounting an untrusted ext4 hands the guest
the kernel's filesystem parser as an attack surface; `debugfs` is userspace and
read-only. This closes the adversarial-guest hole and, as a side effect, the
symlink / special-file / directory / opaque-whiteout enumeration gaps
(follow-ups 2026-06-20/21), because the walk is ours. *Trade-off:* diff is only
available post-run, never live; and `command` runs without a scratch disk keep
the in-guest path, flagged `trusted: false` in `changeset.json`.

**D3 — Read-only image for every runner; writable is a maintenance mode.**
`--writable-image` remains for image maintenance only, refuses on an immutable
file, and prints why. Every run is disposable by construction and the shared
image can be opened read-only + shared across concurrent rooms (follow-up
2026-06-20, jailer perms).

**D4 — The ladder, applied per seam.** Property tests stay the floor
everywhere. Quint/Apalache for interleavings; TLA+/TLC where crash-cuts matter
(the `fm-crash-cut-publish` shape); differential replay to bind model to code;
Kani for pure decision functions; Lean not planned.

| Seam | Question | Rung | Why this rung |
| --- | --- | --- | --- |
| Slot claim / lease / return + gc & snapshot sweep (`src/slot.rs`, `src/registry.rs`) | can gc ever reap a slot a live transaction holds? | Quint → Apalache; **replay** against `slot.rs` | interleaving across processes; port of `formal-methods` modules 7–8, known counterexample (follow-up 2026-08-02, #108) |
| Room lifecycle + `kill` identity (`src/registry.rs:29-40`, `room::probe`) | can kill signal the wrong pid, or leave an un-killable room? | Quint | pid reuse + `pid_starttime: None` (follow-up 2026-08-09) is an ordering question |
| Provenance seal (`src/room.rs`, `src/snapshot.rs`) | can a tainted room ever be snapshotted; can a secret precede resume? | Quint (safety only) | small monotone state machine; the deserialize-trust follow-up (2026-07-27) becomes an invariant |
| Clone-fleet teardown (`src/clonenet.rs`, `src/restore_exec.rs`) | with a crash at every cut, does teardown leave zero leaks? | TLA+/TLC crash-cut | the exact-teardown gate is the phase-2 hard check; crash-cut exploration is what TLC does well |
| `box.sh up/down` ownership (`scripts/box.sh`) | is the accepted lookup/delete race the *only* residual? | Quint (tiny) | turns a prose deferral (#117) into a checked residual |
| `artifacts::validate_path`, `egress_audit::classify_host`, `is_lane_escape`, `ImmutableFileIdentity` compare, version parse | no input escapes / misclassifies within a bound | **Kani** | pure functions; proptest already exists — Kani proves the bound rather than samples it |
| Parsers: lifecycle events, `result.json`, matrix manifest, `env.toml` | never panic on hostile bytes | `cargo fuzz`, nightly | parsers, not protocols |

*Alternative for the pure core:* Verus or Creusot give unbounded proofs but need
annotation-heavy rewrites; Kani runs on the code as written. *Alternative for
conformance:* Stateright (Rust-native model checker) would remove the Quint→Rust
gap by modelling in Rust directly. Not chosen for round one because the Quint
models and the differential-replay pattern already exist in the portfolio;
revisit if the ITF replay proves brittle (§10 Q2).

**D5 — Models live in this repo, not in `formal-methods`.** The curriculum stays
where it is; the *asserted* artifacts move next to the code they constrain, so
a refactor of `slot.rs` breaks `formal/slot-gc/judge.sh` in the same PR.

**D6 — Mutation testing measures the floor.** `cargo-mutants` (planned in
`going-public/gp-ci-mutants-workflow.md`) runs weekly on the seams above; a
surviving mutant in a modelled seam is a finding for either the proptest or the
model, recorded in the seam's `CLAIMS.md`.

## 5. Data model

```
docs/threat-model.md
formal/README.md                     the ladder + how to run
formal/<seam>/model.qnt | Model.tla  ≤ 300 lines
formal/<seam>/SOURCE_MAP.md          transition → file:line
formal/<seam>/CLAIMS.md              Proved (bounds) / Reproduced fixture / Mapped not proved / Not proved
formal/<seam>/fixtures/*.itf.json    counterexample traces, normalized, byte-compared
formal/<seam>/judge.sh               offline; exit ≠ 0 on drift
tests/formal_replay.rs               ITF → Rust driver per seam (feature `formal`)
.github/workflows/formal.yml         PR: typecheck + run; nightly: verify + kani + fuzz
```

Replay harness shape (`tests/formal_replay.rs`):

```rust
trait Replayable { type Action: DeserializeOwned; type Obs: PartialEq + Debug;
    fn apply(&mut self, a: &Self::Action) -> Self::Obs; }
fn replay<M: Replayable>(trace: &Itf, m: &mut M) -> Result<(), Divergence>  // step-indexed diff
```

Each seam implements `Replayable` over its real types (`slot::Pool`,
`registry::RoomState`, `room::Provenance`) with an in-memory state base under
`tempdir`, so the code under replay is the shipping code, not a port.

## 6. API contract

No new CLI verbs. New surfaces:

- `changeset.json` gains `"source": "host-scratch" | "guest"` and `"trusted": bool`; `rooms diff` exits 2 (indeterminate) when `trusted=false` and an escape is claimed absent.
- `rooms doctor` gains `image_clean` (`needs_recovery` unset), `jailer_uid_read` (probe as 999), `tap_rdwr`.
- `result.json` gains `"cancelled_by": "sigint" | "sigterm" | "max_wall" | null`.
- `make formal` (typecheck + run), `make formal-verify`, `make kani`, `make fuzz`.
- Every `formal/<seam>/judge.sh` prints one line per claim: `ok|drift <claim>`.

## 7. Key flows

**7.1 Hardening fix, the required shape.** For each item: (1) a failing test at
the follow-up's seam — unit where the seam is pure, e2e otherwise; (2) the
smallest fix; (3) the follow-up moved to *Closed* with the PR link. No fix
without (1). Items sharing a seam ship in one PR under the sizing bands.

**7.2 Host-side diff.** After `workload_exited` and shutdown, before scratch
deletion: `debugfs -R "ls -l -p /upper" scratch.ext4` recursively; for each
entry classify added / modified / deleted (char-dev 0:0 whiteout) / opaque dir
(`ea_get trusted.overlay.opaque`); apply lane rules on the host; write
`changeset.json{source:"host-scratch",trusted:true}`; then delete scratch.
`debugfs` failure → `changeset.json` absent, `rooms diff` exits 2, room still
torn down.

**7.3 A model earning its place** (slot/gc, round one):
1. Port `formal-methods` module 7's `SlotSweep.qnt` into `formal/slot-gc/`, with `SOURCE_MAP.md` re-anchored to current `slot.rs` / `registry.rs` lines.
2. `quint verify --invariant=noReapOfHeldSlot` against the **pre-#108** transition (the mutant) → counterexample; normalize → `fixtures/gc-reaps-held-slot.itf.json`; `judge.sh` byte-compares.
3. Same invariant against the current transition → no violation within bound (state in `CLAIMS.md` with the bound).
4. Export a batch of random `quint run` traces; `tests/formal_replay.rs` drives `slot::Pool` with them; observations must match step by step.
5. CI on PR: steps 1, 4 (seconds). Nightly: step 2–3 (Apalache).

**7.4 Crash-cut teardown (TLC).** State = {netns, veth pair, tap, chain, mount,
scratch, snapshot lease, room dir}; actions = each allocate/teardown step;
`Crash` enabled at every step; invariant `AfterGc: no leaked resource whose owner
is dead`. Expected finding class: an ordering where a crash between two steps
leaves a resource `gc` cannot attribute (the meta-less-leak follow-up,
2026-06-28, is exactly that shape).

## 8. Failure model

- A model that finds nothing and reproduces nothing is deleted at the gate, not kept for decoration.
- A `judge.sh` drift on a PR blocks like a test: either the code changed behaviour (fix code or update the model *and* its `CLAIMS.md` in the same PR) or the model was wrong (say so in `CLAIMS.md`).
- Apalache/JDK missing locally → `make formal` still runs the npm-only rungs; `verify` is nightly by design so no PR waits on the JVM.
- Kani unsupported construct → the harness narrows its bound and records it; never `#[cfg]`-out production code to satisfy the prover.

## 9. Rollout / implementation plan

| Phase | Goal | High-level tasks | Depends on | Band | Gate |
| --- | --- | --- | --- | --- | --- |
| **H1 — threat model + fix batch 1** | Named invariants; the verified bugs gone | `docs/threat-model.md`; SIGTERM teardown parity + `cancelled_by`; creds-in-URL refusal; read-only default + `--writable-image`; `--out` chown + non-root `ls/gc`; partial-log preservation; kill exit-code contract (2026-06-29 ×2) | — | ideal (~600) | **G-H** |
| **H2 — host-side truth** | `rooms diff` and doctor stop trusting the wrong principal | debugfs enumeration of scratch upper; `trusted` field; doctor `image_clean` / `jailer_uid_read` / `tap_rdwr`; jailer RO shared perms; egress-cleanup WARN gating | H1, sibling A (scratch disk) | ideal (~650) | — |
| **F1 — formal spine** | Models asserted in CI | `formal/README.md` + ladder; `formal/slot-gc` port with fixture; `formal/provenance` (new, small); `formal.yml` PR job (typecheck + run); Makefile targets | H1 | ideal (~500, mostly `.qnt` at 0.5×) | — |
| **F2 — differential replay** | Model bound to code | `tests/formal_replay.rs` + `Replayable` for `slot::Pool` and `Provenance`; ITF export in `judge.sh`; nightly `verify` | F1 | ideal (~500) | **G-F** |
| **F3 — Kani + fuzz on the pure core** | Bounded proofs where proptest samples | Kani harnesses for `validate_path`, `classify_host`, `is_lane_escape`, identity compare, version parse; `cargo fuzz` targets for the four parsers; nightly job | F1 | amazing (~400) | — |
| **F4 — lifecycle, teardown, box models** | The remaining seams | `formal/room-kill` (Quint); `formal/fleet-teardown` (TLA+/TLC crash-cut); `formal/box-ownership` (Quint) | F2, G-F | stretch (~800) | — |
| **F5 — mutants weekly** | Measure the floor | `cargo-mutants` on modelled seams; surviving mutants → `CLAIMS.md` | F3 | amazing | — |

H1, H2, F1, F2 are the commitment. F3 can run in parallel with F2. F4 and F5
are gated on G-F.

## 10. Open questions

1. **`debugfs` on the rooms-host.** e2fsprogs is present on Ubuntu; is the `ea_get` subcommand available in the shipped version (needed for opaque dirs)? If not, opaque-dir detection stays a known gap in `CLAIMS.md` for the diff seam.
2. **ITF replay brittleness.** If keeping Quint state names aligned with Rust types costs more than it catches, switch the conformance rung to Stateright (model in Rust, check in Rust). Decide at G-F with evidence from F2.
3. **Which rung for egress semantics?** Allowlist composition (`none` ⊂ `allowlist` ⊂ open; proxy + chain agreement) may be a *law* — the Lean-shaped question. Not planned; revisit if the sibling TDD's proxy lands and the two mechanisms disagree in practice.
4. **Should `trusted:false` diffs exit 2 or 0 with a warning?** Consumers (runway) treat exit 2 as indeterminate today; proposal is 2.
5. **Kani on `#[cfg(unix)]` code.** The pure functions listed are cfg-free; confirm before F3 so the harnesses compile on the CI runner.

## 11. Validation plan

**G-H (after H1).** `make check` green; e2e on the rooms-host green including
new regression tests; each of the six verified bugs has a test that fails on
`main@e8c4504` and passes on the fix (recorded in the PR as the failing-then-
passing run). Binary.

**G-F (after F2).** The formal artifacts are load-bearing: at least one of
(a) `formal/slot-gc` reproduces the 2026-08-02 race as a fixture the pre-#108
mutant fails and current code passes, *and* the replay harness catches a
deliberately introduced ordering bug in `slot.rs` (a mutant PR that CI must
reject); or (b) a model finds a new bug that becomes a follow-up with a fix.
If neither, F4/F5 are cancelled and the `formal/` dir is reduced to what proved
something — the ladder says stop, not climb.
