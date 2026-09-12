#!/usr/bin/env python3
"""Real Linux/KVM proof. Run as root through sudo; never touches an existing out dir."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import time


def sha256(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def events(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line]


def run_case(args, name, command, expected, extra=(), terminate=False):
    out = args.out / name
    lifecycle = args.out / (name + '.ndjson')
    argv = [str(args.rooms), 'run', '--image', str(args.image), '--cpus', '2',
            '--memory', '1024', '--disk', '2', '--out', str(out),
            '--lifecycle', str(lifecycle), '--command', command, *extra]
    with (args.out / (name + '.host.log')).open('w') as log:
        process = subprocess.Popen(argv, stdout=log, stderr=subprocess.STDOUT)
        try:
            if terminate:
                deadline = time.monotonic() + 120
                while not any(e['event'] == 'workload_started' for e in events(lifecycle)):
                    if process.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError('workload did not start')
                    time.sleep(.1)
                time.sleep(5)
                process.send_signal(signal.SIGTERM)
            code = process.wait(timeout=180)
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
                process.wait(timeout=45)
    assert code == expected, (name, code, expected)
    result = json.loads((out / 'result.json').read_text())
    assert result['exit_code'] == expected, result
    assert events(lifecycle)[-1]['event'] == 'cleanup_done', events(lifecycle)
    assert not any(e['event'] in ('collection_failed', 'cleanup_failed') for e in events(lifecycle))
    for path in [out, *out.rglob('*')]:
        assert path.stat().st_uid == int(os.environ.get('SUDO_UID', os.getuid())), path
    print(name, result['status'], flush=True)
    return out, result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rooms', type=Path, required=True)
    parser.add_argument('--image', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--repo', default='https://github.com/itsHabib/rooms')
    parser.add_argument('--base-sha', default='e8c4504c5ed441af3c754acb1a456a9aaba621d7')
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=False)
    before = sha256(args.image)
    repo = ['--repo', args.repo, '--base-sha', args.base_sha, '--max-wall', '120s']
    command = '''set -e
[ "$(nproc)" = 2 ]
awk '/MemTotal/ { if ($2 < 900000 || $2 > 1050000) exit 1 }' /proc/meminfo
mount | grep '/dev/vdb on /oldroot/mnt type ext4'
bash -n scripts/lib/overlay-init.sh
dd if=/dev/zero of=/workspace/large.bin bs=1M count=768
printf 'proof edit\n' > rooms-proof.txt
echo DISK_AND_REPO_OK
'''
    out, result = run_case(args, 'repository', command, 0, repo)
    assert result['status'] == 'succeeded'
    assert '+proof edit' in (out / result['patch_path']).read_text()
    assert 'DISK_AND_REPO_OK' in (out / 'logs/stdout.log').read_text()
    out, result = run_case(args, 'fresh-failed',
                           'test ! -e /workspace/large.bin && test ! -e rooms-proof.txt || exit 9; '
                           'echo failed-edit > rooms-proof.txt; exit 7', 7, repo)
    assert result['status'] == 'failed'
    assert '+failed-edit' in (out / result['patch_path']).read_text()
    partial = 'echo PARTIAL_STDOUT; echo PARTIAL_STDERR >&2; sleep 120'
    out, result = run_case(args, 'timeout', partial, 124, ['--max-wall', '30s'])
    assert result['status'] == 'timed_out'
    assert 'PARTIAL_STDOUT' in (out / 'logs/stdout.log').read_text()
    assert 'PARTIAL_STDERR' in (out / 'logs/stderr.log').read_text()
    out, result = run_case(args, 'sigterm', partial, 143, terminate=True)
    assert result['status'] == 'cancelled'
    assert 'PARTIAL_STDOUT' in (out / 'logs/stdout.log').read_text()
    assert 'PARTIAL_STDERR' in (out / 'logs/stderr.log').read_text()
    blocked = args.out / 'not-a-directory'
    blocked.write_text('preserve me')
    lifecycle = args.out / 'collection-failure.ndjson'
    failed = subprocess.run([str(args.rooms), 'run', '--image', str(args.image),
                             '--disk', '1', '--command', 'echo collected',
                             '--out', str(blocked), '--lifecycle', str(lifecycle),
                             '--max-wall', '60s'], capture_output=True, timeout=120)
    assert failed.returncode != 0
    assert blocked.read_text() == 'preserve me'
    assert any(e['event'] == 'collection_failed' for e in events(lifecycle))
    assert events(lifecycle)[-1]['event'] == 'cleanup_done'
    assert sha256(args.image) == before, 'shared image changed'
    print('PASS: resources, repository, patch, isolation, timeout, SIGTERM, ownership, collection failure, cleanup, image hash')


if __name__ == '__main__':
    main()
