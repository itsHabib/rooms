#!/bin/sh
# Post-restore hygiene agent — the guest half of the resume nudge.
#
# Runs as a boot-time poll loop baked into the neutral base (so it is in the
# pre-warm process baseline and survives quiesce). No connection is held
# across a snapshot: every attempt is a fresh bounded VSOCK connect that
# fails fast in an ordinary base (the host binds the 5003 listener only in a
# restored room's jail). After a restore the loop reconnects, applies the
# hygiene nudge, and ACKs only once every step has succeeded.
set -eu

RESUME_DIR=/run/rooms
SECRETS_DIR=/run/rooms-secrets
SSH_HOST_KEY_DIR=/etc/ssh
SSH_HOST_KEY="$SSH_HOST_KEY_DIR/ssh_host_ed25519_key"
SSHD_SOURCE_CONFIG=/etc/ssh/sshd_config
SSHD_CONFIG="$RESUME_DIR/sshd_config"
SSHD_BIN=/usr/sbin/sshd
SSHD_RUNTIME_DIR=/run/sshd
ROOT_UID=0
ROOT_GID=0
SUDOERS_DIR=/etc/sudoers.d
SUDOERS_FILE="$SUDOERS_DIR/rooms"
SUDOERS_GRANT='rooms ALL=(ALL) NOPASSWD: ALL'

fail() {
    printf 'ERR %s\n' "$*" >&2
    exit 1
}

# Stream a progress step to the host over the protocol stream (stdout). A
# snapshot-resumed guest's serial console is detached, so this is the host's
# only visibility into hygiene; the host logs each STEP and gates on the
# terminal ACK.
step() {
    printf 'STEP %s\n' "$*"
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
        # dd bs=1 count=N reads EXACTLY N bytes — one read(2) per byte, so it
        # never consumes into the next frame's header. `head -c N` on a socket
        # over-reads (a single large read() grabs the following frame bytes and
        # discards them), which silently desyncs the stream.
        dd bs=1 count="$length" >"$destination" 2>/dev/null
    fi
    actual="$(wc -c <"$destination")"
    [ "$actual" -eq "$length" ] || fail "short $expected frame: wanted $length bytes, got $actual"
}

stage_secrets() {
    source_file="$1"
    if [ ! -s "$source_file" ]; then
        rm -f "$source_file"
        return 0
    fi
    install -d -m 0700 -o rooms -g rooms "$SECRETS_DIR"
    install -m 0400 -o rooms -g rooms "$source_file" "$SECRETS_DIR/env"
    rm -f "$source_file"
}

valid_room_identity() {
    room_id="$1"
    [ "${#room_id}" -eq 26 ] || return 1
    case "$room_id" in
        *[!0-9a-z]*) return 1 ;;
    esac
}

# Replace the restored rooms user's global config with an identity derived only
# from the canonical room id. Writing the minimal file directly avoids invoking
# a shell through git and cannot retain a credential helper from the base.
write_git_identity() {
    room_id="$1"
    destination="$2"
    valid_room_identity "$room_id" || fail "invalid room identity"
    umask 077
    {
        printf '[user]\n'
        printf '\tname = rooms %s\n' "$room_id"
        printf '\temail = %s@rooms.invalid\n' "$room_id"
    } >"$destination" || fail "cannot write git identity"
}

# Apply the same identity to the one repository a neutral base can carry.
# The caller runs this helper as `rooms`, so git's lock+rename preserves that
# ownership and a hostile warm command cannot turn this root hygiene step into
# a symlink write. Other local config is left intact.
update_repository_git_identity() {
    room_id="$1"
    repo="$2"
    valid_room_identity "$room_id" || fail "invalid room identity"
    [ ! -L "$repo" ] || fail "repository path is a symlink"
    [ -e "$repo" ] || return 0
    [ -d "$repo" ] || fail "repository path is not a directory"

    git_dir="$repo/.git"
    [ ! -L "$git_dir" ] || fail "repository git directory is a symlink"
    [ -e "$git_dir" ] || return 0
    [ -d "$git_dir" ] || fail "repository git path is not a directory"

    config="$git_dir/config"
    [ ! -L "$config" ] || fail "repository git config is a symlink"
    [ ! -e "$config" ] || [ -f "$config" ] || fail "repository git config is not regular"
    git -C "$repo" config --local user.name "rooms $room_id" \
        || fail "cannot write repository git name"
    git -C "$repo" config --local user.email "$room_id@rooms.invalid" \
        || fail "cannot write repository git email"
    [ ! -L "$config" ] && [ -f "$config" ] \
        || fail "repository git config changed shape"
}

install_repository_git_identity() {
    [ ! -L /workspace ] && [ -d /workspace ] || fail "workspace path is not a real directory"
    su rooms -s /bin/sh -c \
        '/sbin/rooms-resume-agent repository-identity' \
        || fail "cannot install repository git identity"

    config=/workspace/repo/.git/config
    [ -e /workspace/repo/.git ] || return 0
    [ ! -L "$config" ] && [ -f "$config" ] \
        || fail "repository git config changed shape"
    rooms_uid="$(id -u rooms)" || fail "cannot resolve rooms uid"
    rooms_gid="$(id -g rooms)" || fail "cannot resolve rooms gid"
    owner="$(stat -c '%u:%g' "$config")" || fail "cannot inspect repository git config owner"
    [ "$owner" = "$rooms_uid:$rooms_gid" ] || fail "repository git config is not rooms-owned"
}

fresh_git_identity() {
    room_id="$1"
    destination=/home/rooms/.gitconfig
    pending=/home/rooms/.gitconfig.rooms-new
    [ ! -L /home ] && [ -d /home ] || fail "home root is not a real directory"
    [ ! -L /home/rooms ] && [ -d /home/rooms ] || fail "rooms home is not a real directory"
    rm -f "$pending" || fail "cannot clear pending git identity"
    [ ! -d "$destination" ] || fail "git identity path is a directory"
    write_git_identity "$room_id" "$pending"
    chown rooms:rooms "$pending" || fail "cannot own git identity"
    chmod 0600 "$pending" || fail "cannot protect git identity"
    mv -f "$pending" "$destination" || fail "cannot install git identity"
    install_repository_git_identity
}

repository_identity_session() {
    rooms_uid="$(id -u rooms)" || fail "cannot resolve rooms uid"
    [ "$(id -u)" = "$rooms_uid" ] || fail "repository identity must run as rooms"
    IFS= read -r room_id <"$RESUME_DIR/identity" || fail "missing room identity"
    update_repository_git_identity "$room_id" /workspace/repo
}

clear_ssh_host_key_paths() {
    [ ! -L "$SSH_HOST_KEY_DIR" ] && [ -d "$SSH_HOST_KEY_DIR" ] \
        || fail "SSH host-key directory is not a real directory"
    for path in "$SSH_HOST_KEY_DIR"/ssh_host_*; do
        [ -e "$path" ] || [ -L "$path" ] || continue
        [ ! -d "$path" ] || fail "SSH host-key path is a directory: $path"
        rm -f "$path" || fail "cannot remove existing SSH host-key path: $path"
    done
}

ssh_host_key_pair_matches() {
    private_key="$1"
    public_key="$2"
    [ ! -L "$private_key" ] && [ -f "$private_key" ] || return 1
    [ ! -L "$public_key" ] && [ -f "$public_key" ] || return 1
    derived="$(ssh-keygen -y -f "$private_key" 2>/dev/null \
        | awk 'NR == 1 { print $1 " " $2; next } { exit 1 }')" || return 1
    published="$(awk 'NR == 1 { print $1 " " $2; next } { exit 1 }' "$public_key")" \
        || return 1
    case "$published" in
        'ssh-ed25519 '*) ;;
        *) return 1 ;;
    esac
    [ "$derived" = "$published" ]
}

# Generate exactly one post-reseed host key. The neutral base carries no host
# private keys, but clearing every pre-existing shape here also fails closed if
# a malformed overlay or symlink somehow reaches resume.
fresh_ssh_host_key() {
    clear_ssh_host_key_paths
    umask 077
    ssh-keygen -q -t ed25519 -N '' -f "$SSH_HOST_KEY" >/dev/null 2>&1 \
        || fail "cannot generate fresh Ed25519 SSH host key"
    chmod 0600 "$SSH_HOST_KEY" || fail "cannot protect SSH host private key"
    chmod 0644 "$SSH_HOST_KEY.pub" || fail "cannot protect SSH host public key"
    ssh_host_key_pair_matches "$SSH_HOST_KEY" "$SSH_HOST_KEY.pub" \
        || fail "fresh Ed25519 SSH host key failed verification"
}

# Derive a runtime sshd config from the immutable image config while removing
# every alternate key source. Prepending the one HostKey keeps it in global
# scope even when the source config ends with a Match block.
write_pinned_sshd_config() {
    [ ! -L "$SSHD_SOURCE_CONFIG" ] && [ -f "$SSHD_SOURCE_CONFIG" ] \
        || fail "sshd source config is not a regular file"
    pending="$SSHD_CONFIG.rooms-new"
    rm -f "$pending" || fail "cannot clear pending sshd config"
    [ ! -d "$SSHD_CONFIG" ] || fail "sshd runtime config path is a directory"
    {
        printf 'HostKey %s\n' "$SSH_HOST_KEY"
        awk '
            {
                line = $0
                sub(/^[[:space:]]*/, "", line)
                directive = line
                sub(/[=[:space:]].*$/, "", directive)
                directive = tolower(directive)
                if (directive == "hostkey" || directive == "hostcertificate" ||
                    directive == "hostkeyagent" || directive == "include") {
                    next
                }
                print
            }
        ' "$SSHD_SOURCE_CONFIG"
    } >"$pending" || fail "cannot write pinned sshd config"
    chmod 0600 "$pending" || fail "cannot protect pinned sshd config"
    mv -f "$pending" "$SSHD_CONFIG" || fail "cannot install pinned sshd config"
}

pinned_sshd_config_is_safe() {
    config="$1"
    expected_key="$2"
    [ ! -L "$config" ] && [ -f "$config" ] || return 1
    awk -v expected="$expected_key" '
        {
            line = $0
            sub(/^[[:space:]]*/, "", line)
            if (line == "" || substr(line, 1, 1) == "#") {
                next
            }
            directive = line
            sub(/[=[:space:]].*$/, "", directive)
            directive = tolower(directive)
            value = line
            sub(/^[^=[:space:]]+/, "", value)
            sub(/^[=[:space:]]+/, "", value)
            sub(/[[:space:]]+$/, "", value)
            if (directive == "hostkey") {
                count++
                key = value
            }
            if (directive == "hostcertificate" || directive == "hostkeyagent" ||
                directive == "include") {
                forbidden++
            }
        }
        END { exit !(count == 1 && key == expected && forbidden == 0) }
    ' "$config"
}

effective_sshd_host_keys_are_safe() {
    expected_key="$1"
    awk -v expected="$expected_key" '
        tolower($1) == "hostkey" {
            count++
            key = $2
        }
        END { exit !(count == 1 && key == expected) }
    '
}

effective_sshd_config_is_safe() {
    effective="$("$SSHD_BIN" -T -f "$SSHD_CONFIG" \
        -C user=rooms,host=rooms-agent,addr=127.0.0.1 2>/dev/null)" || return 1
    printf '%s\n' "$effective" \
        | effective_sshd_host_keys_are_safe "$SSH_HOST_KEY"
}

prepare_sshd_runtime_dir() {
    [ ! -L "$SSHD_RUNTIME_DIR" ] || fail "sshd runtime directory is a symlink"
    install -d -m 0755 -o "$ROOT_UID" -g "$ROOT_GID" "$SSHD_RUNTIME_DIR" \
        || fail "cannot prepare sshd runtime directory"
    [ ! -L "$SSHD_RUNTIME_DIR" ] && [ -d "$SSHD_RUNTIME_DIR" ] \
        || fail "sshd runtime directory changed shape"
    owner="$(stat -c '%u:%g' "$SSHD_RUNTIME_DIR")" \
        || fail "cannot inspect sshd runtime directory"
    mode="$(stat -c '%a' "$SSHD_RUNTIME_DIR")" \
        || fail "cannot inspect sshd runtime directory mode"
    [ "$owner" = "$ROOT_UID:$ROOT_GID" ] && [ "$mode" = 755 ] \
        || fail "sshd runtime directory is not root-owned mode 0755"
}

pin_sshd_to_fresh_host_key() {
    write_pinned_sshd_config
    pinned_sshd_config_is_safe "$SSHD_CONFIG" "$SSH_HOST_KEY" \
        || fail "sshd config is not pinned to the fresh Ed25519 host key"
    prepare_sshd_runtime_dir
    effective_sshd_config_is_safe \
        || fail "effective sshd config does not use exactly the fresh Ed25519 host key"
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

restore_workload_sudo() {
    [ ! -L "$SUDOERS_DIR" ] && [ -d "$SUDOERS_DIR" ] \
        || fail "sudoers directory is not a real directory"
    owner="$(stat -c '%u:%g' "$SUDOERS_DIR")" || fail "cannot inspect sudoers directory owner"
    mode="$(stat -c '%a' "$SUDOERS_DIR")" || fail "cannot inspect sudoers directory mode"
    [ "$owner" = 0:0 ] && [ "${#mode}" -eq 3 ] \
        || fail "sudoers directory owner or mode is unsafe"
    case "$mode" in
        ?[2367]?|??[2367]) fail "sudoers directory is writable by group/other" ;;
    esac

    pending="$SUDOERS_FILE.rooms-new"
    rm -f "$pending" || fail "cannot clear pending rooms sudoers grant"
    [ ! -d "$SUDOERS_FILE" ] || fail "rooms sudoers path is a directory"
    umask 077
    printf '%s\n' "$SUDOERS_GRANT" >"$pending" \
        || fail "cannot write rooms workload sudo grant"
    chown root:root "$pending" || fail "cannot own rooms workload sudo grant"
    chmod 0440 "$pending" || fail "cannot protect rooms workload sudo grant"
    command -v visudo >/dev/null 2>&1 \
        || fail "visudo is unavailable for workload sudo validation"
    visudo -cf "$pending" >/dev/null 2>&1 \
        || fail "rooms workload sudo grant failed syntax validation"
    mv -f "$pending" "$SUDOERS_FILE" \
        || fail "cannot install rooms workload sudo grant"
    sudoers_grant_is_exact "$SUDOERS_FILE" 0 0 \
        || fail "installed rooms workload sudo grant changed shape"
    visudo -cf /etc/sudoers >/dev/null 2>&1 \
        || fail "effective sudo policy failed syntax validation"
    su rooms -s /bin/sh -c \
        'exec env -i HOME=/home/rooms USER=rooms LOGNAME=rooms PATH=/usr/local/bin:/usr/bin:/bin sudo -n true' \
        >/dev/null 2>&1 \
        || fail "rooms workload sudo grant is not effective"
}

# Step the clock from an epoch, tolerating busybox/coreutils date differences.
step_clock() {
    epoch="$1"
    date -u -s "@$epoch" >/dev/null 2>&1 && return 0
    stamp="$(date -u -d "@$epoch" '+%Y-%m-%d %H:%M:%S' 2>/dev/null)" || return 1
    date -u -s "$stamp" >/dev/null 2>&1
}

# Start sshd directly with the verified runtime config. Falling back to openrc
# would silently return to /etc/ssh/sshd_config and its default multi-key set.
start_sshd() {
    prepare_sshd_runtime_dir
    "$SSHD_BIN" -t -f "$SSHD_CONFIG" 2>/dev/null || return 1
    "$SSHD_BIN" -f "$SSHD_CONFIG" 2>/dev/null
}

session() {
    printf 'ROOMS-RESUME/1\n'
    IFS=' ' read -r kind room_id || fail "missing IDENTITY line"
    [ "$kind" = IDENTITY ] || fail "wanted IDENTITY, got $kind"
    valid_room_identity "$room_id" || fail "invalid room identity"
    IFS=' ' read -r kind epoch || fail "missing CLOCK line"
    [ "$kind" = CLOCK ] || fail "wanted CLOCK, got $kind"
    case "$epoch" in
        ''|*[!0-9]*) fail "invalid CLOCK epoch" ;;
    esac

    # 0755 so an unprivileged workload can read its own room identity below;
    # the transient entropy/secrets frames are removed right after use, and
    # real secrets live in the 0700 SECRETS_DIR.
    install -d -m 0755 "$RESUME_DIR"
    read_frame ENTROPY "$RESUME_DIR/.entropy"
    read_frame SECRETS "$RESUME_DIR/.secrets"
    IFS=' ' read -r end end_length || fail "missing END frame"
    [ "$end" = END ] && [ "$end_length" = 0 ] || fail "malformed END frame"

    # Reseed first: everything after (host keys) must draw post-divergence.
    cat "$RESUME_DIR/.entropy" >/dev/urandom || fail "cannot reseed /dev/urandom"
    rm -f "$RESUME_DIR/.entropy"
    step reseeded
    step_clock "$epoch" || fail "cannot step clock"
    step clock
    printf '%s\n' "$room_id" >"$RESUME_DIR/identity"
    chmod 0644 "$RESUME_DIR/identity"
    stage_secrets "$RESUME_DIR/.secrets"
    fresh_git_identity "$room_id"
    step identity
    fresh_ssh_host_key
    pin_sshd_to_fresh_host_key
    step hostkeys
    restore_workload_sudo
    step privilege
    start_sshd || fail "cannot start sshd"
    step sshd

    printf 'ACK resume\n'
}

loop() {
    # A SINGLE long-lived socat that retries the connect internally
    # (retry/interval), rather than a shell loop respawning socat+sleep every
    # iteration. That matters for the base: the quiesce gate refuses any
    # non-baseline process to survive, and a respawning loop churns fresh
    # socat/sleep pids the gate would flag. This one process is captured in the
    # baseline and forks its EXEC child only on a successful connect — which
    # only happens post-restore, after quiesce. `-T 120` bounds a wedged
    # session; socat's double-float timespec makes interval=0.1 a bounded 100ms
    # poll cadence until the host listener appears.
    exec socat -T 120 \
        VSOCK-CONNECT:2:5003,forever,interval=0.1,retry=1000000 \
        EXEC:'/sbin/rooms-resume-agent session' \
        >/dev/null 2>&1
}

if [ "${ROOMS_AGENT_LIBRARY_ONLY:-0}" = 1 ] \
    && [ "${1:-}" = __rooms_test_library__ ]; then
    return 0 2>/dev/null || exit 0
fi

case "${1:-}" in
    session) session ;;
    loop) loop ;;
    repository-identity) repository_identity_session ;;
    *) fail "usage: rooms-resume-agent {session|loop}" ;;
esac
