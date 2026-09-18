#!/usr/bin/env bash
# Rooms substrate for `swarm gym team --seat-cmd`: one seat turn inside a kept-alive clone.
#
# Setup once (as the box user, token already in ~/.swarm-token, 0600):
#   rooms-seat.sh up  <n> <wall-seconds> <out-dir>     keep N clones alive, start git daemon + binary server
#   rooms-seat.sh down                                  kill the clones
# Per turn (called by the runner with SEAT, PROMPT_FILE, RESUME, DIR, REMOTE, MODEL, TURNS,
# SWARM_STORE in the environment): rooms-seat.sh turn   -> prints the seat's JSON line last.
set -euo pipefail
STATE=${ROOMS_SEAT_STATE:-$HOME/seat-state}
HOST_IP=${ROOMS_SEAT_HOST_IP:-10.128.0.3}
GUEST_IP=${ROOMS_SEAT_GUEST_IP:-172.16.0.6}
ROOMS=${ROOMS_BIN:-$HOME/rooms/target/release/rooms}
SNAP=${ROOMS_SEAT_SNAPSHOT:-$HOME/lab/snap2g}
IMAGE=${ROOMS_SEAT_IMAGE:-$HOME/rooms/images/agent-alpine.ext4}
TOOLSTORE=${ROOMS_SEAT_TOOLSTORE:-$HOME/toolstores/go}
BIN_PORT=8765
KEY=${ROOMS_SEAT_KEY:-$HOME/.ssh/id_rooms}
SSH_OPTS=(-i "$KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=10)

guest() { # guest <index> <command...>
    local idx=$1; shift
    sudo ip netns exec "rooms-c$idx" ssh -n "${SSH_OPTS[@]}" "rooms@$GUEST_IP" "$@"
}
put() { # put <index> <local> <remote>
    sudo ip netns exec "rooms-c$1" scp "${SSH_OPTS[@]}" -q "$2" "rooms@$GUEST_IP:$3"
}

up() {
    local n=$1 wall=$2 out=$3
    mkdir -p "$STATE/seats" "$out"
    echo "$n" > "$STATE/count"
    echo "$wall" > "$STATE/wall"
    # Serve the swarm binary and the runner's bare remotes to guests over the host LAN address.
    # StrictHostKeyChecking=no is deliberate: every restored clone has fresh host keys.
    mkdir -p "$STATE/serve" && cp "$HOME/swarm" "$STATE/serve/swarm"  # serve only the binary, never $HOME
    (cd "$STATE/serve" && setsid nohup python3 -m http.server "$BIN_PORT" --bind "$HOST_IP" < /dev/null > "$STATE/http.log" 2>&1 & echo $! > "$STATE/http.pid")
    # receive-pack is required: a landing is a branch the seat itself pushes to origin.
    setsid nohup git daemon --base-path="$out" --export-all --enable=receive-pack --reuseaddr \
        --listen="$HOST_IP" --port=9418 < /dev/null > "$STATE/gitd.log" 2>&1 &
    echo $! > "$STATE/gitd.pid"
    # The token enters only the launcher's environment; the clone receives it after resume.
    [ -s "$HOME/.swarm-token" ] || { echo "no ~/.swarm-token" >&2; exit 1; }
    (CLAUDE_CODE_OAUTH_TOKEN=$(cat "$HOME/.swarm-token") setsid nohup sudo -E env HOME="$HOME" "$ROOMS" clone "$SNAP" \
        --image "$IMAGE" --toolstore "$TOOLSTORE" -n "$n" --secret CLAUDE_CODE_OAUTH_TOKEN \
        --command "sleep $wall" --max-wall "$((wall + 120))s" --out "$out/rooms" --json \
        < /dev/null > "$STATE/clone.json" 2> "$STATE/clone.err" &)
    local ok=0
    for _ in $(seq 1 90); do
        ok=0
        for k in $(seq 1 "$n"); do sudo ip netns exec "rooms-c$k" nc -z -w1 "$GUEST_IP" 22 2>/dev/null && ok=$((ok + 1)); done
        [ "$ok" = "$n" ] && break
        sleep 2
    done
    [ "$ok" = "$n" ] || { echo "only $ok of $n clones reachable" >&2; exit 1; }
    for k in $(seq 1 "$n"); do
        guest "$k" "wget -q -O ~/swarm http://$HOST_IP:$BIN_PORT/swarm && chmod +x ~/swarm && mkdir -p ~/prompts ~/work"
    done
    echo "up: $n clones alive for ${wall}s"
}

down() {
    sudo pkill -f "sleep $(cat "$STATE/wall" 2>/dev/null || echo 99999)" 2>/dev/null || true
    for f in http gitd; do kill "$(cat "$STATE/$f.pid" 2>/dev/null)" 2>/dev/null || true; done
    sleep 5
    sudo "$ROOMS" ls
}

seat_index() { # allocate a clone to a seat name, once; mkdir is the atomic claim
    local f="$STATE/seats/$SEAT"
    if [ -f "$f" ]; then cat "$f"; return; fi
    local n i; n=$(cat "$STATE/count")
    for i in $(seq 1 "$n"); do
        if mkdir "$STATE/seats/.clone-$i" 2>/dev/null; then
            echo "$i" > "$f"
            echo "$i"
            return
        fi
    done
    echo "no free clone for seat $SEAT" >&2
    exit 1
}

turn() {
    local idx; idx=$(seat_index)
    local remote="git://$HOST_IP/$(basename "$REMOTE")"
    put "$idx" "$PROMPT_FILE" "prompts/$SEAT.txt"
    # Every runner-supplied value is passed through printf %q so the guest shell
    # sees it as one word. The seat reads /run/rooms/secrets.env itself.
    local cmd
    cmd=$(printf 'export PATH=/nix/var/rooms/env/bin:$PATH SWARM_STORE=%q SWARM_INCARNATION=$(cat /proc/sys/kernel/random/uuid); cd ~ && ~/swarm seat run --seat %q --prompt %q --dir %q --remote %q --resume %q --model %q --turns %q --skip-permissions' \
        "$SWARM_STORE" "$SEAT" "prompts/$SEAT.txt" "work/$SEAT" "$remote" "${RESUME:-}" "${MODEL:-}" "${TURNS:-0}")
    guest "$idx" "$cmd"
}

case "${1:-}" in
    up) shift; up "$@" ;;
    down) down ;;
    turn) turn ;;
    *) echo "usage: rooms-seat.sh up <n> <wall-seconds> <out-dir> | down | turn" >&2; exit 2 ;;
esac
