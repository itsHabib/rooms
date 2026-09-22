#!/usr/bin/env bash
# First real-agent run: flat, 2 seats, kvlab, claude-sonnet-5, 150 turns, 20 min wall, seats in Rooms clones.
set -euo pipefail
OUT=${1:-$HOME/runs/kvlab-flat-rooms}
HOST_IP=${ROOMS_SEAT_HOST_IP:-$(hostname -I | awk "{print \$1}")}
STORE=resp:$HOST_IP:6379/kvlab:
# Probe the exact endpoint seats will use, not just any process named
# redis-server: pgrep -x matches the command name regardless of which
# address or port it bound, so an unrelated redis-server elsewhere would
# have skipped starting ours while STORE still pointed every seat at
# $HOST_IP:6379 with nothing listening there.
(exec 3<>"/dev/tcp/$HOST_IP/6379") 2>/dev/null && exec 3<&- 3>&- || \
    redis-server --bind "$HOST_IP" --port 6379 --protected-mode no --daemonize yes --dir /tmp --save "" >/dev/null
rm -rf "$HOME/seat-state"
"$(dirname "$0")/rooms-seat.sh" up 2 1500 "$OUT"
~/swarm gym team --shape flat --n 2 --goal kvlab --model claude-sonnet-5 --turns 150 --wall 20m \
  --store "$STORE" --out "$OUT" --seat-cmd "$(dirname "$0")/rooms-seat.sh turn" > "$OUT/team.log" 2>&1 || echo "team exit $?"
tail -5 "$OUT/team.log"
ls "$OUT"; head -c 1500 "$OUT/result.json" 2>/dev/null; echo
"$(dirname "$0")/rooms-seat.sh" down
