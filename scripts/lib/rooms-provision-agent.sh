#!/bin/sh
# Credential-free neutral-base provisioning agent.
set -eu

PROVISION_DIR=/run/rooms-provision
BUNDLE="$PROVISION_DIR/repo.bundle"
WARM="$PROVISION_DIR/warm"
PROCESS_BASELINE="$PROVISION_DIR/processes.before-warm"
PROTECTED_BEFORE="$PROVISION_DIR/protected.before-warm"
PROTECTED_AFTER="$PROVISION_DIR/protected.after-warm"
ROOMS_HOME=/home/rooms
ROOMS_REPO=/workspace/repo
SYSTEM_GIT_CONFIG=/etc/gitconfig
SUDOERS_DIR=/etc/sudoers.d
SUDOERS_FILE="$SUDOERS_DIR/rooms"
SUDOERS_GRANT='rooms ALL=(ALL) NOPASSWD: ALL'

fail() {
    printf 'ERR %s\n' "$*" >&2
    exit 1
}

# The provisioning directory is a root-owned control boundary, but the rooms
# user must be able to traverse it to read the explicitly named bundle and warm
# script. 0711 grants traversal without granting listing or mutation rights.
provision_dir_is_safe() {
    directory="$1"
    expected_uid="$2"
    expected_gid="$3"
    [ ! -L "$directory" ] && [ -d "$directory" ] || return 1
    owner="$(stat -c '%u:%g' "$directory")" || return 1
    mode="$(stat -c '%a' "$directory")" || return 1
    [ "$owner" = "$expected_uid:$expected_gid" ] && [ "$mode" = 711 ]
}

prepare_provision_dir() {
    [ ! -L "$PROVISION_DIR" ] || fail "provision directory is a symlink"
    [ ! -e "$PROVISION_DIR" ] || [ -d "$PROVISION_DIR" ] \
        || fail "provision path is not a directory"
    install -d -m 0711 -o root -g root "$PROVISION_DIR" \
        || fail "cannot prepare provision directory"
    provision_dir_is_safe "$PROVISION_DIR" 0 0 \
        || fail "provision directory is not root-owned mode 0711"
}

sudoers_grant_is_exact() {
    grant="$1"
    expected_uid="$2"
    expected_gid="$3"
    [ ! -L "$grant" ] && [ -f "$grant" ] || return 1
    owner="$(stat -c '%u:%g' "$grant")" || return 1
    mode="$(stat -c '%a' "$grant")" || return 1
    [ "$owner" = "$expected_uid:$expected_gid" ] && [ "$mode" = 440 ] || return 1
    [ "$(wc -l <"$grant")" -eq 1 ] || return 1
    [ "$(cat "$grant")" = "$SUDOERS_GRANT" ]
}

sudoers_directory_is_safe() {
    directory="$1"
    expected_uid="$2"
    expected_gid="$3"
    [ ! -L "$directory" ] && [ -d "$directory" ] || return 1
    owner="$(stat -c '%u:%g' "$directory")" || return 1
    mode="$(stat -c '%a' "$directory")" || return 1
    [ "$owner" = "$expected_uid:$expected_gid" ] && [ "${#mode}" -eq 3 ] || return 1
    case "$mode" in
        ?[2367]?|??[2367]) return 1 ;;
        *) return 0 ;;
    esac
}

rooms_sudo_is_revoked() {
    ! su rooms -s /bin/sh -c \
        'exec env -i HOME=/home/rooms USER=rooms LOGNAME=rooms PATH=/usr/local/bin:/usr/bin:/bin sudo -n true' \
        >/dev/null 2>&1
}

# A normal workload gets passwordless sudo, but a neutral-base warm command is
# deliberately less privileged: it may populate user-owned caches, never alter
# the root policy that will execute after the snapshot is forked. The unlink is
# atomic, and the failed sudo probe proves no alternate policy grants root.
revoke_workload_sudo() {
    sudoers_directory_is_safe "$SUDOERS_DIR" 0 0 \
        || fail "sudoers directory is not root-owned and non-writable by group/other"
    sudoers_grant_is_exact "$SUDOERS_FILE" 0 0 \
        || fail "rooms sudoers grant is not the exact root-owned mode 0440 policy"
    rm -f "$SUDOERS_FILE" || fail "cannot revoke rooms workload sudo"
    [ ! -e "$SUDOERS_FILE" ] && [ ! -L "$SUDOERS_FILE" ] \
        || fail "rooms workload sudo grant survived revocation"
    rooms_sudo_is_revoked || fail "rooms retains sudo authority during neutral warm"
}

protected_directory_record() {
    directory="$1"
    [ ! -L "$directory" ] && [ -d "$directory" ] || return 1
    owner="$(stat -c '%u:%g' "$directory")" || return 1
    mode="$(stat -c '%a' "$directory")" || return 1
    printf 'directory|%s|%s|%s\n' "$directory" "$owner" "$mode"
}

protected_file_record() {
    file="$1"
    [ ! -L "$file" ] && [ -f "$file" ] || return 1
    owner="$(stat -c '%u:%g' "$file")" || return 1
    mode="$(stat -c '%a' "$file")" || return 1
    digest="$(sha256sum "$file" | awk 'NR == 1 { print $1 }')" || return 1
    [ -n "$digest" ] || return 1
    printf 'file|%s|%s|%s|%s\n' "$file" "$owner" "$mode" "$digest"
}

protected_optional_file_record() {
    file="$1"
    if [ ! -e "$file" ] && [ ! -L "$file" ]; then
        printf 'missing|%s\n' "$file"
        return 0
    fi
    protected_file_record "$file"
}

write_protected_state() {
    destination="$1"
    umask 077
    {
        for directory in \
            /sbin /etc /etc/ssh "$SUDOERS_DIR" \
            /home "$ROOMS_HOME" "$ROOMS_HOME/.ssh"; do
            protected_directory_record "$directory" || return 1
        done
        for file in \
            /sbin/rooms-resume-agent /etc/ssh/sshd_config \
            /etc/sudoers "$ROOMS_HOME/.ssh/authorized_keys"; do
            protected_file_record "$file" || return 1
        done
        protected_optional_file_record "$SYSTEM_GIT_CONFIG" || return 1
    } >"$destination" || return 1
    chmod 0600 "$destination"
}

capture_protected_state() {
    write_protected_state "$PROTECTED_BEFORE" \
        || fail "cannot capture protected pre-warm policy state"
}

verify_protected_state() {
    write_protected_state "$PROTECTED_AFTER" \
        || fail "protected path changed shape during neutral warm"
    cmp -s "$PROTECTED_BEFORE" "$PROTECTED_AFTER" \
        || fail "protected policy or authorized key changed during neutral warm"
    rm -f "$PROTECTED_AFTER"
}

read_frame() {
    expected="$1"
    destination="$2"
    IFS=' ' read -r kind length || fail "missing $expected frame"
    [ "$kind" = "$expected" ] || fail "wanted $expected frame, got $kind"
    case "$length" in
        ''|*[!0-9]*) fail "invalid $expected length" ;;
    esac
    : >"$destination"
    if [ "$length" -ne 0 ]; then
        # dd bs=1 count=N reads EXACTLY N bytes. BusyBox `head -c N` can issue a
        # larger read against a pipe/socket, consume the following frame header,
        # and discard those extra bytes when it exits.
        dd bs=1 count="$length" >"$destination" 2>/dev/null
    fi
    actual="$(wc -c <"$destination")"
    [ "$actual" -eq "$length" ] || fail "short $expected frame: wanted $length bytes, got $actual"
}

credential_file_is_absent() {
    path="$1"
    if [ -e "$path" ] || [ -L "$path" ]; then
        printf 'credential path present: %s\n' "$path" >&2
        return 1
    fi
}

optional_directory_is_real() {
    directory="$1"
    [ ! -e "$directory" ] && [ ! -L "$directory" ] && return 0
    [ ! -L "$directory" ] && [ -d "$directory" ]
}

url_config_records_are_safe() {
    awk '
        {
            line = tolower($0)
            if (line ~ /[a-z][a-z0-9+.-]*:\/\/[^\/[:space:]]*@/ ||
                line ~ /[?&](access[_-]?token|token|password|oauth|api[_-]?key)=/ ||
                line ~ /authorization[:=]/ ||
                line ~ /bearer([+%][0-9a-f]+|[[:space:]])/) {
                unsafe = 1
            }
        }
        END { exit unsafe ? 1 : 0 }
    '
}

# Inspect one raw config file without following includes. Includes themselves
# are forbidden, so parsing them would only widen the root agent's read surface.
# Unrelated warm-cache settings and credential-free URLs remain valid.
git_config_file_is_safe() {
    config="$1"
    [ ! -e "$config" ] && [ ! -L "$config" ] && return 0
    [ ! -L "$config" ] && [ -f "$config" ] || return 1

    risky='^(credential(\..*)?|http(\..*)?\.(extraheader|cookiefile)|include(\..*)?|includeif\..*|core\.(askpass|sshcommand))$'
    if keys="$(git config --file "$config" --no-includes --name-only --get-regexp "$risky" 2>/dev/null)"; then
        key="$(printf '%s\n' "$keys" | awk 'NR == 1 { print; exit }')"
        printf 'risky git config directive in %s: %s\n' "$config" "$key" >&2
        return 1
    else
        status=$?
        [ "$status" -eq 1 ] || return 1
    fi

    urls='^(url\..*\.(insteadof|pushinsteadof)|remote\..*\.(url|pushurl)|submodule\..*\.url|http(\..*)?\.proxy)$'
    if records="$(git config --file "$config" --no-includes --get-regexp "$urls" 2>/dev/null)"; then
        printf '%s\n' "$records" | url_config_records_are_safe || {
            printf 'authenticated URL or rewrite in git config: %s\n' "$config" >&2
            return 1
        }
        return 0
    else
        status=$?
        [ "$status" -eq 1 ]
    fi
}

credential_state_is_safe() {
    optional_directory_is_real "$ROOMS_HOME/.config" || return 1
    optional_directory_is_real "$ROOMS_HOME/.config/git" || return 1
    optional_directory_is_real "$ROOMS_HOME/.config/gh" || return 1
    for path in \
        "$ROOMS_HOME/.git-credentials" \
        "$ROOMS_HOME/.netrc" \
        "$ROOMS_HOME/.config/git/credentials" \
        "$ROOMS_HOME/.config/gh/hosts.yml"; do
        credential_file_is_absent "$path" || return 1
    done

    if [ -e "$ROOMS_HOME/.ssh" ]; then
        extra_ssh="$(find "$ROOMS_HOME/.ssh" -mindepth 1 -maxdepth 1 \
            ! -name authorized_keys -print -quit)" || return 1
        [ -z "$extra_ssh" ] || {
            printf 'unexpected SSH credential path present: %s\n' "$extra_ssh" >&2
            return 1
        }
    fi

    git_config_file_is_safe "$SYSTEM_GIT_CONFIG" || return 1
    git_config_file_is_safe "$ROOMS_HOME/.gitconfig" || return 1
    git_config_file_is_safe "$ROOMS_HOME/.config/git/config" || return 1

    [ ! -L "$ROOMS_REPO" ] || return 1
    [ ! -e "$ROOMS_REPO" ] && return 0
    [ -d "$ROOMS_REPO" ] || return 1
    [ ! -L "$ROOMS_REPO/.git" ] && [ -d "$ROOMS_REPO/.git" ] || return 1
    git_config_file_is_safe "$ROOMS_REPO/.git/config" || return 1
    git_config_file_is_safe "$ROOMS_REPO/.git/config.worktree"
}

clone_bundle() {
    [ -s "$BUNDLE" ] || return 0
    rm -rf /workspace/repo
    chown rooms:rooms "$BUNDLE"
    su rooms -s /bin/sh -c \
        "env -i HOME=/home/rooms USER=rooms LOGNAME=rooms PATH=/usr/local/bin:/usr/bin:/bin \
         git clone '$BUNDLE' /workspace/repo && \
         env -i HOME=/home/rooms USER=rooms LOGNAME=rooms PATH=/usr/local/bin:/usr/bin:/bin \
         git -C /workspace/repo remote remove origin"
}

run_warm() {
    [ -s "$WARM" ] || return 0
    chown rooms:rooms "$WARM"
    chmod 0500 "$WARM"
    su rooms -s /bin/sh -c \
        "exec env -i HOME=/home/rooms USER=rooms LOGNAME=rooms \
         PATH=/usr/local/bin:/usr/bin:/bin ROOMS_NEUTRAL_WARM=1 /bin/sh '$WARM'"
}

ipv6_is_disabled() {
    [ ! -s /proc/net/if_inet6 ] || return 1
    [ ! -e /sys/module/ipv6/parameters/disable ] \
        || grep -Eq '^(1|Y)$' /sys/module/ipv6/parameters/disable
}

no_ssh_surface() {
    ! ps -eo comm= 2>/dev/null | grep -Eq '(^|/)sshd$' || return 1
    for table in /proc/net/tcp /proc/net/tcp6; do
        [ -r "$table" ] || continue
        if awk '$4 == "0A" && $2 ~ /:0016$/ { found=1 } END { exit !found }' "$table"; then
            return 1
        fi
    done
    return 0
}

process_identity() {
    proc="$1"
    pid="${proc##*/}"
    IFS= read -r stat <"$proc/stat" || return 1
    rest="${stat##*) }"
    [ "$rest" != "$stat" ] || return 1
    set -- $rest
    [ "$#" -ge 20 ] || return 1
    shift 19
    printf '%s:%s\n' "$pid" "$1"
}

capture_process_baseline() {
    baseline="$1"
    : >"$baseline"
    for proc in /proc/[0-9]*; do
        [ -L "$proc/exe" ] || continue
        process_identity "$proc" >>"$baseline" || true
    done
}

no_post_warm_processes() {
    baseline="$1"
    for proc in /proc/[0-9]*; do
        [ -L "$proc/exe" ] || continue
        identity="$(process_identity "$proc")" || continue
        grep -Fxq "$identity" "$baseline" && continue
        IFS= read -r comm <"$proc/comm" || comm=unknown
        printf 'post-warm process survived: %s (%s)\n' "$comm" "$identity" >&2
        return 1
    done
}

emit_beacon() {
    # The caller replaces all three stdio descriptors before forking this
    # helper, so it cannot retain the provisioning stream. The host binds this
    # listener before boot but accepts only after provisioning EOF; an early
    # connect queues safely and preserves that structural ordering.
    printf 'quiesced\n' | socat - VSOCK-CONNECT:2:5002
}

session() {
    IFS= read -r preface || fail "missing protocol preface"
    [ "$preface" = "ROOMS-PROVISION/1" ] || fail "unsupported protocol"
    prepare_provision_dir
    capture_process_baseline "$PROCESS_BASELINE"

    read_frame BUNDLE "$BUNDLE"
    printf 'ACK stage\n'
    clone_bundle
    printf 'ACK clone\n'

    read_frame WARM "$WARM"
    IFS=' ' read -r end end_length || fail "missing END frame"
    [ "$end" = END ] && [ "$end_length" = 0 ] || fail "malformed END frame"
    revoke_workload_sudo
    capture_protected_state
    credential_state_is_safe || fail "configured credential source present before warm"
    run_warm
    rooms_sudo_is_revoked || fail "warm command reacquired sudo authority"
    verify_protected_state
    credential_state_is_safe || fail "warm command left a configured credential source"
    printf 'ACK warm\n'

    rm -f "$BUNDLE" "$WARM" "$PROTECTED_BEFORE" "$PROTECTED_AFTER"
    for service in sshd crond syslog acpid; do
        rc-service "$service" stop >/dev/null 2>&1 || true
    done
    ipv6_is_disabled || fail "IPv6 is not provably disabled"
    no_ssh_surface || fail "SSH process or listener survived quiesce"
    no_post_warm_processes "$PROCESS_BASELINE" || fail "warm descendant survived quiesce"
    rm -f "$PROCESS_BASELINE"
    rmdir "$PROVISION_DIR"
    emit_beacon </dev/null >/dev/null 2>&1 &
}

if [ "${ROOMS_AGENT_LIBRARY_ONLY:-0}" = 1 ]; then
    return 0 2>/dev/null || exit 0
fi

[ "${1:-}" = session ] || fail "usage: rooms-provision-agent session"
session
