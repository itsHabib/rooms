**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-contributing-md` (id: `tsk_01KSMXFD7DSW48DA1D7V36Y8FN`), [going-public driver](driver.md)

# P3: add CONTRIBUTING.md

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Docs | `CONTRIBUTING.md` (new) | ~50 | 0 |
| **Total weighted** | | | **~0** |

Band: **amazing**.

## Goal

Give external contributors a first-stop document covering dev setup, lint discipline, PR sizing, and the reviewer convention — framed for non-operator audiences (CLAUDE.md is agent-facing).

## Fix

Create `CONTRIBUTING.md` at the repo root, pulling the relevant pieces from `CLAUDE.md`:

- **Dev setup** — link to `scripts/setup-rooms-host.sh` and call out the Hyper-V / KVM requirement; mention the `make check` loop.
- **Lint discipline** — clippy strict, pedantic + nursery warn, complexity caps from `clippy.toml`. Don't add `#[allow(...)]` without justification.
- **PR sizing bands** — amazing < 500 weighted LOC, ideal < 700, stretch < 1000.
- **Reviewers** — Copilot + `@codex review` + `@claude review` per PR; CI green before merge.
- **Where to start** — link to `docs/follow-ups.md` (after `gp-docs-followups-file` lands) and the productionization driver manifest.
- **Link CLAUDE.md** — at the bottom, for deeper agent-facing context.

## Acceptance

- `CONTRIBUTING.md` exists at the repo root.
- GitHub's "Community Standards" tab shows it as detected.
- A first-time reader can run `make check` after reading without consulting other docs.

## Non-goals

- Setting up a public dev-environment script for non-Hyper-V hosts — real engineering ask; out of scope for v0.
- A formal RFC process — premature at v0.
