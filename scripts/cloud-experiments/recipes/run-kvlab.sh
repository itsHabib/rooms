#!/usr/bin/env bash
# First real-agent run: flat, 2 seats, kvlab, claude-sonnet-5, 150 turns, 20 min wall, seats in Rooms clones.
set -euo pipefail
OUT=${1:-$HOME/runs/kvlab-flat-rooms}
STORE=resp:10.128.0.3:6379/kvlab:
pgrep -x redis-server >/dev/null || redis-server --bind 10.128.0.3 --port 6379 --protected-mode no --daemonize yes --dir /tmp --save "" >/dev/null
rm -rf "$HOME/seat-state"
~/rooms-seat.sh up 2 1500 "$OUT"
~/swarm gym team --shape flat --n 2 --goal kvlab --model claude-sonnet-5 --turns 150 --wall 20m \
  --store "$STORE" --out "$OUT" --seat-cmd "$HOME/rooms-seat.sh turn" > "$OUT/team.log" 2>&1 || echo "team exit $?"
tail -5 "$OUT/team.log"
ls "$OUT"; cat "$OUT/result.json" 2>/dev/null | head -c 1500; echo
~/rooms-seat.sh down
