**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-docs-index` (id: `tsk_01KSMXF7CJ6RGJ890S2FXQ16WJ`), [going-public driver](driver.md)

# P3: add docs/README.md as the canonical docs entry point

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Docs | `docs/README.md` (new) | ~15 | 0 |
| **Total weighted** | | | **~0** |

Band: **amazing**.

## Goal

Anchor "start reading here" for `docs/` so visitors clicking into it from GitHub see a navigable index rather than a bare file list.

## Fix

Create `docs/README.md`:

```markdown
# rooms — docs

Start with the elevator pitch in [the top-level README](../README.md). For more depth:

1. **What and why** — [`vision.md`](vision.md). Non-goals, roadmap, the substrate framing.
2. **v0 contract** — [`features/rooms-v0/spec.md`](features/rooms-v0/spec.md). Single source of truth for v0.
3. **Architecture** — see [`CLAUDE.md`](../CLAUDE.md)'s "Architecture" section.
4. **Productionization manifest** — [`features/01-productionization/driver.md`](features/01-productionization/driver.md).
5. **Runner contract** — [`runner-contract.md`](runner-contract.md). Artifact layout consumers need.

Per-feature spec docs live under [`features/<slug>/spec.md`](features/).
```

## Acceptance

- `docs/README.md` exists and links the five docs above.
- All linked paths resolve (no broken relative links).
- GitHub renders the file as the directory landing page.

## Non-goals

- A generated docs site (mdbook, docusaurus) — markdown-on-disk is the portfolio default.
- Backfilling architecture diagrams.
