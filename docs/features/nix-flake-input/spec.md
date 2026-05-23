**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-23
**Related**: dossier task `nix-flake-input` (id: `tsk_01KSBE4ZP40VZ1D69RNZMFRRGA`), [v0 spec](../rooms-v0/spec.md), [rootfs-builder](../rootfs-builder/spec.md)

# Nix flake as deps spec — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source (1×) | `src/rootfs.rs` (flake input handling), `src/main.rs` (CLI), `src/domain.rs` (FlakeRef type) | ~300 | 300 |
| Configs (1×) | `profiles/node-dev/flake.nix`, `profiles/node-dev/flake.lock`, `profiles/node-dev/README.md` | ~150 | 150 |
| Tests (0.5×) | end-to-end flake build + boot test | ~120 | 60 |
| Docs (0×) | `docs/flakes.md` (operator-facing flake authoring guide) | ~120 | 0 |
| **Total weighted** | | | **~510** |

Band: **ideal** (under 700). If the Nix learning curve produces more code than estimated, split into:
- **PR-A**: flake input accepted, builds rootfs from flake, `--image` path still works (deprecation warning).
- **PR-B**: `profiles/node-dev/flake.nix` reference flake produces a rootfs equivalent to #6's debootstrap output.

## Goal

Flip the primitive's input from `--image <prebuilt>` to `--flake <path/to/flake.nix>`. The flake describes the deps; `rooms` invokes `nix build`, mounts the resulting rootfs image, boots the microVM from it.

This is what makes the "microVM-with-deps primitive" framing real. Until this lands, `rooms` accepts a prebuilt opaque image; after, the deps spec IS the input.

## Functional

**CLI change:**

```
rooms create --flake <path-or-url> --repo <repo> [--profile <name>]
rooms run    --flake <path-or-url> --repo <repo> --task <spec>
```

`<path-or-url>` is a Nix flake reference (`./profiles/node-dev`, `github:user/repo#node-dev`, etc.). `--image` continues to work with a deprecation warning; removed in v0.2.

**Behavior:**
1. Resolve flake reference to a local path (Nix handles git/github refs).
2. `nix build <flake>#rootfs --out-link <work-dir>/rootfs-link`.
3. The flake's `rootfs` output is the ext4 image path (convention; see "Flake contract" below).
4. Symlink-deref into the per-room overlay path; mount as before.
5. Boot Firecracker. Same as `--image` path from there.

**Flake contract** (documented in `docs/flakes.md`):

A `rooms`-compatible flake exposes:

```nix
{
  outputs = { self, nixpkgs, ... }: {
    packages.x86_64-linux.rootfs = ...;   # an ext4 image derivation
    packages.x86_64-linux.kernel = ...;   # optional; defaults to rooms' built-in kernel
  };
}
```

The `rootfs` output must be a derivation producing a single `.ext4` file (the build script can use `nixos-generate -f raw-efi` or a custom `runCommand` with `mkfs.ext4` + populate). The `kernel` output is optional; absent → use rooms' default `vmlinux`.

**Reference flake** (`profiles/node-dev/flake.nix`):
- Inputs: `nixpkgs` (pinned), `nixos-generators` (for the rootfs derivation helper).
- Output: a rootfs equivalent to #6's debootstrap-built image — Ubuntu-ish base with `git`, `openssh-server`, `nodejs_20`, `npm`, `@anthropic-ai/claude-code`, `@cursor/sdk` from #4.
- "Equivalent" means: same set of tools available at the same paths inside the booted VM. Exact distro can differ (Nix-built rootfs is most naturally NixOS, not Ubuntu — that's fine).

**Caching:**
- `nix build` is content-addressed; second build of the same flake is instant.
- Per-room work dir holds a symlink to the Nix store; substrate unmounts cleanly without affecting the store.

## Tradeoffs

- **Nix vs an `image-name = ...` registry.** A registry would be simpler but moves the deps problem outside the substrate (someone has to maintain the registry). Nix puts deps inline; aligns with portfolio principle of "tool for ourselves, not protocol for others."
- **NixOS-style rootfs vs Ubuntu-equivalent rootfs.** NixOS is what `nixos-generators` produces naturally; mimicking Ubuntu would require fighting Nix. Accept that the rootfs is NixOS-shaped; document the inside-VM tool paths in `docs/flakes.md`.
- **Flake reference: local path only vs git refs too.** Git refs (`github:user/repo#profile`) let profiles live in other repos. Free with Nix; accept the broader surface.
- **Backward compat with `--image`.** Worth it for one release to ease migration; remove in v0.2.

## EDs (engineering decisions)

- **ED-1: Flake output convention: `packages.<system>.rootfs`** (required) and `packages.<system>.kernel` (optional). Other names rejected with an actionable error.
- **ED-2: Accept NixOS as the natural rootfs shape.** Don't fight Nix to produce Ubuntu. Document the differences in `docs/flakes.md`.
- **ED-3: Shell out to `nix build`** (not embed Nix as a library). Simpler; `nix` binary required as a prereq, surfaced in `rooms doctor`.
- **ED-4: Pin `nixos-generators` in the reference flake.** Avoid breaking when upstream changes.
- **ED-5: Cache management is Nix's job.** rooms doesn't `nix-collect-garbage`; operator runs it themselves. Document in `docs/flakes.md`.
- **ED-6: `--image` deprecated, not removed in v0.1.** One-release migration window; gone in v0.2.

## Validation

- E2E: `rooms run --flake ./profiles/node-dev --repo <fixture> --task <task.md>` lands a patch. Same task as #4's e2e test (add a line to README); assert same patch shape.
- Cache test: run the same flake build twice; second should print "cached" or similar Nix output, not rebuild.
- Backward compat: `rooms run --image images/node-dev.ext4 ...` still works, prints a deprecation warning.
- Flake validation: pass a flake missing the `rootfs` output → actionable error mentioning the expected output name.
- Inside-VM tool check (after boot): SSH in, run `which git`, `which node`, `which claude`. All exit 0.

## Risks

- **Nix learning curve.** This task is the riskiest of the eight; budget for slip. Mitigation: PR-A / PR-B split if the reference flake proves harder than the input plumbing.
- **NixOS-vs-Ubuntu drift for downstream tools.** `claude-code` and `@cursor/sdk` may misbehave on NixOS due to dynamic linker assumptions. Mitigation: validate inside-VM tool checks early; add a `buildFHSEnv` wrapper if needed.
- **`nix build` is slow on first invocation.** First-build cold cache could take 5-10 minutes. Mitigation: pre-warm in `rooms doctor --deep`; document the one-time cost.
- **Flake reference resolution requires network.** `github:` refs hit GitHub; air-gapped envs need a local fork. Document.

## Out-of-scope

- A flake-authoring DSL or higher-level wrapper around Nix. Operators write Nix.
- Multi-arch flake outputs — `rooms-host` is x86_64 today.
- Auto-update of the reference flake's pinned inputs.
- `nix-collect-garbage` automation (operator runs).
- A profile registry (e.g. `rooms create --profile node-dev` resolves to a published flake). Defer until there are >3 profiles and a publish story.

## Implementation-plan

**PR-A (input plumbing):**
1. Add `FlakeRef` type in `src/domain.rs`. Parse `path | github | gitlab | etc.` per Nix's flake-ref grammar.
2. Add `--flake <ref>` CLI arg; mutually exclusive with `--image` (which is preserved with deprecation warning).
3. `src/rootfs.rs` gains `build_from_flake(ref) -> Result<PathBuf>` that shells `nix build <ref>#rootfs --out-link <work-dir>/rootfs-link`, validates output, returns the path.
4. Wire through `rooms create` → `rooms run`.
5. `rooms doctor` checks `nix --version` >= 2.18.
6. Tests for flake-ref parsing + error paths for missing output.

**PR-B (reference flake):**
7. Author `profiles/node-dev/flake.nix` using `nixos-generators` for the rootfs derivation.
8. Pin inputs via `flake.lock`.
9. E2E test that builds + boots the flake, runs a trivial task, asserts patch shape.
10. Write `docs/flakes.md` with the contract + authoring guide.
11. Add "Profile" section to README pointing to `profiles/node-dev/`.

PR shape: split unless A+B together fit under 700 weighted LOC. Reviewers: Copilot, `@codex review`, `@claude review`.

**Sequencing note:** Depends on #6 (rootfs-builder). The debootstrap path from #6 is the *reference* for what the flake's `rootfs` output must be functionally equivalent to (same tools, same paths).
