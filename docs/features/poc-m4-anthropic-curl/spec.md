**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-24
**Related**: dossier task `poc-m4-anthropic-curl` (id: `tsk_01KSC5S5N7THMBV2BBEY9NEEJK`), [rooms v0 spec](../rooms-v0/spec.md), [poc-m3 spec](../poc-m3-ssh-access/spec.md), POC phase `00-poc-implementation`

# POC m4: curl Anthropic from inside the room (POC upper bar)

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source (1×) | `src/main.rs` (changes), `src/lib.rs` (+1), `src/runner.rs` (new) | ~170 | 170 |
| Scripts (1×) | `scripts/bake-rootfs-ssh.sh` (+ ~3 LOC) | ~3 | 3 |
| Tests (0.5×) | `src/runner.rs` mod tests, `src/main.rs` CLI shape test | ~80 | 40 |
| Docs (0×) | spec doc, README `--command` mention | ~400 | 0 |
| **Total weighted** | | | **~213** |

Band: **amazing**.

## Goal

Demonstrate the POC upper bar:

```sh
export ANTHROPIC_API_KEY=sk-ant-...    # already in ~/.bashrc on rooms-host
rooms run --image ~/rooms/images/rootfs.ext4 \
    --command 'curl -s -H "x-api-key: $ANTHROPIC_API_KEY" \
                   -H "anthropic-version: 2023-06-01" \
                   -H "content-type: application/json" \
                   -d "{\"model\":\"claude-3-5-sonnet-latest\",\"max_tokens\":256,\"messages\":[{\"role\":\"user\",\"content\":\"say hello\"}]}" \
                   https://api.anthropic.com/v1/messages'
```

Expected: the real Anthropic JSON response (`{"id":"msg_...","content":[{"type":"text","text":"Hello! ..."}],...}`) lands on the host's stdout. `rooms` exit code = 0. No leaked firecracker processes or per-room state dirs.

Composes the m1+m2+m3 milestones (boot + network + SSH) end-to-end with an arbitrary guest-side command — *not* an Anthropic-specific shortcut.

## Critical framing — substrate surface, not agent surface

The dossier task body proposes `rooms run --image ... --prompt "say hello"`. **We are not shipping `--prompt`.** Per [`rooms-v0/spec.md`](../rooms-v0/spec.md) line 21 ("Agent invocation is `rooms exec <id> -- claude -p < task.md`; the substrate sees `exec a command`, not `run an agent`") and the architecture framing in the project handoff ("rooms is a SUBSTRATE: spawn microVM with deps → run a command → collect artifacts → destroy"), the m4 surface is `--command <STRING>` — a substrate-shaped primitive. An Anthropic prompt is one of many things an operator (or future consumer) might want to run; the substrate doesn't know which.

This is a non-trivial pivot from the dossier task body. The new surface generalizes to N future consumers (ship's `RoomCursorRunner`, `/work-driver` crash recovery, future replay, manual operator use) — they each compose the same `--command` primitive without rooms knowing about them. A `--prompt` flag would couple rooms to "this is for Anthropic", which is exactly what the substrate vs. consumer layering is designed to prevent.

The dossier task's *acceptance test* still holds: an operator can run the curl invocation above (an instance of `--command`) and get a Claude JSON response back. The verb shape just changes from `--prompt "say hello"` to `--command '<curl ...>'`.

## Functional

### CLI changes (`src/main.rs`)

Extend the existing `Run` subcommand:

```rust
Run {
    /// Path to the rootfs image (ext4).
    #[arg(long)]
    image: PathBuf,
    /// Keep the room alive until Ctrl-C instead of the default 3s auto-shutdown.
    /// Mutually exclusive with `--command`.
    #[arg(long, conflicts_with = "command")]
    keep: bool,
    /// Run a single command in the guest via SSH, capture its stdout/stderr on
    /// host stdout/stderr, propagate its exit code, then shut down.
    /// Mutually exclusive with `--keep`.
    #[arg(long, conflicts_with = "keep", value_parser = non_empty_command)]
    command: Option<String>,
}

fn non_empty_command(s: &str) -> Result<String, String> {
    if s.is_empty() {
        Err("--command must not be empty".to_owned())
    } else {
        Ok(s.to_owned())
    }
}
```

`clap`'s `conflicts_with` is reciprocal — declaring it on one side is sufficient, but the spec puts it on both for human readability. Cursor MUST NOT collapse it to one side (the symmetry survives clippy and makes the constraint visible in either flag's help text). The `value_parser` rejects `--command ''` at parse time — see ED-11 for why this is worth three lines.

Three behaviors based on flags:

| `--keep` | `--command` | Behavior |
|---|---|---|
| false | `None` | boot, sleep 3s, shut down (existing) |
| true  | `None` | boot, block on Ctrl-C, shut down (existing) |
| false | `Some(c)` | boot, wait for sshd, exec `c` in guest, shut down, exit with guest's exit code |
| true  | `Some(c)` | rejected by clap |

### New module: `src/runner.rs`

Two public async functions. No structs — keep the surface minimal; the planned `runner.rs` per [rooms-v0/spec.md](../rooms-v0/spec.md) grows artifact capture and contract enforcement in productionization (`runner-contract` task), but m4 only needs `exec a command in the guest`.

```rust
//! Run commands inside a booted microVM, via SSH.
//!
//! POC: shells out to the `ssh` client. A native russh/openssh-rs client is
//! a productionization concern.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{debug, info};

/// Probe the guest's sshd until it accepts a pubkey connection, or `timeout`
/// elapses. Returns the last underlying error on timeout so failure modes
/// (network down, key not baked, sshd never started) are debuggable from the
/// surface error.
pub async fn wait_for_ssh(
    guest_ip: &str,
    key_path: &Path,
    timeout: Duration,
) -> Result<()> { /* ... */ }

/// Exec `command` in the guest as root via SSH. Wires the guest's stdin from
/// /dev/null, and inherits stdout / stderr so guest output flows to the host's
/// fds directly (operators can pipe `rooms run --command '...' | jq`).
///
/// Returns the guest command's exit code clamped to `0..=255`:
/// - guest exited normally with `ExitStatus.code() == Some(n)` → returns `n`
///   (always in `0..=255` per POSIX). The clamp to `u8` happens in the caller,
///   not here — keep the wider type so the caller can distinguish "ran" from
///   any future "did not run" sentinel.
/// - guest killed by signal: returns `128 + sig.unwrap_or(0)`. Per
///   `ExitStatusExt::signal()`, `sig` is `Option<i32>`, but in practice it's
///   `Some(n)` with `n <= 64` whenever the underlying status was a signal kill,
///   so the result stays in `0..=255`.
///
/// SSH-internal errors (host unreachable, key rejected, ssh-binary missing)
/// surface as `Err` with anyhow context; the bail message names the most likely
/// cause. The SSH-internal exit code 255 is NOT translated — it passes through
/// as `Ok(255)` and is indistinguishable from "remote command exited 255".
/// Documented tradeoff (see Risks).
///
/// Forwards `ANTHROPIC_API_KEY` from the host process env via SSH's `SendEnv`
/// option. The matching `AcceptEnv ANTHROPIC_API_KEY` lives in the rootfs's
/// `/etc/ssh/sshd_config`, baked by `scripts/bake-rootfs-ssh.sh`.
pub async fn exec_in_guest(
    guest_ip: &str,
    key_path: &Path,
    command: &str,
) -> Result<i32> { /* ... */ }
```

The implementation MUST set `kill_on_drop(true)` on the spawned ssh `Command` so that callers can cancel via dropping the future (the `--command` arm in `main.rs` uses `tokio::select!` with `ctrl_c()` for clean cancellation — see ED-15).

Implementation requirements:

**`wait_for_ssh`:**

- Loop body: spawn `ssh -i <key> -o BatchMode=yes -o ConnectTimeout=2 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR root@<ip> true` with `stdin=null, stdout=null, stderr=piped` (capture stderr for the error message).
- On exit code 0: return `Ok(())`.
- On any non-zero exit: record stderr as `last_err`, sleep 1s, retry.
- On `timeout` elapsed: `anyhow::bail!("sshd at {guest_ip} did not accept connections within {timeout:?} (last stderr: {last_err})")`.
- Default timeout passed by caller: 60s. (Bionic boot + sshd is typically <10s; 60s buys headroom for cold-start variability without making "stuck" feel like "still working".)
- The probe MUST use `BatchMode=yes` so missing/wrong keys fail fast instead of prompting on stdin (which would block forever in a non-tty context).

**`exec_in_guest`:**

```rust
let status = Command::new("ssh")
    .args([
        "-i", key_path.to_str().context("key path not utf-8")?,
        "-o", "BatchMode=yes",
        "-o", "ConnectTimeout=5",
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "-o", "LogLevel=ERROR",
        "-o", "SendEnv=ANTHROPIC_API_KEY",
        &format!("root@{guest_ip}"),
        "--",
        command,
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit())
    .kill_on_drop(true)
    .status()
    .await
    .context("failed to spawn ssh; is openssh-client installed?")?;
```

Exit-code mapping (after the status returns):

- `status.code()` is `Some(n)` if exited normally → return `n`.
- `status.code()` is `None` if killed by signal → on Unix, return `128 + std::os::unix::process::ExitStatusExt::signal(&status).unwrap_or(0)`. (The `signal()` method returns `Option<i32>` of the underlying signal number, `<= 64` on real Unix.)
- Ctrl-C handling for the `--command` flow lives in the *caller* (`main.rs`'s `post_boot` uses `tokio::select!` between this future and `tokio::signal::ctrl_c()`; on Ctrl-C, this future drops and `kill_on_drop(true)` SIGKILLs the ssh child). `exec_in_guest` itself doesn't install a SIGINT handler — see ED-15 for the rationale.

**`-- <command>` argument shape:** the `--` separator before `<command>` tells SSH to treat the entire next argument as the remote command verbatim. SSH joins all args after the host with spaces and passes the joined string to the remote shell (per the OpenSSH man page). Because we pass exactly one argument after `--`, no joining happens; the guest's `bash -c` receives `command` byte-for-byte. **Cursor MUST NOT** split `command` on whitespace into multiple args — the Anthropic curl invocation has spaces inside quoted JSON, which would corrupt the request.

### `dispatch` refactor (`src/main.rs`)

**Two structural changes** beyond adding the flag:

1. `dispatch` return type changes from `Result<()>` to `Result<u8>` so the guest exit code can be threaded to `ExitCode::from`. `Doctor` and the default / `--keep` arms return `Ok(0)`; `--command` returns `Ok(guest_exit_code_clamped_to_u8)`.
2. The `Run` body is extracted into two helpers — `run_room` (owns boot + always-shutdown) and `post_boot` (the inner three-way match). This is **non-optional**: keeping it inline in `dispatch` runs into two real problems — (a) the inner match arms use `?` and would early-return from `dispatch`, bypassing the `vm.shutdown()` call (the exact bug m1's reviewer caught: `kill_on_drop` reaps the firecracker child, but only `shutdown()` removes the per-room state dir); (b) `clippy::cognitive_complexity` (cap 20) trips on the nested matches.

```rust
async fn dispatch(cli: Cli) -> Result<u8> {
    match cli.command {
        Command::Run { image, keep, command } => run_room(image, keep, command).await,
        Command::Doctor => {
            info!("rooms doctor");
            anyhow::bail!("doctor: not yet implemented (POC in flight)")
        }
    }
}

async fn run_room(image: PathBuf, keep: bool, command: Option<String>) -> Result<u8> {
    info!(?image, keep, command = ?command.as_deref(), "rooms run");

    let kernel = image
        .parent()
        .ok_or_else(|| anyhow::anyhow!("--image has no parent directory: {}", image.display()))?
        .join("vmlinux.bin");
    anyhow::ensure!(
        kernel.exists(),
        "kernel not found at {}; expected sibling of --image",
        kernel.display()
    );

    let network = firecracker::NetworkConfig {
        tap_name: "tap-fc0".to_owned(),
        guest_ip: "172.16.0.2".to_owned(),
        gateway_ip: "172.16.0.1".to_owned(),
        netmask: "255.255.255.0".to_owned(),
    };
    let key = key_path()?;
    let mut vm = firecracker::boot(&kernel, &image, Some(&network)).await?;

    // Always run shutdown, whatever post_boot returns. `post_boot` is a separate
    // function so its internal `?` returns from itself, NOT from run_room — that's
    // what guarantees the shutdown call below runs on the error paths.
    let outcome = post_boot(&network, &key, keep, command, &mut vm).await;
    if let Err(e) = vm.shutdown().await {
        warn!(error = %e, "shutdown reported an error after post-boot");
    }
    outcome
}

async fn post_boot(
    network: &firecracker::NetworkConfig,
    key: &Path,
    keep: bool,
    command: Option<String>,
    vm: &mut firecracker::BootedVm,
) -> Result<u8> {
    match (keep, command) {
        (true, _) => {
            info!(
                guest_ip = %network.guest_ip,
                "microVM is up; Ctrl-C to shut down (try `ping {}` from another shell)",
                network.guest_ip,
            );
            tokio::signal::ctrl_c().await.context("waiting for Ctrl-C")?;
            Ok(0)
        }
        (false, Some(cmd)) => {
            runner::wait_for_ssh(&network.guest_ip, key, Duration::from_secs(60)).await?;
            // tokio::select! between exec and ctrl_c so a Ctrl-C during the guest
            // command drops the exec future (kill_on_drop SIGKILLs the ssh child),
            // returns Ok(130), and run_room's vm.shutdown() runs cleanly. Without
            // this, the default SIGINT terminates rooms before shutdown can fire.
            let exec_fut = runner::exec_in_guest(&network.guest_ip, key, &cmd);
            tokio::pin!(exec_fut);
            tokio::select! {
                res = &mut exec_fut => {
                    let code = res?;
                    Ok(u8::try_from(code).unwrap_or(2))
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("ctrl-c received during command; aborting and shutting down");
                    Ok(130)
                }
            }
        }
        (false, None) => {
            tokio::time::sleep(Duration::from_secs(3)).await;
            if vm.is_alive()? {
                info!("microVM is up; shutting down (POC: no exec yet)");
                Ok(0)
            } else {
                anyhow::bail!("firecracker exited prematurely; check serial output")
            }
        }
    }
}

fn key_path() -> Result<PathBuf> {
    // Convention: same dedicated key m3's bake script creates / reuses.
    // No env-var override at the m4 layer; --key-path lands in productionization
    // when per-room dynamic keys become a thing.
    //
    // Bail (don't fall back to "/root") if HOME is unset — silent /root fallback
    // would mask "you ran with sudo" footguns, where the key actually lives in
    // the operator's home. The bake script itself refuses to run under sudo for
    // the same reason.
    let home = std::env::var("HOME")
        .context("HOME env var unset; rooms needs it to locate ~/.ssh/id_rooms")?;
    Ok(PathBuf::from(home).join(".ssh/id_rooms"))
}

#[tokio::main]
async fn main() -> ExitCode {
    // ... tracing init (switched to stderr — see ED-3) ...

    let cli = Cli::parse();
    match dispatch(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            warn!(error = %err, "command failed");
            ExitCode::from(2)
        }
    }
}
```

`u8::try_from(code).unwrap_or(2)` is intentional: by the contract in `runner::exec_in_guest`, `code` is already `0..=255`. The `unwrap_or(2)` is defense-in-depth for any future bug that pushes it out of range — fall back to the same exit code the `Err` path uses, not a misleading `0`. **Cursor MUST NOT** "simplify" this to `code as u8` (clippy `cast_possible_truncation` will fire) or to `unwrap_or(0)` (silently swallows failures).

Cursor MUST NOT add a `--key-path` CLI flag in this PR. The convention `~/.ssh/id_rooms` is set by `scripts/bake-rootfs-ssh.sh`; making it operator-configurable belongs in the per-room dynamic key work (`harden-firecracker-control`).

**While editing `main.rs`:** delete the m3/m4 phase reference in the `Run` subcommand's doc comment (currently `"POC scope: boot + shutdown only. --command / --task / repo transport / agent runner land in later milestones (m3 = SSH access, m4 = curl Anthropic from inside, then the cursor-sdk-runner task)."`) per the no-design-doc-refs convention in CLAUDE.md. Replace with a behavior-only one-liner like `"Boot a microVM and optionally run a single command in it via SSH."`. The "intentionally absent in this PR" comment block right below it can also go — `--command` is no longer absent.

### Tracing → stderr (`src/main.rs`)

```rust
tracing_subscriber::fmt()
    .with_env_filter(env_filter)
    .with_writer(std::io::stderr)
    .init();
```

Required so the guest command's stdout flows cleanly to the host's stdout. Without this, `rooms run --command 'curl ...' | jq` would interleave tracing INFO lines into the JSON. Existing log lines (`info!(...)`, `warn!(...)`) move to stderr.

### `scripts/bake-rootfs-ssh.sh` changes

**Three small edits:**

1. **§1 prereqs:** append `ssh` to the prereqs loop so missing `openssh-client` fails at bake time with a clear apt-install hint, instead of m4's `rooms run --command ...` failing at exec time with a cryptic "failed to spawn ssh" anyhow context:

   ```sh
   for cmd in sudo mount mountpoint losetup ssh ssh-keygen sed grep tee e2fsck awk; do
   ```

   `ssh` is the openssh-client binary. On Ubuntu Server 24.04 it's installed by default, but stripped-down images or container-derived hosts may not have it; either way, this catches it once at bake time rather than per-run.

2. **§7 sshd_config:** add one line after the existing three `set_directive` calls:

   ```sh
   set_directive PermitRootLogin yes
   set_directive PubkeyAuthentication yes
   set_directive PasswordAuthentication no
   set_directive AcceptEnv ANTHROPIC_API_KEY   # NEW for m4
   ```

   No quotes around the value — the existing `set_directive` function takes positional args; bash strips quotes anyway, and the sshd_config emitted line is unquoted (`AcceptEnv ANTHROPIC_API_KEY`). Quoting in the call site would be syntactic noise.

3. **§9 final logging:** append one more line:

   ```sh
   log "    env passthrough:    set ANTHROPIC_API_KEY before invoking rooms (SendEnv plumbs it to the guest)"
   ```

Operators must re-run `bash scripts/bake-rootfs-ssh.sh` once after pulling m4 — the script is idempotent (the existing match-or-append logic handles the "missing → append" case for the new directive cleanly).

**Cursor MUST NOT** change the existing `set_directive` shape to support multi-token values. The current regex matches single-token values (one whitespace block then one token), and `ANTHROPIC_API_KEY` is one token. Multi-var passthrough is a post-POC concern.

### README

Add one new example line under the existing `## CLI (POC)` block in `README.md` (lines 22–26 in the current file), keeping the existing two examples intact:

```sh
rooms run --image ~/rooms/images/rootfs.ext4          # boot + auto-shutdown after 3s
rooms run --image ~/rooms/images/rootfs.ext4 --keep   # boot until Ctrl-C
rooms run --image ~/rooms/images/rootfs.ext4 \
    --command 'curl -s https://example.com'           # boot, ssh in, run cmd, shut down
rooms doctor                                          # host env check (stub)
```

Add one paragraph below the existing CLI block:

> `--command` and `--keep` are mutually exclusive. The command runs in the guest's bash via SSH; stdout / stderr flow to the host's stdout / stderr and the guest's exit code becomes rooms's exit code. `ANTHROPIC_API_KEY` is forwarded into the guest via SSH's `SendEnv` if set in the operator's shell.

No deeper restructure. Productionization (`docs-vision-and-readme`) owns the full README rewrite.

## Tradeoffs

- **`--command "<one string>"` vs `-- <args...>`.** Single-string wins for POC because the Anthropic curl invocation needs shell parsing on the guest (quoted JSON, `$ANTHROPIC_API_KEY` expansion, header strings with spaces). `-- <args>` would force the operator to pre-tokenize and lose shell semantics. The substrate's "exec a command in a shell" contract maps naturally to a single string.
- **Shell out to `ssh` vs use `russh`/`openssh-rs`.** Shell out wins for POC because (a) we already shell out to `firecracker` and `curl`, (b) `ssh` is universally available on Linux hosts, (c) `russh` is well-maintained but adds a non-trivial dep + key-format handling for ~50 LOC of savings. Productionization (`harden-firecracker-control` or a later task) can swap if needed.
- **`SendEnv ANTHROPIC_API_KEY` (sshd-baked allowlist) vs stdin-secrets-file vs argv-env.** SendEnv wins because: (a) the key never appears in argv (`ps`-safe), (b) it's the standard SSH idiom, (c) future env vars are one-line additions to `bake-rootfs-ssh.sh`, (d) the alternative — writing a one-shot secrets file via SSH stdin — couples stdin (which we want available for future use), and the third alternative — `env ANTHROPIC_API_KEY=$KEY ssh ...` — leaks via the host's ssh-process argv.
- **Bake script gains AcceptEnv (one operator-visible re-bake) vs runtime injection.** Re-bake wins because runtime injection (e.g., writing `/root/.ssh/environment` via SSH first) requires `PermitUserEnvironment yes` in sshd_config — same operator re-bake friction, weaker security posture (any process inside the guest can read the env file). Re-bake is one-time per host.
- **Exit-code propagation: SSH's exit code passes through verbatim.** Means the SSH-internal 255 code collides with "remote command exited 255". POC accepts; productionization (`runner-contract`) can wrap the remote command with `; echo "ROOMS-EXIT:$?"` to disambiguate.
- **No `--env <KEY=VAL>` flag in m4.** The operator must export `ANTHROPIC_API_KEY` in their shell before invoking rooms. Adding `--env` is YAGNI for POC (one variable, one consumer); generic env plumbing arrives with `cursor-sdk-runner` or `runner-contract` post-POC.
- **Boot retry / health check is "ssh probe", not "wait for boot banner over serial".** Pubkey SSH is the property we actually care about for `--command` — probing it directly (loop `ssh ... true` until success) is the simplest possible check. Parsing the serial console for "Started OpenBSD Secure Shell server" would be brittle (init system differences, log line wording) and prove less.
- **Tracing routed to stderr.** Means existing log readers that grep stdout will miss the lines. Acceptable — the only log readers right now are operators reading interactive output, who don't distinguish stdout from stderr in the terminal. Productionization may add a `--json-logs` flag for tooling consumers.

## EDs (engineering decisions)

- **ED-1: `--command` takes a single string, not trailing `-- <args>`.** Shell metacharacters in the curl invocation (quoted JSON, `$VAR`, header strings with spaces) need guest-side bash parsing. Pre-tokenization in the operator's shell would corrupt the request body.
- **ED-2: `--command` and `--keep` mutually exclusive via `clap conflicts_with`.** Combining is meaningless: keep is for manual SSH from a second shell; command is one-shot. Reciprocal `conflicts_with` for human-readable help.
- **ED-3: Tracing init switches from default stdout to `std::io::stderr` writer.** Keeps host stdout clean so guest command stdout flows through unmolested. `rooms run --command 'curl ...' | jq` works.
- **ED-4: SSH options on every invocation.** `BatchMode=yes` (no password prompt fallback — fail fast if pubkey not accepted); `ConnectTimeout=5` (don't hang on dead network); `StrictHostKeyChecking=no` + `UserKnownHostsFile=/dev/null` (bionic regenerates host keys each boot — same m3 reasoning); `LogLevel=ERROR` (suppress "Warning: Permanently added..." spam on stderr); `SendEnv=ANTHROPIC_API_KEY` (env passthrough).
- **ED-5: Env passthrough via `SendEnv` + `AcceptEnv`, not argv or stdin-file.** SendEnv keeps the key out of `ps` output and out of the rust binary's argv. AcceptEnv on the sshd side gates which vars are accepted (one explicit allowlist entry, not `*`).
- **ED-6: New `src/runner.rs` module, not inline in `main.rs`.** Matches the planned layered architecture (`runner` is the canonical home for "exec a command in the guest" per [rooms-v0/spec.md](../rooms-v0/spec.md)). Seeds the layer for `cursor-sdk-runner` post-POC. `main.rs` already imports `firecracker`; importing `runner` follows the same pattern.
- **ED-7: `runner.rs` exposes free functions, no struct.** POC scope is two operations (probe, exec); a struct would be ceremony. `runner-contract` post-POC can wrap with a `RunnerArtifacts` struct when artifact capture lands.
- **ED-8: `dispatch` returns `Result<u8>` not `Result<()>`; the `Run` body is extracted into `run_room` + `post_boot` helpers.** Clean way to thread the guest exit code to `ExitCode::from(code)`. The two-helper split is required for the always-shutdown guarantee: `post_boot`'s internal `?` returns from `post_boot`, not from `run_room`, so `run_room`'s `vm.shutdown()` call always fires (this is the bug m1's reviewer caught — `kill_on_drop` reaps the child but only `shutdown()` removes the per-room state dir). Bonus: keeps both helpers well under the clippy `cognitive_complexity = 20` cap.
- **ED-9: `wait_for_ssh` probes `ssh ... true`, not `tcp_connect 22` or serial-console grep.** The property we care about is "pubkey SSH works"; probe that directly. TCP-connect-to-22 would pass before sshd is ready to accept keys; serial-console grep is fragile across init system / log line wording.
- **ED-10: Probe timeout 60s, probe interval 1s.** Bionic boot + sshd up is typically <10s; 60s is headroom without "stuck" feeling. 1s interval is responsive enough without flooding logs.
- **ED-11: `--command` arg rejects the empty string at clap parse time.** Use clap's `value_parser`:
  ```rust
  #[arg(long, conflicts_with = "keep", value_parser = non_empty_command)]
  command: Option<String>,
  ```
  with a helper `fn non_empty_command(s: &str) -> Result<String, String> { if s.is_empty() { Err("--command must not be empty".to_owned()) } else { Ok(s.to_owned()) } }`. Reason: `ssh root@host --  ""` drops into a login shell that immediately sees EOF on stdin and exits 0 — *currently* harmless because `Stdio::null()` is set on stdin, but a fragile assumption ("any future stdin wiring change makes this hang forever"). Rejecting at parse time costs three lines and removes the trap.
- **ED-12: `key_path()` is a private helper in `main.rs`; returns `Result<PathBuf>`, bails if HOME is unset.** No fallback to `/root` — a silent fallback masks "you ran with sudo" footguns where the key actually lives in the operator's `~/.ssh/id_rooms`. The bake script already refuses to run under sudo for the same reason; `rooms` should mirror that opinion. No `--key-path` CLI flag in m4 — operator-configurable key paths are `harden-firecracker-control`.
- **ED-13: `bake-rootfs-ssh.sh` gains exactly one new `set_directive AcceptEnv ANTHROPIC_API_KEY` line + `ssh` added to the prereq loop.** No restructure of `set_directive` to support multi-token values; no widening to `AcceptEnv *_API_KEY`. One-token, one-var, deliberate.
- **ED-14: No new dependencies in `Cargo.toml`.** Shelling out to `ssh` requires no crate beyond what `tokio::process::Command` already provides. The lints in `Cargo.toml` (`unsafe_code = forbid`, `clippy::cargo` warn for dep bloat) reward this minimal-deps stance.
- **ED-15: Signal handling: `tokio::select!` between the exec future and `tokio::signal::ctrl_c()` in the `--command` arm.** On Ctrl-C, the exec future is dropped — `kill_on_drop(true)` on the ssh child fires SIGKILL — `post_boot` returns `Ok(130)` — `run_room`'s `vm.shutdown()` runs cleanly — rooms exits 130. Without the `select!`, the default SIGINT handler terminates rooms before `vm.shutdown()` can fire, leaking the firecracker process AND the per-room state dir. The `--keep` arm already has this pattern (its `tokio::signal::ctrl_c().await` is *the* exit trigger for that mode); the `--command` arm adopts it for the same cleanup reason.
- **ED-16: Hoist `key_path()` once into `run_room`; pass `&Path` down to `post_boot` and `runner::*`.** Avoids reading `HOME` twice and avoids the duplicate-allocation clippy noise.

## Validation

The agent implementing this MUST run all of these on the rooms-host VM and confirm they pass before opening the PR. **Paste the curl JSON response (step 5) into the PR description.**

Note: `make check` on the Windows side covers fmt + clippy + unit tests; the e2e validation steps below require the rooms-host VM (Linux, KVM, baked rootfs).

1. **Lint + unit tests.** From the worktree root: `source ~/.cargo/env && make check`. Must exit 0 with no warnings. (Note: `make lint` runs `cargo clippy --all-targets --all-features -- -D warnings`, so the `e2e` feature module *compiles* under clippy — that's intentional, it keeps the e2e module lint-clean even on Windows. `make test` runs `cargo test` *without* `--all-features`, so the e2e test bodies don't *execute* — they only run on the rooms-host via the explicit `cargo test --features e2e` invocation we never put in `make`.)
2. **Re-bake the rootfs** (one-time per host after pulling m4):
   ```sh
   bash scripts/bake-rootfs-ssh.sh
   ```
   Expected: §7's three existing `set_directive` calls log "already = yes/no" (idempotent), AcceptEnv logs "missing or commented; appending" on first run (and "already = ANTHROPIC_API_KEY" on subsequent runs). Exit 0.
   Verify (mount the rootfs separately into a fresh tempdir):
   ```sh
   TMP=$(mktemp -d); LP=$(sudo losetup -f --show ~/rooms/images/rootfs.ext4); sudo mount "$LP" "$TMP"
   sudo grep -cE "^AcceptEnv ANTHROPIC_API_KEY\$" "$TMP/etc/ssh/sshd_config"   # must be 1
   sudo umount "$TMP"; sudo losetup -d "$LP"; rmdir "$TMP"
   ```
3. **Smoke: trivial command.**
   ```sh
   cargo run --quiet -- run --image ~/rooms/images/rootfs.ext4 --command 'uname -a'
   ```
   Expected: `Linux ubuntu-fc-uvm 4.14.174 ...` on stdout; rooms log lines on stderr; exit 0. **Cleanup check:** `ls ~/.local/state/rooms/` must be empty; `pgrep firecracker` must return nothing.
4. **Env passthrough proof.**
   ```sh
   export ANTHROPIC_API_KEY=test-value-not-a-real-key
   cargo run --quiet -- run --image ~/rooms/images/rootfs.ext4 \
       --command 'echo "guest sees: ${ANTHROPIC_API_KEY:-MISSING}"'
   ```
   Expected stdout: `guest sees: test-value-not-a-real-key`. If the output is `guest sees: MISSING`, AcceptEnv isn't taking effect — re-check step 2's grep and re-bake. Reset the var to the real key before step 5.
5. **The upper bar: real Anthropic call.** With the real `ANTHROPIC_API_KEY` exported:
   ```sh
   [[ -n "${ANTHROPIC_API_KEY:-}" ]] || { echo "ANTHROPIC_API_KEY unset — export it (and verify ~/.bashrc didn't fail to source in this non-interactive shell)"; exit 1; }
   cargo run --quiet -- run --image ~/rooms/images/rootfs.ext4 \
       --command 'curl -s -H "x-api-key: $ANTHROPIC_API_KEY" \
                       -H "anthropic-version: 2023-06-01" \
                       -H "content-type: application/json" \
                       -d "{\"model\":\"claude-3-5-sonnet-latest\",\"max_tokens\":256,\"messages\":[{\"role\":\"user\",\"content\":\"say hello in one short sentence\"}]}" \
                       https://api.anthropic.com/v1/messages' | tee /tmp/m4-response.json
   ```
   Expected stdout: a JSON object with `content[0].text` containing a real Claude greeting. Exit code 0. **Capture the full response and paste it (or its first 30 lines, redacting message-id) into the PR description.**
   Optional sanity: `jq -r '.content[0].text' /tmp/m4-response.json` prints the greeting alone.
   **If the response is `{"error":{"type":"authentication_error",...}}`** — the env var didn't reach the guest. Most likely the rootfs wasn't re-baked (step 2's grep should show `^AcceptEnv ANTHROPIC_API_KEY$` with count 1), or `ANTHROPIC_API_KEY` actually isn't set in the operator's current shell (re-run step 4's env-passthrough proof to confirm).
6. **Exit-code propagation: non-zero.**
   ```sh
   cargo run --quiet -- run --image ~/rooms/images/rootfs.ext4 --command 'exit 17'; echo "rooms exit: $?"
   ```
   Expected: `rooms exit: 17`. Proves the substrate threads guest exit codes through to the operator's shell.
7. **Exit-code propagation: command-not-found.**
   ```sh
   cargo run --quiet -- run --image ~/rooms/images/rootfs.ext4 --command 'nonexistent-binary-xyzzy'; echo "rooms exit: $?"
   ```
   Expected: `rooms exit: 127` (bash's "command not found"). Stderr includes the guest's `bash: nonexistent-binary-xyzzy: command not found` (via SSH stderr forward).
8. **Mutual exclusion.**
   ```sh
   cargo run --quiet -- run --image ~/rooms/images/rootfs.ext4 --keep --command 'echo hi'
   ```
   Expected: clap rejects (exact wording may vary by clap version, but stderr names *both* `--keep` and `--command`), exit code 2. Note: clap's `Cli::parse()` calls `process::exit(2)` directly on this — `dispatch` never runs, no `info!("rooms run")` log fires. Empty-string rejection check:
   ```sh
   cargo run --quiet -- run --image ~/rooms/images/rootfs.ext4 --command ''
   ```
   Expected: clap rejects with the message from ED-11's `non_empty_command` parser, exit code 2.
9. **Existing behavior unchanged.**
   - `cargo run --quiet -- run --image ~/rooms/images/rootfs.ext4` — boots, sleeps 3s, shuts down, exits 0 (existing default).
   - `cargo run --quiet -- run --image ~/rooms/images/rootfs.ext4 --keep` — boots, waits for Ctrl-C, shuts down on Ctrl-C, exits 0 (existing keep mode). Verify Ctrl-C is responsive.
10. **Cleanup verification (after the full validation pass).** Across the steps that actually boot a VM (3, 4, 5, 6, 7, 9; step 8 short-circuits in clap before boot), the following invariants MUST hold at the end:
    - `ls ~/.local/state/rooms/` is empty (no leaked per-room dirs).
    - `pgrep firecracker` returns nothing (no leaked processes).
    - `ip link show tap-fc0` shows the TAP still up (we don't tear it down per-room in POC).
    Bonus check: between steps 4 and 5, hit Ctrl-C immediately during the curl invocation (the request takes ~2s to round-trip). After the Ctrl-C exits rooms, `ls ~/.local/state/rooms/` MUST STILL be empty — that's the property ED-15's `tokio::select!` exists to guarantee. (If this leaks, the `select!` is wired wrong.)

If any step fails, do NOT open the PR; fix and re-validate from step 1.

## Risks

- **SSH exit code 255 ambiguity.** SSH returns 255 for ssh-internal errors (key rejected, host unreachable, sshd never came up) AND for "remote command exited 255". POC accepts the collision — the bail messages in `wait_for_ssh` distinguish at the connect-fail layer, and the exec layer trusts the remote exit code. Productionization (`runner-contract`) wraps the remote command with `; printf "ROOMS-EXIT:%s\n" "$?"` and parses the trailing line to disambiguate, but that's a bigger change.
- **`openssh-client` missing on the rooms-host.** Mitigated: the bake script's prereq loop (ED-13) now checks for `ssh` and fails at bake time with the apt-install hint, instead of m4 failing at exec time with a cryptic anyhow context. Ubuntu Server 24.04 ships with `openssh-client` by default; this catches the stripped-down case.
- **Ctrl-C during `--command` leaks the room.** Mitigated by ED-15's `tokio::select!` between the exec future and `tokio::signal::ctrl_c()`: on Ctrl-C, the exec future drops → `kill_on_drop(true)` SIGKILLs the ssh child → `post_boot` returns `Ok(130)` → `run_room`'s `vm.shutdown()` runs cleanly. Validation §10 has the explicit "hit Ctrl-C during curl, verify state dir is empty" check.
- **sshd takes longer than 60s to come up on a stressed host.** Probe times out, rooms bails with "sshd at 172.16.0.2 did not accept connections within 60s". Operator can re-run; if it persists, that's a real environment issue worth surfacing. Mitigation in productionization (`harden-firecracker-control`): boot-output parsing as an early signal.
- **Stale `~/.local/state/rooms/<ulid>/` dirs from a crashed previous run.** Doesn't affect m4 directly — `firecracker::boot` creates a *new* ulid each invocation, so existing dirs are inert. They DO leak disk space over time. Operator can `rm -rf ~/.local/state/rooms/` between runs; productionization (`harden-firecracker-control`) adds startup-time stale-dir reaping.
- **Operator pulls m4 but forgets to re-bake the rootfs.** Step 4 (env passthrough proof) catches this: guest sees `MISSING`, and the real-API call (step 5) fails with `{"error":{"type":"authentication_error",...}}`. Step 5's failure-mode footnote names this explicitly so the operator doesn't waste time on the rust side. The bake script's idempotent re-run is fast; productionization (`rootfs-builder`) bakes AcceptEnv by default.
- **`ANTHROPIC_API_KEY` unset on host.** `SendEnv` of an unset var is silently dropped — the guest sees no env var; the curl in step 5 fails with auth error from Anthropic. Acceptable for POC; could add a "warn if `--command` references `$ANTHROPIC_API_KEY` but it's not set" guardrail, but that's argument-parsing-the-command territory and adds fragility.
- **`SendEnv=ANTHROPIC_API_KEY` exposes the var in the SSH protocol envelope.** The ssh client sends the var name + value in cleartext over the encrypted SSH tunnel — fine for transport, but the var is in memory in both ssh client and sshd. POC-acceptable (we boot a fresh microVM per command; the value lives only as long as the room). Hardening (encrypted-at-rest secrets, vsock-injected) is a productionization concern (`secret-injection-via-vsock`).
- **Tracing now on stderr surprises downstream tooling.** Anyone who captured rooms stderr expecting it to be quiet now sees log lines. Acceptable — there's no downstream tooling yet, and the new behavior is the right one (host stdout = guest stdout). README mentions it.
- **`exec_in_guest`'s `--` separator misinterpreted by `Command::args`.** `tokio::process::Command::args(["...", "--", command])` passes `--` as a literal argument to the ssh binary; ssh's argv parsing then treats it as "end of ssh options, the rest is the remote command". This is the documented OpenSSH behavior; cursor MUST NOT remove the `--` thinking it's redundant.
- **`set_directive AcceptEnv "ANTHROPIC_API_KEY"` regex interaction.** The existing `set_directive` regex `^${dir}[[:space:]]+${val}\$` matches `AcceptEnv ANTHROPIC_API_KEY` exactly. Future additions like `AcceptEnv FOO BAR` would NOT match (the regex sees `FOO` not `FOO BAR`) and would always trigger the "missing → append" branch, accumulating duplicate lines. m4 has one var; flagged for productionization.

## Out-of-scope (deferred to future tasks)

- **`--prompt <STRING>` convenience verb.** Substrate doesn't know about Anthropic. A `--prompt` shape lives in a *consumer* (an Anthropic-aware wrapper script, or a future `rooms-anthropic` binary). Not in m4, not in rooms substrate.
- **Generic `--env KEY=VAL` plumbing.** One var (`ANTHROPIC_API_KEY`) for POC. Generic env plumbing comes with `cursor-sdk-runner` or a new `runner-contract` task.
- **Repo transport into guest.** No `/workspace/repo` for m4; the curl invocation is self-contained. Repo transport lands in the cursor-sdk-runner milestone.
- **Patch / artifact extraction.** No `result.patch` for m4; the curl response IS the artifact, and it flows back via stdout. Artifact capture is `runner-contract`.
- **Per-room SSH keys.** One shared key for all rooms — m3's pattern, preserved in m4. Per-room dynamic keys are `harden-firecracker-control`.
- **Native russh / openssh-rs client.** Shell out for POC, swap if profiling shows the spawn overhead matters (it won't for one command per VM).
- **Vsock command channel.** SSH is the POC. Vsock is `harden-firecracker-control` or later.
- **Multiple parallel commands per boot.** One `--command` per room; for multi-command, boot multiple rooms. Multi-exec-per-room is a `runner-contract` consideration.
- **Structured error types.** `anyhow::bail!` is fine per CLAUDE.md POC scope. `FirecrackerError` enum is `harden-firecracker-control`.
- **`rooms exec <room_id> -- <command>` standalone verb.** The v0 spec lists this as a primitive; m4 uses `rooms run --command` (compose verb) since the create / exec / collect / destroy primitive split needs persistent room IDs across CLI invocations (not yet built). The standalone `exec` verb lands with the cursor-sdk-runner work where persistent rooms become useful.

## Implementation plan

The agent implementing this MUST follow these steps in order. Each step is independently reviewable.

1. **CLI flag.** Add `command: Option<String>` to the `Run` variant in `src/main.rs` with `conflicts_with = "keep"` on it (clap reciprocates automatically; the spec puts it on `keep` too for symmetric `--help` text) plus the `value_parser = non_empty_command` per ED-11. **Delete** the now-false m3/m4 phase-ref doc comment on the `Run` variant (and the "intentionally absent in this PR" comment block right below it) per the no-design-doc-refs convention in CLAUDE.md — replace with a one-liner like `/// Boot a microVM and optionally run a single command in it via SSH.`. Confirm `cargo run -- run --help` shows the new flag and the conflict notes.
2. **Tracing to stderr.** One-line change in `main()`: `.with_writer(std::io::stderr)` on the `tracing_subscriber::fmt()` builder. Confirm `cargo run -- run --image /nonexistent` prints the error on stderr (not stdout) and exits 2.
3. **New module `src/runner.rs`.** Implement `wait_for_ssh` and `exec_in_guest` per the Functional section above. Add `pub mod runner;` to `src/lib.rs`. Confirm `cargo build` compiles clean.
4. **`dispatch` / `run_room` / `post_boot` refactor.** Change `dispatch` return type to `Result<u8>`; extract `run_room` (owns boot + always-shutdown) and `post_boot` (the inner `(keep, command)` match with `tokio::select!` in the `Some(cmd)` arm per ED-15); add the `key_path() -> Result<PathBuf>` helper per ED-12 (bails on missing HOME, no `/root` fallback); hoist `key_path()` into `run_room` once and pass `&Path` down. Confirm clippy doesn't complain about complexity (cognitive cap 20) on either helper.
5. **Runner unit tests** in `src/runner.rs` mod tests. The test module MUST open with the same `#![allow(...)]` panicky-lint header `src/firecracker.rs`'s tests use:
   ```rust
   #[cfg(test)]
   mod tests {
       #![allow(
           clippy::unwrap_used,
           clippy::expect_used,
           clippy::panic,
           reason = "test module: panicky lints are noise in tests"
       )]
       // ...
   }
   ```
   The single required test:
    - `wait_for_ssh_times_out_when_no_sshd`: point at a guaranteed-unreachable address (`127.0.0.255` or an unreachable port like `127.0.0.1:1` with a short timeout) and assert the function bails within roughly the timeout window with an error message containing `"did not accept connections"`. Mirrors the existing `wait_for_socket_requires_listener_not_just_file` test in firecracker.rs in shape and intent. Use a 2-second timeout so the test stays fast.
   `exec_in_guest` is not unit-tested in m4 — a meaningful test would need either a real sshd or a fake-ssh shim, and the latter is Windows-unfriendly (no `#!/bin/sh` semantics) per the cross-platform `make check` story. The Validation §§3–7 e2e steps cover `exec_in_guest`'s behavior end-to-end on the rooms-host.

   Also extend the existing `cli_definition_is_valid` test in `main.rs` (it's a one-liner today) with a second test that asserts mutual exclusion errors at clap parse time:
   ```rust
   #[test]
   fn keep_and_command_are_mutually_exclusive() {
       let err = Cli::try_parse_from([
           "rooms", "run", "--image", "x", "--keep", "--command", "echo hi",
       ])
       .expect_err("--keep + --command should fail to parse");
       assert!(
           err.to_string().contains("--keep") && err.to_string().contains("--command"),
           "expected error to name both flags; got: {err}"
       );
   }
   ```
   This catches mid-implementation `conflicts_with` regressions without depending on clap's exact error wording (just the flag names).
6. **Bake script.** Three edits to `scripts/bake-rootfs-ssh.sh`: (a) add `ssh` to the §1 prereqs loop; (b) append `set_directive AcceptEnv ANTHROPIC_API_KEY` after the existing three `set_directive` calls in §7; (c) append the env-passthrough log line in §9. `shellcheck scripts/bake-rootfs-ssh.sh` exits clean.
7. **README.** One-paragraph addition (text above).
8. **Local checks.** `make check` exits 0 with no warnings. (Windows-side runs fmt + clippy + unit tests; no e2e.)
9. **Push to rooms-host VM, run e2e validation** (Validation §§ 1–10). Capture step 5's JSON response.
10. **Commit + PR.** Branch: `m4-anthropic-curl`. Reviewers: Copilot (`gh pr edit N --add-reviewer @copilot`), `@codex review` (comment), `@claude review` (comment). PR body includes the captured JSON response from validation step 5 as proof of the upper bar.

PR shape: one PR, ~213 weighted LOC. "amazing" band.

**Branch:** `m4-anthropic-curl`.

**Spec path for `ship.ship`:** `docs/features/poc-m4-anthropic-curl/spec.md`, relative to the worktree root.
