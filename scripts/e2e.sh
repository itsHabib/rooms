#!/usr/bin/env bash
# Host-e2e harness — one command to preflight, build, run the e2e suite, and
# assert zero host leaks on the rooms-host. Idempotent and safe to re-run.
#
# Tap creation needs root, but compiling as root would litter target/ and the
# cargo cache with root-owned files (breaking the next non-root build). So this
# builds AS the invoking user and runs only the root-needing e2e test binary as
# root. Order: build (user) -> preflight on `rooms doctor` (a FAIL aborts before
# any boot) -> run the e2e binary (root) -> assert no host-global leak (leftover
# tap-fc* / firecracker procs / a non-clean `rooms ls`) -> one-line PASS/FAIL.
#
# Usage (rooms-host):
#   sudo -E make e2e            # or: sudo -E scripts/e2e.sh [--image <rootfs>]
#
# Preconditions (the preflight names any that are missing): /dev/kvm,
# firecracker + jailer, the ROOMS_FWD chain (`sudo bash scripts/setup-tap.sh
# --host`), and the guest images.

# No `-e`: errors are handled explicitly so the leak assertions always run.
set -uo pipefail

if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
    echo "[e2e] must run as root (tap creation) — try: sudo -E make e2e" >&2
    exit 2
fi

# Build as the user who invoked sudo, not root, so the cargo cache + target/
# stay theirs. Falls back to root only when not under sudo.
build_user="${SUDO_USER:-root}"
user_home="$(getent passwd "$build_user" 2>/dev/null | cut -d: -f6)"
user_home="${user_home:-${HOME}}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${user_home}/rooms/images/rootfs.ext4"
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
echo "[e2e] repo=$repo_root image=$image user=$build_user logs=$log_dir"

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

# 1. Build the rooms binary + the e2e test binary AS THE USER (no-run: compile
#    only). The e2e tests themselves need root, so we run the built binary below.
echo "[e2e] building (as $build_user)..."
if ! sudo -u "$build_user" env HOME="$user_home" \
    cargo test --features e2e --no-run >"$log_dir/build.log" 2>&1; then
    tail -n 30 "$log_dir/build.log" >&2
    fail "build failed (see $log_dir/build.log)"
fi
rooms_bin="$repo_root/target/debug/rooms"
test_bin="$(grep -oE 'target/debug/deps/pool_e2e-[a-zA-Z0-9]+' "$log_dir/build.log" | head -1)"
[[ -n "$test_bin" && -x "$repo_root/$test_bin" ]] ||
    fail "could not locate the pool_e2e test binary in $log_dir/build.log"

# 2. Preflight (root — iptables / kvm reads). `rooms doctor` exits non-zero on
#    any FAIL; abort with the remediation before booting.
echo "[e2e] preflight: rooms doctor..."
if ! "$rooms_bin" doctor --image "$image" 2>"$log_dir/doctor.log"; then
    cat "$log_dir/doctor.log" >&2
    fail "doctor preflight reported a FAIL — fix the checks above before running"
fi

# Baseline host-global state the run must restore exactly.
taps_before="$(ip -o link show 2>/dev/null | count 'tap-fc')"
fc_before="$(pgrep -c firecracker 2>/dev/null || true)"
fc_before="${fc_before:-0}"

# 3. Run the e2e binary as root (serial — the tests share host-global taps/slots)
#    with the user's HOME so it finds the images + guest key. Each test still
#    self-isolates its own scratch state base, so a crash never poisons the next.
echo "[e2e] running e2e ($test_bin)..."
e2e_rc=0
HOME="$user_home" "$repo_root/$test_bin" --test-threads=1 --nocapture \
    >"$log_dir/e2e.log" 2>&1 || e2e_rc=$?
tail -n 15 "$log_dir/e2e.log"

# 4. Assert zero host-global leak: no new tap-fc*, no orphaned firecracker, and a
#    clean `rooms ls`. `"id":` appears once per listed room in the --json report.
taps_after="$(ip -o link show 2>/dev/null | count 'tap-fc')"
fc_after="$(pgrep -c firecracker 2>/dev/null || true)"
fc_after="${fc_after:-0}"
ls_rooms="$(HOME="$user_home" "$rooms_bin" ls --json 2>/dev/null | count '"id":')"

leaks=()
[[ "$taps_after" -gt "$taps_before" ]] &&
    leaks+=("tap-fc leaked (before=$taps_before after=$taps_after): $(ip -o link show | grep 'tap-fc' | tr '\n' ' ')")
[[ "$fc_after" -gt "$fc_before" ]] &&
    leaks+=("firecracker procs leaked (before=$fc_before after=$fc_after)")
[[ "$ls_rooms" -gt 0 ]] &&
    leaks+=("rooms ls not clean: $ls_rooms room(s) left — $(HOME="$user_home" "$rooms_bin" ls 2>&1 | tr '\n' ';')")

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
