#!/usr/bin/env bash
# examples/drive-anthropic.sh — single-shot Claude API call from inside a
# fresh rooms microVM. Reads the prompt on stdin; prints Anthropic's JSON
# response on stdout. Exit code mirrors `rooms run` (which mirrors the
# guest curl).
#
# Demonstrates the substrate's `--command` verb (POC m4) end-to-end:
# host → cargo run rooms → microVM boot → SSH exec (key forwarded via
# SendEnv) → curl HTTPS → host stdout.
#
# Requires on the host:
# - rooms checkout at $ROOMS_DIR (default: ~/dev/rooms)
# - rootfs image at $ROOMS_IMAGE (default: ~/rooms/images/rootfs.ext4)
# - ANTHROPIC_API_KEY in env (forwarded to the guest via SSH SendEnv —
#   the host never substitutes the key into the command string)
# - jq, base64, cargo
#
# Requires in the rootfs:
# - curl + system CA bundle (baked by scripts/bake-rootfs-ssh.sh)
# - AcceptEnv ANTHROPIC_API_KEY in /etc/ssh/sshd_config (also baked)
#
# Usage:
#     echo "Reply with the single word: pong" | examples/drive-anthropic.sh
#     cat task.md | examples/drive-anthropic.sh
#     examples/drive-anthropic.sh <<< "What is 2+2?"
#
# Tunables (env):
#     ROOMS_DIR     path to rooms checkout (default ~/dev/rooms)
#     ROOMS_IMAGE   path to rootfs ext4    (default ~/rooms/images/rootfs.ext4)
#     MODEL         Anthropic model id     (default claude-sonnet-4-5)
#     MAX_TOKENS    response token cap     (default 1024)
#
# Note: the prompt is base64-encoded inline into the guest command. Host
# argv limits cap the practical prompt size around ~100KB. The substrate
# has no file-injection verb yet — a future `--input <local>:<guest>`
# would be the right primitive for larger payloads.

set -euo pipefail

ROOMS_DIR="${ROOMS_DIR:-$HOME/dev/rooms}"
ROOMS_IMAGE="${ROOMS_IMAGE:-$HOME/rooms/images/rootfs.ext4}"
MODEL="${MODEL:-claude-sonnet-4-5}"
MAX_TOKENS="${MAX_TOKENS:-1024}"

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
    echo "drive-anthropic: ANTHROPIC_API_KEY not in env (forwarded to guest via SendEnv)" >&2
    exit 2
fi
if [ ! -d "$ROOMS_DIR" ]; then
    echo "drive-anthropic: rooms checkout not found at $ROOMS_DIR (set ROOMS_DIR)" >&2
    exit 2
fi
if [ ! -f "$ROOMS_IMAGE" ]; then
    echo "drive-anthropic: rootfs image not found at $ROOMS_IMAGE (set ROOMS_IMAGE)" >&2
    exit 2
fi

PROMPT="$(cat)"
if [ -z "$PROMPT" ]; then
    echo "drive-anthropic: empty prompt on stdin" >&2
    exit 2
fi

PAYLOAD=$(jq -n \
    --arg msg "$PROMPT" \
    --arg model "$MODEL" \
    --argjson max "$MAX_TOKENS" \
    '{ model: $model, max_tokens: $max, messages: [{role: "user", content: $msg}] }')
B64=$(printf '%s' "$PAYLOAD" | base64 -w0)

# Build the guest command. `$ANTHROPIC_API_KEY` MUST be expanded by the
# GUEST shell (so the key never appears on the host's argv), so it stays
# as literal `$ANTHROPIC_API_KEY` in this string; `$B64` needs host
# expansion, so it's spliced inline.
GUEST_CMD="echo ${B64} | base64 -d > /tmp/p.json && curl -sS -H 'x-api-key: '\"\$ANTHROPIC_API_KEY\"'' -H 'anthropic-version: 2023-06-01' -H 'content-type: application/json' --data-binary @/tmp/p.json https://api.anthropic.com/v1/messages"

cd "$ROOMS_DIR"
exec cargo run --quiet -- run --image "$ROOMS_IMAGE" --command "$GUEST_CMD"
