#!/bin/sh
# Extend hook for build-rootfs-alpine.sh (--extend). Runs chroot'd as root after
# the baseline install, before unmount. Adds Node + a vendored, pinned
# @cursor/sdk and drops cursor-runner.js at /opt/rooms/cursor-runner/, owned by
# the unprivileged rooms user, plus `AcceptEnv CURSOR_API_KEY` in sshd_config so
# the host can forward the key into the guest.
#
# Build the cursor image variant with:
#   sudo ./scripts/build-rootfs-alpine.sh \
#     --out images/agent-alpine-cursor.ext4 \
#     --size 1G \
#     --ssh-key ~/.ssh/id_rooms.pub \
#     --extend scripts/rootfs/install-cursor.sh
#
# Node is deliberately NOT in the base builder (the lean claude-only image
# carries no Node); this hook is the seam that adds it for cursor runs.
set -e

CURSOR_SDK_VERSION="1.0.16"
DEST="/opt/rooms/cursor-runner"
GUEST_USER="rooms"

echo "[install-cursor] apk add nodejs npm"
apk add --no-cache nodejs npm
# @cursor/sdk depends on sqlite3, a native addon with no musl prebuilt — it
# compiles from source via node-gyp, which needs python3 + a C/C++ toolchain.
# Installed as a virtual group and dropped after the vendored install so the
# runtime image doesn't carry a compiler it never uses.
apk add --no-cache --virtual .cursor-build python3 make g++ linux-headers

echo "[install-cursor] vendoring @cursor/sdk@${CURSOR_SDK_VERSION} into ${DEST}"
mkdir -p "$DEST"
cat > "$DEST/package.json" <<EOF
{
  "name": "rooms-cursor-runner",
  "private": true,
  "type": "module",
  "dependencies": { "@cursor/sdk": "${CURSOR_SDK_VERSION}" }
}
EOF
LOCKFILE="/tmp/package-lock.json"
[[ -f "$LOCKFILE" ]] || { echo "missing vendored package-lock.json at $LOCKFILE (staged by build-rootfs-alpine.sh --extend)" >&2; exit 1; }
cp "$LOCKFILE" "$DEST/package-lock.json"
( cd "$DEST" && npm ci --strict-peer-deps --no-audit --no-fund --omit=dev )

apk del .cursor-build

echo "[install-cursor] writing cursor-runner.js"
# One-shot Node ESM wrapper around @cursor/sdk. Reads task.md + meta.json from
# /workspace/in, runs the agent against /workspace/repo, streams events to
# /workspace/out/events.ndjson, writes summary.md, and signals outcome purely
# via exit code (0 succeeded, 1 agent-level failure, 2 runner/SDK error). The
# substrate owns result.json. Error taxonomy mirrors ship's LocalCursorRunner.
cat > "$DEST/cursor-runner.js" <<'CURSOR_RUNNER_JS'
import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";

const IN = "/workspace/in";
const OUT = "/workspace/out";
const EVENTS = `${OUT}/events.ndjson`;
const SUMMARY = `${OUT}/summary.md`;

// Append one JSON object per line. Never let event logging break the run — a
// circular or otherwise unserializable payload degrades to a marker line.
function emit(obj) {
  const line = (extra) =>
    JSON.stringify({ ts: new Date().toISOString(), ...extra }) + "\n";
  try {
    appendFileSync(EVENTS, line(obj));
  } catch {
    try {
      appendFileSync(EVENTS, line({ kind: "log", note: "unserializable event" }));
    } catch {
      /* give up on this line rather than crash the run */
    }
  }
}

function writeSummary(text) {
  try {
    writeFileSync(SUMMARY, text == null ? "" : String(text));
  } catch {
    /* summary is best-effort; the empty file written at startup still stands */
  }
}

// Emit a structured error line, leave a short summary, and exit. `ship` is the
// ship LocalCursorRunner category string (kept verbatim so the two stay legibly
// parallel); `cause` is the underlying SDK error, if any.
function fail(phase, ship, cause, code) {
  emit({
    kind: "error",
    phase,
    error: ship,
    message: cause === undefined ? ship : String(cause),
  });
  writeSummary(`# cursor run failed\n\nphase: ${phase}\nerror: ${ship}\n`);
  process.exit(code);
}

async function dispose(agent) {
  try {
    if (agent && typeof agent[Symbol.asyncDispose] === "function") {
      await agent[Symbol.asyncDispose]();
    }
  } catch {
    /* swallow secondary dispose errors */
  }
}

async function main() {
  mkdirSync(OUT, { recursive: true });
  // The substrate sets events_path/summary_path unconditionally for cursor runs,
  // so both files must exist even if we fail before the agent produces anything.
  writeFileSync(EVENTS, "");
  writeFileSync(SUMMARY, "");

  const apiKey = process.env.CURSOR_API_KEY;
  if (!apiKey) {
    return fail("api_key", "CURSOR_API_KEY environment variable is not set", undefined, 2);
  }

  let meta;
  try {
    meta = JSON.parse(readFileSync(`${IN}/meta.json`, "utf8"));
  } catch (err) {
    return fail("input", `failed to read ${IN}/meta.json`, err, 2);
  }
  let taskMd;
  try {
    taskMd = readFileSync(`${IN}/task.md`, "utf8");
  } catch (err) {
    return fail("input", `failed to read ${IN}/task.md`, err, 2);
  }

  let Agent;
  try {
    ({ Agent } = await import("@cursor/sdk"));
  } catch (err) {
    return fail("sdk_load", "failed to load @cursor/sdk", err, 2);
  }

  let agent;
  try {
    agent = await Agent.create({
      apiKey,
      model: {
        id: meta.model_id,
        ...(meta.model_params ? { params: meta.model_params } : {}),
      },
      local: { cwd: "/workspace/repo", settingSources: ["project"] },
      ...(meta.agent_name ? { name: meta.agent_name } : {}),
    });
  } catch (err) {
    return fail("agent_create", "Agent.create failed", err, 2);
  }

  let run;
  try {
    run = await agent.send(taskMd);
  } catch (err) {
    await dispose(agent);
    return fail("send", "agent.send failed after Agent.create", err, 2);
  }

  // Stream events as they arrive. A stream error doesn't immediately fail: a
  // terminal RunResult from wait() (below) is preferred if one exists.
  let streamErr;
  try {
    for await (const ev of run.stream()) {
      emit({ kind: ev && ev.type ? ev.type : "event", event: ev });
    }
  } catch (err) {
    streamErr = err;
  }

  let result;
  try {
    result = await run.wait();
  } catch (err) {
    await dispose(agent);
    if (streamErr) {
      // Both the stream and wait() failed; surface both causes so the event
      // line isn't misleadingly attributed to the stream alone.
      return fail("stream", "stream errored without a terminal RunResult", `${streamErr}; wait() also rejected: ${err}`, 2);
    }
    return fail("wait", "run.wait() rejected after a clean stream", err, 2);
  }

  await dispose(agent);

  // Terminal RunResult: map status to summary + events + exit code. A post-run
  // "error" status is a failure, not a thrown error (matches ship).
  const status = result && result.status;
  if (status === "finished") {
    writeSummary(result.result ?? "");
    emit({ kind: "result", status: "succeeded" });
    process.exit(0);
  }
  if (status === "cancelled") {
    writeSummary(result && result.result != null ? result.result : "");
    emit({ kind: "result", status: "cancelled" });
    process.exit(1);
  }
  const message =
    result && result.result != null
      ? result.result
      : "Cursor SDK reported error without a message";
  writeSummary(String(message));
  emit({ kind: "result", status: "failed", message: String(message) });
  process.exit(1);
}

main().catch((err) => {
  try {
    emit({
      kind: "error",
      phase: "unexpected",
      error: "unhandled runner error",
      message: String(err),
    });
  } catch {
    /* already exiting */
  }
  process.exit(2);
});
CURSOR_RUNNER_JS

chmod 0644 "$DEST/cursor-runner.js"
chown -R "${GUEST_USER}:${GUEST_USER}" "$DEST"

echo "[install-cursor] sshd AcceptEnv CURSOR_API_KEY + GH_TOKEN"
SSHD=/etc/ssh/sshd_config
if ! grep -qE '^AcceptEnv[[:space:]].*\bCURSOR_API_KEY\b' "$SSHD"; then
    printf 'AcceptEnv CURSOR_API_KEY\n' >> "$SSHD"
fi
if ! grep -qE '^AcceptEnv[[:space:]].*\bGH_TOKEN\b' "$SSHD"; then
    printf 'AcceptEnv GH_TOKEN\n' >> "$SSHD"
fi

echo "[install-cursor] smoke: node --version + cursor-runner.js syntax"
NODE_OUT="$(node --version 2>&1)" || { echo "node --version failed: $NODE_OUT" >&2; exit 1; }
case "$NODE_OUT" in
  *"symbol not found"*|*"Error relocating"*)
    echo "glibc symbol leaked into the musl node binary: $NODE_OUT" >&2; exit 1 ;;
esac
node --check "$DEST/cursor-runner.js" || { echo "cursor-runner.js failed node --check" >&2; exit 1; }
echo "[install-cursor] node ok: $NODE_OUT; cursor-runner.js syntax ok"
