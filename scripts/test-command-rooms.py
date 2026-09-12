#!/usr/bin/env python3
"""Real Linux/KVM proof. Run as root through sudo; never touches an existing out dir."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import shlex
import subprocess
import time


def sha256(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def events(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line]


def run_case(args, name, command, expected, extra=(), terminate=False, guest_exit=None):
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
    assert result['exit_code'] == (expected if guest_exit is None else guest_exit), result
    assert events(lifecycle)[-1]['event'] == 'cleanup_done', events(lifecycle)
    assert not any(e['event'] in ('collection_failed', 'cleanup_failed') for e in events(lifecycle))
    for path in [out, *out.rglob('*')]:
        assert path.stat().st_uid == int(os.environ.get('SUDO_UID', os.getuid())), path
    print(name, result['status'], flush=True)
    return out, result


def cancellation_probe(args, program):
    tools = args.out / (program + '-tools')
    tools.mkdir()
    marker = tools / 'started'
    formatter = tools / program
    finish = {'mkfs.ext4': 'exec sleep 120', 'curl': 'exec sleep 120',
              'chown': 'sleep 2; exec /usr/bin/chown "$@"'}[program]
    formatter.write_text('#!/bin/sh\necho $$ > ' + shlex.quote(str(marker)) + '\n' + finish + '\n')
    formatter.chmod(0o755)
    lifecycle = args.out / (program + '-cancel.ndjson')
    environment = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ['PATH'])
    with (args.out / (program + '-cancel.host.log')).open('w') as log:
        process = subprocess.Popen([str(args.rooms), 'run', '--image', str(args.image),
                                    '--disk', '1', '--command', 'echo COMPLETED_COMMAND', '--out', str(tools / 'out'),
                                    '--lifecycle', str(lifecycle)], env=environment,
                                   stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 30
            while not marker.exists():
                if process.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError(program + ' did not start')
                time.sleep(.05)
            process.send_signal(signal.SIGTERM)
            assert process.wait(timeout=10) == 143
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
                process.wait(timeout=10)
    formatter_pid = int(marker.read_text())
    deadline = time.monotonic() + 5
    while Path(f'/proc/{formatter_pid}').exists() and time.monotonic() < deadline:
        time.sleep(.05)
    assert not Path(f'/proc/{formatter_pid}').exists(), program + ' survived cancellation'
    history = events(lifecycle)
    booted = any(e['event'] == 'vmm_started' for e in history)
    assert booted == (program == 'chown'), history
    if program == 'chown':
        assert history[-1]['event'] == 'cleanup_done'
        result = json.loads((tools / 'out/result.json').read_text())
        assert result['status'] == 'succeeded' and result['exit_code'] == 0
    claim = next(e for e in history if e['event'] == 'slot_allocated')
    state = Path.home() / '.local/state/rooms'
    assert not (state / claim['room_id']).exists()
    assert not (state / 'jailer/firecracker' / claim['room_id']).exists()
    assert not (state / 'slots' / str(claim['slot'])).exists()
    print(program + '-cancel: 143, no child/slot/jail residue', flush=True)


def reject_missing_init(args):
    directory = args.out / 'missing-init'
    directory.mkdir()
    image = directory / 'rootfs.ext4'
    with image.open('wb') as disk:
        disk.truncate(64 * 1024 * 1024)
    subprocess.run(['mkfs.ext4', '-q', '-F', str(image)], check=True)
    (directory / 'vmlinux.bin').symlink_to(args.image.parent.resolve() / 'vmlinux.bin')
    for name, extra in [('scratch', ['--disk', '1']), ('repo', ['--repo', args.repo])]:
        lifecycle = directory / (name + '.ndjson')
        result = subprocess.run([str(args.rooms), 'run', '--image', str(image),
                                 '--command', 'true', '--json', '--lifecycle', str(lifecycle), *extra],
                                capture_output=True, text=True, timeout=15)
        assert result.returncode == 2, result
        assert not events(lifecycle), 'invalid image claimed a room'
        assert json.loads(result.stdout)['error_kind'] == 'internal'
    print('missing init: scratch and repo modes rejected before claim', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rooms', type=Path, required=True)
    parser.add_argument('--image', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--repo', default='https://github.com/itsHabib/rooms')
    # Pin the pre-feature Rooms tree so repository content is repeatable.
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
    out, result = run_case(args, 'patch-failed',
                           'touch .git/index.lock; echo PATCH_LOCKED; exit 7', 2, repo, guest_exit=7)
    assert result['status'] == 'failed' and 'patch_path' not in result
    assert 'PATCH_LOCKED' in (out / 'logs/stdout.log').read_text()
    partial_out = args.out / 'unsafe-tar'
    rejected = subprocess.run([str(args.rooms), 'run', '--image', str(args.image),
                               '--disk', '1', '--command', 'ln -s /etc/passwd /workspace/out/unsafe',
                               '--out', str(partial_out), '--max-wall', '60s'],
                              capture_output=True, timeout=120)
    assert rejected.returncode != 0
    assert (partial_out / 'changeset.json').exists()
    for path in [partial_out, *partial_out.rglob('*')]:
        assert path.lstat().st_uid == int(os.environ.get('SUDO_UID', os.getuid())), path
    cancellation_probe(args, 'mkfs.ext4')
    cancellation_probe(args, 'curl')
    cancellation_probe(args, 'chown')
    reject_missing_init(args)
    assert sha256(args.image) == before, 'shared image changed'
    print('PASS: resources, repository, patch, isolation, timeout, SIGTERM, ownership, collection failure, patch failure, boot/finalization cancellation, image admission, cleanup, image hash')


if __name__ == '__main__':
    main()
