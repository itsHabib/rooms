#!/usr/bin/env bash
# Host-e2e harness — one command to preflight, build, run the e2e suite, and
# assert zero host leaks on the rooms-host. Idempotent and safe to re-run.
#
# Order: build the `rooms` binary → preflight on `rooms doctor` (a FAIL aborts
# with the remediation, before any boot) → `cargo test --features e2e` → assert
# no host-global leak (leftover tap-fc* / firecracker procs / a non-clean
# `rooms ls`). Prints a one-line PASS/FAIL and the log dir.
#
# Usage (rooms-host, root for tap creation):
#   sudo -E scripts/e2e.sh [--image <rootfs.ext4>]
#
# Preconditions: /dev/kvm, firecracker + jailer, the ROOMS_FWD chain
# (`sudo bash scripts/setup-tap.sh --host`), and the guest images. The preflight
# names any that are missing rather than failing deep in a boot.

# No `-e`: errors are handled explicitly so the leak assertions always run.
set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${HOME}/rooms/images/rootfs.ext4"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --image)
            image="$2"
            shift 2
            ;;
        *)
            echo "unknown arg: $1" >&2
            exit 2
            ;;
    esac
done

log_dir="$(mktemp -d "${TMPDIR:-/tmp}/rooms-e2e.XXXXXX")"
echo "[e2e] repo=$repo_root image=$image logs=$log_dir"

fail() {
    echo "[e2e] FAIL: $*" >&2
    echo "[e2e] logs: $log_dir" >&2
    exit 1
}

# Count matching lines, tolerating grep's exit-1-on-no-match under pipefail.
count() {
    local n
    n="$(grep -c "$1" 2>/dev/null || true)"
    echo "${n:-0}"
}

cd "$repo_root" || fail "repo root not found: $repo_root"

# 1. Build the binary the preflight doctor runs. Debug, not release: the e2e
#    tests use the debug `rooms` (via CARGO_BIN_EXE_rooms), so a release build
#    would be both unused and a slow cold compile — the debug build is shared
#    with the `cargo test` step below.
echo "[e2e] building..."
if ! cargo build >"$log_dir/build.log" 2>&1; then
    tail -n 30 "$log_dir/build.log" >&2
    fail "build failed (see $log_dir/build.log)"
fi
rooms_bin="$repo_root/target/debug/rooms"

# 2. Preflight — `rooms doctor` exits non-zero on any FAIL. Abort before booting.
echo "[e2e] preflight: rooms doctor..."
if ! "$rooms_bin" doctor --image "$image" 2>"$log_dir/doctor.log"; then
    cat "$log_dir/doctor.log" >&2
    fail "doctor preflight reported a FAIL — fix the checks above before running"
fi

# Baseline host-global state the run must restore exactly.
taps_before="$(ip -o link show 2>/dev/null | count 'tap-fc')"
fc_before="$(pgrep -c firecracker 2>/dev/null || true)"
fc_before="${fc_before:-0}"

# 3. Run the e2e suite (each test self-isolates its own scratch state base, so a
#    crashed run never poisons the next).
echo "[e2e] running cargo test --features e2e..."
e2e_rc=0
cargo test --features e2e >"$log_dir/e2e.log" 2>&1 || e2e_rc=$?
tail -n 15 "$log_dir/e2e.log"

# 4. Assert zero host-global leak: no new tap-fc*, no orphaned firecracker, and a
#    clean `rooms ls`. `"id":` appears once per listed room in the --json report.
taps_after="$(ip -o link show 2>/dev/null | count 'tap-fc')"
fc_after="$(pgrep -c firecracker 2>/dev/null || true)"
fc_after="${fc_after:-0}"
ls_rooms="$("$rooms_bin" ls --json 2>/dev/null | count '"id":')"

leaks=()
[[ "$taps_after" -gt "$taps_before" ]] &&
    leaks+=("tap-fc leaked (before=$taps_before after=$taps_after): $(ip -o link show | grep 'tap-fc' | tr '\n' ' ')")
[[ "$fc_after" -gt "$fc_before" ]] &&
    leaks+=("firecracker procs leaked (before=$fc_before after=$fc_after)")
[[ "$ls_rooms" -gt 0 ]] &&
    leaks+=("rooms ls not clean: $ls_rooms room(s) left — $("$rooms_bin" ls 2>&1 | tr '\n' ';')")

if [[ "$e2e_rc" -ne 0 ]]; then
    echo "[e2e] e2e suite FAILED (rc=$e2e_rc); see $log_dir/e2e.log" >&2
fi
if [[ "${#leaks[@]}" -gt 0 ]]; then
    printf '[e2e] LEAK: %s\n' "${leaks[@]}" >&2
fi

if [[ "$e2e_rc" -eq 0 && "${#leaks[@]}" -eq 0 ]]; then
    echo "[e2e] PASS — e2e green, zero host leaks. logs: $log_dir"
    exit 0
fi
fail "e2e rc=$e2e_rc, leaks=${#leaks[@]}"
