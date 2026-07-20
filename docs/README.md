# rooms — docs

Start with the elevator pitch in [the top-level README](../README.md). For more depth:

1. **What and why** — [`vision.md`](vision.md). Non-goals, roadmap, the substrate framing.
2. **v0 contract** — [`features/rooms-v0/spec.md`](features/rooms-v0/spec.md). Single source of truth for v0.
3. **Architecture** — see [`CLAUDE.md`](../CLAUDE.md)'s "Architecture" section.
4. **Productionization manifest** — [`features/01-productionization/driver.md`](features/01-productionization/driver.md).
5. **Runner contract** — [`runner-contract.md`](runner-contract.md). Artifact layout consumers need.
6. **Doctor preflight gate** — [`preflight.md`](preflight.md). Every host/e2e run preflights on `rooms doctor`; FAIL aborts.
7. **Portfolio experiments** — [`product-directions.md`](product-directions.md). Ranked ways attributable parallel rooms can calibrate workers, reviewers, and grants.

Per-feature spec docs live under [`features/<slug>/spec.md`](features/).
