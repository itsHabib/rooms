**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-23
**Related**: dossier task `docs-vision-and-readme` (id: `tsk_01KSBE572WQ1VJ3E471BVXX202`), [v0 spec](../rooms-v0/spec.md)

# Docs: vision, README, CLAUDE.md — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Docs (0×) | `docs/vision.md`, `README.md`, `CLAUDE.md` | ~400 | 0 |
| **Total weighted** | | | **~0** |

Band: **amazing** (all 0× weight).

## Goal

Round-trip documentation so a new contributor — or future-you on a fresh machine — can land in the `rooms` repo and orient in 5 minutes. Mirrors dossier's docs shape so anyone familiar with the portfolio recognizes the layout immediately.

## Functional

**`docs/vision.md`** — what `rooms` is, why, what we're explicitly NOT building. Parallel to `pers/dossier/docs/vision.md`.

Sections:
- **What rooms is** — the primitive: spawn a clean Firecracker microVM with specified deps, run a command, collect artifacts, destroy it.
- **Why** — the substrate that lets ship/work-driver/future replay all share one isolation story instead of each reinventing.
- **First consumer: LLM agents** — but the substrate doesn't know about agents; that's the layering.
- **What rooms is NOT** — explicitly out: Codespaces-but-local, dev workspace UX, web preview, multi-tenant, port forwarding, persistent rooms across reboots.
- **Where this sits in the portfolio** — ship calls rooms; work-driver crash recovery will use it; dossier tracks the work.
- **Roadmap (light)** — v0 = POC + productionization; v0.1 = Nix + ship integration + cursor SDK runner; v0.2 = snapshots + fork + replay receipts.

Length target: 100-150 lines.

**`README.md`** — what the repo is, how to run it, what to expect.

Sections:
- **rooms** — one-paragraph elevator pitch.
- **Status** — current shipped milestone (POC / v0.1 / etc.).
- **Prereqs** — Linux + KVM host, Firecracker installed, `CURSOR_API_KEY` or `ANTHROPIC_API_KEY` depending on runner, Rust toolchain.
- **Quickstart** — `rooms run --repo <path> --task <task.md>` example with expected output.
- **CLI surface** — `create`, `exec`, `collect`, `destroy`, `doctor`, `run` — brief description of each.
- **Develop** — `make check`, where specs live (`docs/features/<slug>/spec.md`), PR conventions (Copilot, @codex, @claude reviewers).
- **CI** — `make check` on Ubuntu; `@claude review` triggers the workflow.
- **Architecture** — pointer to `docs/vision.md` and `docs/features/rooms-v0/spec.md`.

Length target: 100-150 lines.

**`CLAUDE.md`** — notes for agents working on this repo. Mirrors the dossier/ship CLAUDE.md shape.

Sections:
- **rooms** — one-paragraph identity.
- **State** — what's shipped, what's next.
- **Dev workbench** — full block matching dossier+ship:
  - dossier (project memory, primary)
  - ship (workflow execution)
  - huddle (multi-seat coordination)
  - playwright (browser automation)
  - `/work-driver` (orchestration)
  - `/work-driver-prep` (planning)
  - `/worktree-*` (worktree management)
  - "The loop" diagram from dossier/ship
- **Architecture** — strict layered dependency direction (domain → firecracker / rootfs / transport → runner → main); same shape as dossier and tower.
- **Docs** — pointers to vision.md, spec docs, runner-contract.md, flakes.md.
- **Develop** — `make check` matrix, PR sizing bands, reviewer set, lint discipline summary.
- **How rooms fits** — pointer to ship integration, future work-driver crash recovery.
- **Conventions** — errors lowercase, no design-doc refs in code comments, atomic writes for artifact files.
- **Common gotchas** — `/dev/kvm` missing means nested virt off; SSH key mismatches; rootfs size hitting the ext4 image cap.
- **When you're stuck** — pointers to spec docs vs in-code investigation.

Length target: 250-350 lines (matches dossier/ship CLAUDE.md depth).

## Tradeoffs

- **Three docs vs one big README.** Three lets each serve its purpose without bloat: vision is for "should we use this?", README is for "how do I run it?", CLAUDE.md is for "I'm working inside this repo." Same split dossier and ship use.
- **CLAUDE.md dev-workbench duplication.** The dev-workbench block is ~150 lines duplicated across every portfolio repo. The `/dev-workbench` skill exists exactly to regenerate it; this task includes a one-time scaffolding via that skill, not handwritten.
- **Document the not-yet-built.** Vision mentions v0.2 snapshots; spec table mentions ship integration. Risk of overselling. Mitigation: vision's "Roadmap" section is labeled "light"; nothing claims shipped.

## EDs (engineering decisions)

- **ED-1: CLAUDE.md dev-workbench block is generated via `/dev-workbench` skill**, not handwritten. Same shape across portfolio repos by construction.
- **ED-2: README does NOT include a long architectural deep-dive.** Architecture lives in `docs/vision.md` (operator-facing) and spec docs (contract-facing). README is a starting point, not a syllabus.
- **ED-3: Vision doc states explicit non-goals.** Per `feedback_opinionated_not_generic`, the opinion is "your-laptop-first ephemeral microVMs, Nix-described, portfolio-tool-not-protocol." Non-goals make the opinion legible.
- **ED-4: Roadmap is in vision.md, not README.** README is for "how to use today"; vision is for "where this is heading." Reduces README staleness.
- **ED-5: No PR template.** Reviewers per repo CLAUDE.md is enough; PR templates add friction without value at solo-dev scale.

## Validation

- Read each doc cold (as a new contributor would). Can you land + run a `rooms run` within 5 minutes from README? Within 10 minutes understand the architecture from vision?
- All cross-references are real (`docs/features/rooms-v0/spec.md`, `docs/flakes.md`, etc.). Run a link check or visual grep.
- `CLAUDE.md` dev-workbench block matches dossier's verbatim (modulo project-specific verbs). Diff against dossier's to confirm.
- No claims of shipped features that aren't shipped (e.g. don't claim "Nix flake support" until #7 lands).

## Risks

- **Docs go stale.** README mentions CLI flags that change; vision claims roadmap items that get reprioritized. Mitigation: docs are touched whenever behavior changes (spec convention).
- **CLAUDE.md dev-workbench block drift across portfolio.** When dossier's CLAUDE.md gets a new workbench entry, rooms' won't auto-update. Mitigation: `/dev-workbench` skill re-run on a cadence.

## Out-of-scope

- A separate `CONTRIBUTING.md` — at solo-dev scale, CLAUDE.md covers contribution norms.
- API docs / rustdoc — generated by `cargo doc`; not a write task.
- Logo, favicon, branding.
- Marketing site / GitHub Pages.
- Translated docs.

## Implementation-plan

1. Write `docs/vision.md` from scratch using `pers/dossier/docs/vision.md` as the structural template.
2. Write `README.md` covering the sections above.
3. Run the `/dev-workbench` skill against this repo to generate the CLAUDE.md dev-workbench block.
4. Write the rest of CLAUDE.md (Architecture, Develop, How rooms fits, Conventions, Common gotchas, When you're stuck) using `pers/dossier/CLAUDE.md` as the template.
5. Link-check: grep for any `docs/`, `pers/`, or relative paths in the three docs; verify each exists.
6. Read cold from the README → quickstart → vision → CLAUDE.md. Tune anything that doesn't flow.

PR shape: one PR, ~0 weighted LOC. "amazing" band. Reviewers: Copilot, `@codex review`, `@claude review`.

**Sequencing note:** Parallel-safe with #1, #2, #3, #6. Should land *after* the substrate works (so README's quickstart isn't fiction) but can be drafted in parallel and amended at merge time.
