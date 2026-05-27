**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-github-templates` (id: `tsk_01KSMXFW4NGPS2HCZZ0V6PWXMX`), [going-public driver](driver.md)

# P3: add .github/ISSUE_TEMPLATE + PULL_REQUEST_TEMPLATE.md

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Templates | `.github/ISSUE_TEMPLATE/bug_report.md` (new), `.github/ISSUE_TEMPLATE/feature_request.md` (new), `.github/PULL_REQUEST_TEMPLATE.md` (new) | ~60 | 0 |
| **Total weighted** | | | **~0** |

Band: **amazing**.

## Goal

Structured intake for external bug reports + feature requests; documented PR shape for contributors.

## Fix

Create three template files:

`.github/ISSUE_TEMPLATE/bug_report.md` — sections for:
- What happened
- What you expected
- Reproduction steps
- rooms-host setup (host OS, KVM nested-virt status)
- Kernel + Firecracker versions

`.github/ISSUE_TEMPLATE/feature_request.md` — sections for:
- Use case (what problem are you solving?)
- What does the substrate need to provide
- Alternatives considered

`.github/PULL_REQUEST_TEMPLATE.md` — Summary / Closes / What this adds / Changes / Validation. Matches the convention used in batch 1 PR bodies.

## Acceptance

- All three files exist under `.github/`.
- "New issue" UI on GitHub surfaces both templates as options.
- PR creation auto-populates the body with the template.

## Non-goals

- Linking templates to GitHub Discussions / Projects — set up once issues actually flow.
- Localized templates — English-only at v0.
