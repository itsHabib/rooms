#!/usr/bin/env bash
# box.sh — create, provision, check, and delete disposable rooms hosts ("boxes").
#
# A box is an Ubuntu 24.04 VM with a usable /dev/kvm. Two backends create one:
#   lima  a local VM on an Apple Silicon Mac (M3+), from scripts/lima-rooms-host.yaml
#   gcp   an auto-deleting Spot VM with nested virtualization, in a project you name
#
# Every backend hands back the same thing, an SSH config file and a host alias,
# so provision and check run identically on both.
#
# Usage:
#   scripts/box.sh up <name> --backend lima|gcp [--project <gcp-project>]
#   scripts/box.sh provision <name> [--rev <git-rev>]
#   scripts/box.sh check <name>
#   scripts/box.sh ssh <name> [<command>...]
#   scripts/box.sh down <name>
#
# `up` prints one JSON line ({name, backend, ssh_config, host}) on stdout; all
# progress goes to stderr. `check` passes only when every `rooms doctor` check
# is ok (warnings allowed) and saves the report beside the box's state.
#
# State lives under ${ROOMS_BOX_STATE:-~/.rooms-box}/<name>/. `up` refuses a name
# the backend already has and stamps a random token on the VM it creates; `down`
# deletes only a VM carrying that token, so it never removes one it did not create.
#
# GCP settings (environment):
#   ROOMS_BOX_GCP_PROJECT   required unless --project is given; gcloud's active
#                           project is never used implicitly
#   ROOMS_BOX_GCP_ZONE      default us-central1-a
#   ROOMS_BOX_GCP_MACHINE   default n2-standard-4 (Intel; E2, Arm, and most AMD
#                           machine types cannot nest)
#   ROOMS_BOX_GCP_MAX_RUN   default 3h; GCP deletes the VM when it elapses
#   ROOMS_BOX_GCP_DISK      boot disk size, default 50GB
#
# ROOMS_BOX_SSH_TIMEOUT bounds how long `up` waits for SSH (default 300 seconds).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STATE_ROOT="${ROOMS_BOX_STATE:-$HOME/.rooms-box}"
SSH_TIMEOUT="${ROOMS_BOX_SSH_TIMEOUT:-300}"
GCP_ZONE="${ROOMS_BOX_GCP_ZONE:-us-central1-a}"

log()   { printf '[box] %s\n' "$*" >&2; }
fatal() { printf '[box] error: %s\n' "$*" >&2; exit 1; }

usage() {
    cat >&2 <<'EOF'
usage:
  box.sh up <name> --backend lima|gcp [--project <gcp-project>]
  box.sh provision <name> [--rev <git-rev>]
  box.sh check <name>
  box.sh ssh <name> [<command>...]
  box.sh down <name>
EOF
    exit 2
}

require_cmd() {
    local cmd
    for cmd in "$@"; do
        command -v "$cmd" >/dev/null 2>&1 || fatal "missing required command: $cmd"
    done
}

# Names become Lima instance names and GCE instance names; this is the
# intersection of what both accept.
validate_name() {
    [[ "$1" =~ ^[a-z]([a-z0-9-]{0,38}[a-z0-9])?$ ]] \
        || fatal "invalid box name '$1': use lowercase letters, digits, and hyphens, starting with a letter"
}

box_dir() { printf '%s/%s\n' "$STATE_ROOT" "$1"; }

# A random token `up` stamps on the VM it creates (a Lima param, a GCP label).
# `down` deletes only a VM carrying its box's token, never one that merely
# shares the name.
new_token() { od -An -N8 -tx1 /dev/urandom | tr -d ' \n'; }

# record DIR KEY VALUE [KEY VALUE...] appends shell-quoted assignments in one
# write, so an interrupted call leaves all of its pairs or none.
record() {
    local dir="$1" lines=""
    shift
    while [[ $# -ge 2 ]]; do
        lines+="$(printf '%s=%q' "$1" "$2")"$'\n'
        shift 2
    done
    printf '%s' "$lines" >>"$dir/box.env"
}

load_box() {
    validate_name "$1"
    BOX_DIR="$(box_dir "$1")"
    [[ -f "$BOX_DIR/box.env" ]] \
        || fatal "no box named '$1' under $STATE_ROOT; box.sh only manages boxes it created"
    # Trusted input: the state dir is created mode 700 by this user and every
    # value in box.env was shell-quoted by record().
    # shellcheck disable=SC1091
    source "$BOX_DIR/box.env"
}

remote() {
    ssh -F "$BOX_SSH_CONFIG" -o BatchMode=yes -o ConnectTimeout=10 "$BOX_HOST" "$@"
}

wait_for_ssh() {
    local deadline=$((SECONDS + SSH_TIMEOUT))
    until remote true 2>/dev/null; do
        ((SECONDS < deadline)) || fatal "$1 did not accept SSH within ${SSH_TIMEOUT}s"
        sleep 5
    done
}

# --- lima backend ---

# Prints "<name> <token>" per instance; the token is empty on instances this
# script did not create.
lima_instances() {
    limactl list --format '{{.Name}} {{index .Config.Param "roomsBoxToken"}}'
}

lima_check_free() {
    local instances
    instances="$(lima_instances)" || fatal "could not list lima instances"
    if awk -v n="$1" '$1 == n { found = 1 } END { exit !found }' <<<"$instances"; then
        fatal "lima already has an instance named '$1'; pick another name"
    fi
}

lima_up() {
    # $3 is the gcp project, unused here.
    local name="$1" dir="$2" token="$4" expr instance_dir
    # Lima rejects a param nothing uses, so a provision step consumes it, which
    # also leaves the token readable in the guest at /etc/rooms-box-token.
    # shellcheck disable=SC2016 # $PARAM_roomsBoxToken expands in the guest
    expr='.mounts = [] | .param.roomsBoxToken = "'"$token"'" | .provision += [{"mode": "system", "script": "#!/bin/sh\nprintf %s \"$PARAM_roomsBoxToken\" > /etc/rooms-box-token\n"}]'
    limactl create --tty=false --name "$name" --set "$expr" \
        "$REPO_ROOT/scripts/lima-rooms-host.yaml" >&2
    limactl start --tty=false "$name" >&2
    instance_dir="$(limactl list --format '{{.Dir}}' "$name")"
    [[ -f "$instance_dir/ssh.config" ]] || fatal "lima wrote no ssh.config under $instance_dir"
    record "$dir" BOX_SSH_CONFIG "$instance_dir/ssh.config" BOX_HOST "lima-$name"
}

lima_down() {
    local name="$1" instances
    instances="$(lima_instances)" || fatal "could not list lima instances; state kept in $BOX_DIR"
    if ! awk -v n="$name" -v t="$BOX_TOKEN" '$1 == n && $2 == t { found = 1 } END { exit !found }' <<<"$instances"; then
        log "no lima instance named $name carries this box's token; nothing to delete"
        return 0
    fi
    limactl delete --force "$name" >&2
}

# --- gcp backend ---

# gcp_find PROJECT ZONE NAME [TOKEN] prints the matching instance names.
gcp_find() {
    local filter="name=$3"
    [[ -z "${4:-}" ]] || filter+=" AND labels.rooms_box_token=$4"
    gcloud compute instances list --project="$1" --zones="$2" \
        --filter="$filter" --format='value(name)'
}

gcp_check_free() {
    local found
    found="$(gcp_find "$2" "$GCP_ZONE" "$1")" || fatal "could not look up gcp instances in $2"
    [[ -z "$found" ]] || fatal "gcp project $2 already has an instance named '$1' in $GCP_ZONE; pick another name"
}

gcp_up() {
    local name="$1" dir="$2" project="$3" token="$4" ip
    local zone="$GCP_ZONE"
    ssh-keygen -q -t ed25519 -N '' -C "rooms-box-$name" -f "$dir/id_ed25519"
    printf 'rooms:%s\n' "$(cat "$dir/id_ed25519.pub")" >"$dir/ssh-keys"
    # Recorded before creation so `down` can clean up a half-created box.
    record "$dir" BOX_PROJECT "$project" BOX_ZONE "$zone"
    gcloud compute instances create "$name" \
        --project="$project" --zone="$zone" \
        --machine-type="${ROOMS_BOX_GCP_MACHINE:-n2-standard-4}" \
        --enable-nested-virtualization \
        --provisioning-model=SPOT \
        --instance-termination-action=DELETE \
        --max-run-duration="${ROOMS_BOX_GCP_MAX_RUN:-3h}" \
        --image-family=ubuntu-2404-lts-amd64 --image-project=ubuntu-os-cloud \
        --boot-disk-size="${ROOMS_BOX_GCP_DISK:-50GB}" \
        --labels="purpose=rooms-box,rooms_box_token=$token" \
        --metadata-from-file=ssh-keys="$dir/ssh-keys" >&2
    ip="$(gcloud compute instances describe "$name" --project="$project" --zone="$zone" \
        --format='value(networkInterfaces[0].accessConfigs[0].natIP)')"
    [[ -n "$ip" ]] || fatal "gcp instance $name has no external IP"
    cat >"$dir/ssh.config" <<EOF
Host box-$name
  HostName $ip
  User rooms
  IdentityFile $dir/id_ed25519
  IdentitiesOnly yes
  UserKnownHostsFile $dir/known_hosts
  StrictHostKeyChecking accept-new
  ServerAliveInterval 30
  LogLevel ERROR
EOF
    record "$dir" BOX_SSH_CONFIG "$dir/ssh.config" BOX_HOST "box-$name"
}

# Deletes only an instance carrying this box's token. None found means the box
# is already gone (Spot preemption and max run duration both delete it); a
# failed lookup is not, so it stops before the state is removed.
gcp_down() {
    local name="$1" found
    found="$(gcp_find "$BOX_PROJECT" "$BOX_ZONE" "$name" "$BOX_TOKEN")" \
        || fatal "could not look up gcp instance $name; state kept in $BOX_DIR"
    if [[ -z "$found" ]]; then
        log "no gcp instance named $name carries this box's token; nothing to delete"
        return 0
    fi
    gcloud compute instances delete "$name" --project="$BOX_PROJECT" --zone="$BOX_ZONE" --quiet >&2
}

# --- commands ---

cmd_up() {
    local name="${1:-}" backend="" project="${ROOMS_BOX_GCP_PROJECT:-}" dir token
    [[ -n "$name" ]] || usage
    shift
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --backend) backend="${2:-}"; shift 2 ;;
            --project) project="${2:-}"; shift 2 ;;
            *) usage ;;
        esac
    done
    validate_name "$name"
    require_cmd ssh jq
    case "$backend" in
        lima) require_cmd limactl ;;
        gcp)
            [[ -n "$project" ]] || fatal "gcp needs an explicit project: pass --project or set ROOMS_BOX_GCP_PROJECT"
            require_cmd gcloud ssh-keygen
            ;;
        *) fatal "--backend must be lima or gcp" ;;
    esac
    dir="$(box_dir "$name")"
    [[ ! -e "$dir" ]] || fatal "box '$name' already exists ($dir); run 'box.sh down $name' first"
    "${backend}_check_free" "$name" "$project"
    token="$(new_token)"
    [[ "$token" =~ ^[0-9a-f]{16}$ ]] || fatal "could not generate a box token"
    mkdir -p "$dir"
    chmod 700 "$dir"
    record "$dir" BOX_BACKEND "$backend" BOX_TOKEN "$token"
    log "creating $backend box $name"
    "${backend}_up" "$name" "$dir" "$project" "$token"
    load_box "$name"
    wait_for_ssh "$name"
    jq -cn --arg name "$name" --arg backend "$BOX_BACKEND" \
        --arg ssh_config "$BOX_SSH_CONFIG" --arg host "$BOX_HOST" \
        '{name: $name, backend: $backend, ssh_config: $ssh_config, host: $host}'
}

# Ships the committed tree at REV (never the working tree), keeping the box's
# images/ and target/ so a re-provision reuses downloads and the build cache.
cmd_provision() {
    local name="${1:-}" rev="HEAD" sha
    [[ -n "$name" ]] || usage
    shift
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --rev) rev="${2:-}"; shift 2 ;;
            *) usage ;;
        esac
    done
    load_box "$name"
    require_cmd git ssh
    sha="$(git -C "$REPO_ROOT" rev-parse --verify --quiet "$rev^{commit}")" \
        || fatal "unknown revision: $rev"
    log "shipping rooms $sha to $name"
    remote 'mkdir -p ~/rooms && find ~/rooms -mindepth 1 -maxdepth 1 ! -name images ! -name target -exec rm -rf {} +'
    git -C "$REPO_ROOT" archive --format=tar "$sha" | remote 'tar -x -C ~/rooms'
    remote "printf '%s\n' $sha > ~/rooms/.box-revision"
    log "installing host dependencies"
    remote 'bash ~/rooms/scripts/setup-rooms-host.sh' >&2
    remote 'sudo bash ~/rooms/scripts/setup-tap.sh --host' >&2
    log "building rooms"
    remote 'source ~/.cargo/env && cd ~/rooms && cargo build --release --locked && sudo install -m 0755 target/release/rooms /usr/local/bin/rooms' >&2
    log "provisioned $name at $sha"
}

cmd_check() {
    local name="${1:-}" report failed
    [[ -n "$name" ]] || usage
    load_box "$name"
    require_cmd ssh jq
    report="$BOX_DIR/doctor.json"
    # doctor exits non-zero on any failed check; the report, not the exit code, decides.
    # shellcheck disable=SC2016 # $HOME must expand on the box, not locally
    remote 'sudo -E rooms doctor --json --image "$HOME/rooms/images/rootfs.ext4"' >"$report.tmp" || true
    jq -e '.schema_version == 1
        and (.checks | type) == "array" and (.checks | length) > 0
        and all(.checks[]; (.name | type) == "string" and (.ok | type) == "boolean" and (.message | type) == "string")' \
        "$report.tmp" >/dev/null 2>&1 \
        || fatal "rooms doctor on $name returned no readable report (want schema_version 1 with boolean ok fields; raw output in $report.tmp)"
    mv "$report.tmp" "$report"
    jq -r '.checks[] | select(.ok == true and (.message | startswith("warn:"))) | "[box] warn \(.name): \(.message)"' "$report" >&2
    jq -r '.checks[] | select(.ok == false) | "[box] FAIL \(.name): \(.message)"' "$report" >&2
    failed="$(jq '[.checks[] | select(.ok == false)] | length' "$report")"
    [[ "$failed" -eq 0 ]] || fatal "$name is not ready: $failed rooms doctor check(s) failed (report: $report)"
    log "$name is ready: every rooms doctor check passed (report: $report)"
}

cmd_ssh() {
    local name="${1:-}"
    [[ -n "$name" ]] || usage
    shift
    load_box "$name"
    exec ssh -F "$BOX_SSH_CONFIG" "$BOX_HOST" "$@"
}

cmd_down() {
    local name="${1:-}"
    [[ -n "$name" ]] || usage
    load_box "$name"
    log "deleting $BOX_BACKEND box $name"
    "${BOX_BACKEND}_down" "$name"
    rm -rf "$BOX_DIR"
}

main() {
    local cmd="${1:-}"
    [[ -n "$cmd" ]] || usage
    shift
    case "$cmd" in
        up) cmd_up "$@" ;;
        provision) cmd_provision "$@" ;;
        check) cmd_check "$@" ;;
        ssh) cmd_ssh "$@" ;;
        down) cmd_down "$@" ;;
        *) usage ;;
    esac
}

main "$@"
