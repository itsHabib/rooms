"""Rooms #123 snapshot+toolstore measurement harness (runs inside rooms-host as root).

Usage (from the Mac):
  limactl shell rooms-host sudo env HOME=/srv/s123 python3 - <phase> [args] < remote.py

HOME is the Rooms state base. It must stay short: the jailed API socket's host
path (HOME/.local/state/rooms/jailer/firecracker/<id>/root/api.sock) has to fit
the 107-byte Unix socket limit.

Every phase writes argv, exit code, monotonic wall time, the full host log and the
collected guest output under LAB/<run>/, and prints one JSON summary line.
"""
import base64
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

READY = Path('/home/rooms')
LAB = READY / 'lab/experiments'
BIN = READY / 'src/target/release/rooms'
IMAGE = READY / 'rooms/images/agent.ext4'
STORE = READY / 'lab/python'
FAKE_STORE = LAB / 'fake-store'
PATCH = LAB / 'author.patch'
BASE = '92a706a7982a527ade967e43b68afd4bc1e5d667'
REPO = 'https://github.com/itsHabib/workbench'
PATCH_SHA256 = '83da8be43c9032f1e44bceeae7a3af135ffe7934ec722963c6d99031feac6c87'
TOOLSTORE_SHA256 = '85074bedee9b2c34014488b422c0869c01571dbb121da5191b2e719863e760d3'
BINARY_SHA256 = '5c5a9970c07589f4df9f77b1c0be4e176363211355469b256de3593f13588e1e'
TESTS = "PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s cmd/fleet/examples/headless/task -p 'test_*.py'"

# The neutral warm step: credential-free, offline, scrubbed PATH. It names the
# toolstore explicitly, faults the interpreter/stdlib and repository metadata into
# guest memory, and proves the pinned checkout stays clean.
WARM = f"""set -eu
cd /workspace/repo
test "$(git rev-parse HEAD)" = {BASE}
export PATH=/nix/var/rooms/env/bin:/usr/local/bin:/usr/bin:/bin
export PYTHONDONTWRITEBYTECODE=1
python3 -c 'import argparse, copy, json, pathlib, re, subprocess, sys, tempfile, unittest; unittest.TestLoader()'
python3 -m unittest --help >/dev/null
git status --porcelain >/dev/null
test -z "$(git status --porcelain)"
"""


def digest(path):
    with Path(path).open('rb') as handle:
        return hashlib.file_digest(handle, 'sha256').hexdigest()


def patch_b64():
    return base64.b64encode(PATCH.read_bytes()).decode()


def cold_command():
    # Byte-for-byte the cold baseline's command (PATH comes from the cold sshd SetEnv).
    return ("set -eu; printf '%s' " + patch_b64() + " | base64 -d > /tmp/author.patch; "
            "git apply --check /tmp/author.patch; git apply /tmp/author.patch; " + TESTS)


def restored_command():
    # Restore runs a literal command: the task itself pins the base, checks
    # isolation, applies the frozen patch, runs the tests with the toolstore on an
    # explicit PATH, and exports its patch. Test exit stays distinct from export.
    return f"""set -eu
S=/home/rooms/.rooms-sentinel
if [ -e "$S" ]; then echo "isolation: foreign sentinel $(cat "$S")" >&2; exit 97; fi
printf '%s %s\\n' "$(hostname)" "$(cat /proc/sys/kernel/random/uuid)" > "$S"
{{ echo "hostname=$(hostname)"; echo "sentinel=$(cat "$S")"; ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub 2>/dev/null | sed 's/^/hostkey=/'; }} > /workspace/out/identity.txt
cd /workspace/repo
test "$(git rev-parse HEAD)" = {BASE}
test -z "$(git status --porcelain)"
printf '%s' {patch_b64()} | base64 -d > /tmp/author.patch
git apply --check /tmp/author.patch
git apply /tmp/author.patch
export PATH=/nix/var/rooms/env/bin:$PATH
status=0
{TESTS} || status=$?
git add -A
git diff --binary --cached {BASE} > /workspace/out/result.patch
exit "$status"
"""


def host_state():
    """Every residue a run could leave: processes, mounts, namespaces, taps, rooms."""
    def sh(cmd):
        return subprocess.run(cmd, shell=True, capture_output=True, text=True).stdout.strip()
    ls = subprocess.run([str(BIN), 'ls', '--json'], capture_output=True, text=True)
    return {
        'firecracker_processes': sh("pgrep -a firecracker || true"),
        'jailer_mounts': sh("findmnt -rn -o TARGET | grep -E 'jailer|firecracker' || true"),
        'netns': sh("ip netns list || true"),
        'taps': sh("ip -o link show | grep -oE 'tap-fc[0-9]+|rv[a-z0-9-]+' || true"),
        'rooms_ls': ls.stdout.strip(),
        'restore_intents': sh(f"ls {os.environ['HOME']}/.local/state/rooms/restore-intents 2>/dev/null || true"),
        'snapshot_intents': sh(f"ls {os.environ['HOME']}/.local/state/rooms/snapshot-intents 2>/dev/null || true"),
        'meminfo': sh("grep -E 'MemAvailable|Shmem:|ShmemHugePages|FileHugePages' /proc/meminfo"),
        'thp_vmstat': sh("grep -E '^thp_file_mapped|^thp_split_pmd|^thp_file_alloc' /proc/vmstat"),
    }


def run(name, argv, timeout=420):
    out = LAB / name
    out.mkdir(parents=True, exist_ok=False)
    (out / 'argv.json').write_text(json.dumps(argv, indent=2))
    before = host_state()
    started = time.monotonic()
    started_wall = time.time()
    with (out / 'host.log').open('w') as log:
        process = subprocess.run(argv, stdout=subprocess.PIPE, stderr=log, text=True, timeout=timeout)
    elapsed = time.monotonic() - started
    (out / 'stdout.json').write_text(process.stdout)
    record = dict(run=name, cli_exit=process.returncode, elapsed_seconds=round(elapsed, 3),
                  started_unix=round(started_wall, 3), stdout=process.stdout.strip()[-4000:],
                  host_before=before, host_after=host_state())
    (out / 'summary.json').write_text(json.dumps(record, indent=2))
    return out, record


def collected(out_dir):
    """Parse one collected /workspace/out: result, patch identity, guest identity."""
    out_dir = Path(out_dir)
    result = None
    if (out_dir / 'result.json').exists():
        result = json.loads((out_dir / 'result.json').read_text())
        result.pop('command', None)  # the patch-bearing command stays in argv.json
    patch = out_dir / 'result.patch'
    return dict(
        result=result,
        patch_sha256=digest(patch) if patch.exists() else None,
        patch_matches_input=patch.exists() and digest(patch) == PATCH_SHA256,
        stderr_tail=(out_dir / 'logs/stderr.log').read_text()[-600:] if (out_dir / 'logs/stderr.log').exists() else None,
        identity=(out_dir / 'identity.txt').read_text() if (out_dir / 'identity.txt').exists() else None,
    )


def emit(record):
    print(json.dumps(record, indent=2))


def phase_setup():
    LAB.mkdir(parents=True, exist_ok=True)
    PATCH.write_bytes(Path('/home/rooms/author.patch').read_bytes())
    assert digest(PATCH) == PATCH_SHA256
    identities = {str(p): digest(p) for p in [BIN, IMAGE, STORE / 'toolstore.sqfs', STORE / 'meta.json', PATCH]}
    info = dict(identities=identities, uname=os.uname().release,
                cpus=os.cpu_count(), warm=WARM, host=host_state())
    (LAB / 'inputs.json').write_text(json.dumps(info, indent=2))
    emit(info)


def phase_cold(name):
    out, record = run(name, [
        str(BIN), 'run', '--image', str(IMAGE), '--toolstore', str(STORE),
        '--cpus', '2', '--memory', '1024', '--disk', '1',
        '--repo', REPO, '--base-sha', BASE, '--command', cold_command(),
        '--max-wall', '180s', '--out', str(LAB / name / 'out'),
        '--lifecycle', str(LAB / name / 'lifecycle.ndjson'), '--json'])
    record['collected'] = collected(out / 'out')
    events = [json.loads(line) for line in (out / 'lifecycle.ndjson').read_text().splitlines()]
    record['events'] = [(e['event'], e['ts']) for e in events]
    (out / 'summary.json').write_text(json.dumps(record, indent=2))
    emit(record)


def phase_base(name, snapshot_out=None):
    out, record = run(name, [
        str(BIN), 'base-create', '--image', str(IMAGE), '--toolstore', str(STORE),
        '--cpus', '2', '--memory', '1024', '--repo', REPO, '--base-sha', BASE,
        '--warm', WARM, '--json'])
    base = json.loads(record['stdout'].splitlines()[-1]) if record['cli_exit'] == 0 else None
    record['base'] = base
    if base:
        room_json = Path(os.environ['HOME']) / '.local/state/rooms' / base['room_id'] / 'room.json'
        record['base_room_json'] = json.loads(room_json.read_text())
        argv = [str(BIN), 'snapshot', base['room_id'], '--json']
        if snapshot_out:
            argv[3:3] = ['--out', snapshot_out]
        snap_out, snap = run(name + '-snapshot', argv)
        record['snapshot_run'] = {k: snap[k] for k in ('cli_exit', 'elapsed_seconds', 'stdout')}
        if snap['cli_exit'] == 0:
            result = json.loads(snap['stdout'].splitlines()[-1])
            meta = json.loads((Path(result['directory']) / 'snapshot.json').read_text())
            record['snapshot'] = dict(result=result, meta=meta, files={
                f: os.stat(Path(result['directory']) / f).st_size
                for f in ('snapshot.json', 'snapshot.vmstate', 'snapshot.mem')})
    (out / 'summary.json').write_text(json.dumps(record, indent=2))
    emit(record)


def phase_restore(name, snapshot_dir, *extra):
    argv = [str(BIN), 'restore', snapshot_dir, '--image', str(IMAGE), *extra,
            '--command', restored_command(), '--max-wall', '300s',
            '--out', str(LAB / name / 'out'), '--json']
    out, record = run(name, argv)
    record['collected'] = collected(out / 'out')
    (out / 'summary.json').write_text(json.dumps(record, indent=2))
    emit(record)


def phase_refuse(name, snapshot_dir, *extra):
    """A restore that must be refused before admission, leaving no residue."""
    argv = [str(BIN), 'restore', snapshot_dir, '--image', str(IMAGE), *extra,
            '--command', 'true', '--json']
    out, record = run(name, argv)
    record['refused'] = record['cli_exit'] != 0
    residue = ('firecracker_processes', 'jailer_mounts', 'netns', 'taps', 'rooms_ls',
               'restore_intents', 'snapshot_intents')
    record['residue_unchanged'] = all(record['host_before'][k] == record['host_after'][k] for k in residue)
    (out / 'summary.json').write_text(json.dumps(record, indent=2))
    emit({k: record[k] for k in ('run', 'cli_exit', 'elapsed_seconds', 'stdout', 'refused', 'residue_unchanged')})


def phase_clone(name, snapshot_dir, count):
    argv = [str(BIN), 'clone', snapshot_dir, '--image', str(IMAGE), '--toolstore', str(STORE),
            '-n', count, '--command', restored_command(), '--max-wall', '300s',
            '--out', str(LAB / name / 'out'), '--json']
    out, record = run(name, argv)
    record['collected'] = {p.name: collected(p) for p in sorted((out / 'out').iterdir()) if p.is_dir()}
    (out / 'summary.json').write_text(json.dumps(record, indent=2))
    emit(record)


def phase_thp_mount():
    # The p2-readiness-profile mitigation: snapshot memory on a huge=always
    # tmpfs so read faults map 2 MiB. chattr +i works on tmpfs since 6.0.
    target = LAB / 'thp'
    target.mkdir(exist_ok=True)
    subprocess.run(['mount', '-t', 'tmpfs', '-o', 'size=1600m,huge=always,mode=0700',
                    'tmpfs', str(target)], check=True)
    emit(dict(mount=subprocess.run(['findmnt', str(target)], capture_output=True, text=True).stdout))


def phase_audit():
    identities = json.loads((LAB / 'inputs.json').read_text())['identities']
    emit(dict(inputs_unchanged={p: digest(p) == h for p, h in identities.items()}, host=host_state()))


if __name__ == '__main__':
    phase, *args = sys.argv[1:]
    globals()['phase_' + phase](*args)
