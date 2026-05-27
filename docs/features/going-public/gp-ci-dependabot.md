**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-ci-dependabot` (id: `tsk_01KSMXDS2DEKBMS3EN0H99B2ZR`), [going-public driver](driver.md)

# CI: add dependabot.yml for Cargo + GitHub Actions weekly updates

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Config | `.github/dependabot.yml` (new) | ~14 | 0 |
| **Total weighted** | | | **~0** |

Band: **amazing**.

## Goal

Keep Cargo deps + GitHub Actions versions current without manual intervention.

## Fix

Create `.github/dependabot.yml`:

```yaml
version: 2
updates:
  - package-ecosystem: cargo
    directory: "/"
    schedule:
      interval: weekly
    open-pull-requests-limit: 5

  - package-ecosystem: github-actions
    directory: "/"
    schedule:
      interval: weekly
```

Weekly cadence keeps PR noise low; the `cargo-audit` job (`gp-ci-cargo-audit`) flags actual security issues separately.

## Acceptance

- `.github/dependabot.yml` exists and validates.
- GitHub Settings → Code Security shows Dependabot as Active after push.
- Future weekly run opens PRs for out-of-date deps.

## Non-goals

- Renovate as an alternative — Dependabot is the GitHub-native default.
- Grouped updates (`groups:`) — defer until per-PR noise actually bites.
