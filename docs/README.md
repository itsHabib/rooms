# rooms — docs

Start with the elevator pitch in [the top-level README](../README.md). For more depth:

1. **What and why** — [`vision.md`](vision.md). Non-goals, roadmap, the substrate framing.
2. **v0 contract** — [`features/rooms-v0/spec.md`](features/rooms-v0/spec.md). Single source of truth for v0.
3. **Architecture** — see [`CLAUDE.md`](../CLAUDE.md)'s "Architecture" section.
4. **Productionization manifest** — [`features/01-productionization/driver.md`](features/01-productionization/driver.md).
5. **Runner contract** — [`runner-contract.md`](runner-contract.md). Artifact layout consumers need.
6. **Doctor preflight gate** — [`preflight.md`](preflight.md). Every host/e2e run preflights on `rooms doctor`; FAIL aborts.
7. **Portfolio experiments** — [`product-directions.md`](product-directions.md). Ranked ways attributable parallel rooms can calibrate workers, reviewers, and grants.
8. **Snapshot matrix proof** — [`experiments/snapshot-matrix-2026-08-25.md`](experiments/snapshot-matrix-2026-08-25.md). One immutable snapshot, distinct positive/mutant commands, separate witnessed evidence, clean teardown.
9. **Reproducible cloud lab** — [`experiments/cloud-lab.md`](experiments/cloud-lab.md). Rust measurement runner, retained recipes, portable snapshots, failure lessons and provider cleanup. [All ten experiments](experiments/ten-experiments.md) retain separate verdicts.
10. **Cloud experiment ideas** — [`experiments/cloud-experiment-ideas.md`](experiments/cloud-experiment-ideas.md). Backlog of real-KVM tests: density, a shared Nix store, fork-the-world agent branching, rooms that travel, agents inside rooms.

Per-feature spec docs live under [`features/<slug>/spec.md`](features/).
