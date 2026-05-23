**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-23
**Related**: dossier task `ci-and-claude-workflow` (id: `tsk_01KSBE3JAWA0THZXWV6KBSBKTT`), [v0 spec](../rooms-v0/spec.md)

# CI matrix + Claude review workflow — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | — | 0 | 0 |
| Tests | — | 0 | 0 |
| Configs / workflows (0×) | `.github/workflows/ci.yml`, `.github/workflows/claude.yml` | ~80 | 0 |
| Docs (0×) | README CI section (~5 lines) | ~5 | 0 |
| **Total weighted** | | | **~0** |

Band: **amazing** (workflow YAML is 0× weight; only counted lines are README prose which is also 0×).

## Goal

Bring `rooms` up to the dossier CI bar: `make check` matrix runs on `ubuntu-latest`, the `@claude` review workflow is wired up, no commit lands without the bar being met.

## Functional

`.github/workflows/ci.yml` — three jobs on `ubuntu-latest`:
- `fmt`: `cargo fmt --all -- --check`
- `clippy`: `cargo clippy --all-targets --all-features -- -D warnings` with `Swatinem/rust-cache@v2`
- `test`: `cargo test --all-features` with cache

Triggers: `push` on `main`, `pull_request`. Permissions: `contents: read`. Same shape as `pers/dossier/.github/workflows/ci.yml`.

`.github/workflows/claude.yml` — mirrors dossier's exactly:
- Triggers: `issue_comment[created]`, `pull_request_review_comment[created]`, `issues[opened|assigned]`, `pull_request_review[submitted]`
- Job gated on `@claude` mention in the comment/review body
- Uses `anthropics/claude-code-action@v1` with `claude_code_oauth_token` from `secrets.CLAUDE_CODE_OAUTH_TOKEN`
- Permissions: `contents: read`, `pull-requests: read`, `issues: read`, `id-token: write`, `actions: read`

`Makefile` `check` target already exists from scaffolding. This task confirms the exact CI parity (`make check` = `fmt-check` + `lint` + `test`, same flags as the three CI jobs).

README addition: one paragraph under "Develop" — "CI matrix runs `make check` on Ubuntu; tagging `@claude` on a PR review triggers the Claude review workflow."

## Tradeoffs

- **Why Ubuntu-only?** `rooms` is Linux+KVM by design; Windows CI would only validate surface-level Rust code without testing the substrate. Multi-platform CI is wasted budget until a non-Linux consumer exists. Dossier made the same call but for softer reasons; here it's structural.
- **Why mirror dossier instead of deriving fresh?** Dossier's setup has battle-tested lints + caching across months of dogfood. Reuse the working shape; don't retune what works.
- **Why no `cargo test --no-default-features` axis?** v0 has no feature flags. Add when flags exist.

## EDs (engineering decisions)

- **ED-1: No Windows CI.** Out of scope per "Why Ubuntu-only" tradeoff.
- **ED-2: `Swatinem/rust-cache@v2` on clippy and test jobs.** Trades small cache-staleness risk for ~3× faster CI. Cache invalidates on `Cargo.lock` change.
- **ED-3: No `cargo-deny` or `cargo-audit` job yet.** Add when there are dependencies worth checking; v0's surface is thin (~10 crates).
- **ED-4: Claude workflow uses OAuth token, not API key.** Mirrors dossier; the action handles refresh.

## Validation

- Push a trivial commit to a feature branch; CI runs and passes all three jobs.
- Open a PR; CI runs in PR context with the cache primed.
- Comment `@claude review` on the PR; Claude workflow fires (verify via Actions tab).
- Local: `make check` matches the three CI jobs byte-for-byte (run `make check`, read `.github/workflows/ci.yml`, confirm parity).

## Risks

- **Hot path: Claude workflow auth.** If `CLAUDE_CODE_OAUTH_TOKEN` isn't set as a repo secret, the workflow silently no-ops. Acceptance includes smoke-testing the Claude workflow before closing the task.
- **Cache pollution from a bad first run.** If clippy fails partway through cache write, future runs may use a corrupted cache. Mitigation: cache keys include `Cargo.lock` hash; lockfile change invalidates.

## Out-of-scope

- Auto-merge bots, PR-size linting, conventional-commit enforcement.
- Multi-platform CI (Linux only).
- Release workflows (`cargo publish`, GitHub Releases).
- Coverage reporting.
- `cargo-deny` / supply-chain auditing.
- Any workflow that runs Firecracker inside CI — even though GitHub runners have `/dev/kvm`, that's a v0.2+ exploration tied to e2e tests.

## Implementation-plan

1. Copy `pers/dossier/.github/workflows/ci.yml` → `pers/rooms/.github/workflows/ci.yml`. No edits.
2. Copy `pers/dossier/.github/workflows/claude.yml` → `pers/rooms/.github/workflows/claude.yml`. No edits.
3. Confirm `Makefile`'s `check` target matches the three CI jobs (already true from scaffolding).
4. Add a one-paragraph "CI" section to README.md.
5. Commit + push.
6. Validate by triggering CI and the Claude workflow.

PR shape: one PR, ~0 weighted LOC. "amazing" band. Reviewers: Copilot, `@codex review`, `@claude review`.
