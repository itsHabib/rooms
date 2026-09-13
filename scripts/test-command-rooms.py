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


def cancellation_probe(args, program, witness=False, nested=False):
    tools = args.out / (program + ('-witness' if witness else '') + ('-parents' if nested else '') + '-tools')
    tools.mkdir()
    marker = tools / 'started'
    formatter = tools / program.removesuffix('-stall')
    finish = {'getent': 'exec sleep 120', 'mkfs.ext4': 'exec sleep 120', 'chmod': 'exec sleep 120', 'curl': 'exec sleep 120',
              'chown-stall': 'exec sleep 120', 'chown': 'sleep 2; exec /usr/bin/chown "$@"'}[program]
    if nested:
        first = shlex.quote(str(tools / 'first-call'))
        finish = f'if test ! -e {first}; then touch {first}; sleep 8; exec /usr/bin/chown "$@"; fi; exec sleep 120'
    formatter.write_text('#!/bin/sh\necho $$ > ' + shlex.quote(str(marker)) + '\n' + finish + '\n')
    formatter.chmod(0o755)
    out = tools / ('parent/nested/out' if nested else 'out')
    lifecycle = tools / 'cancel.ndjson'
    environment = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ['PATH'])
    with (tools / 'host.log').open('w') as log:
        process = subprocess.Popen([str(args.rooms), 'run', '--image', str(args.image),
                                    '--disk', '1', '--command', 'echo COMPLETED_COMMAND', '--out', str(out),
                                    '--lifecycle', str(lifecycle), *(['--witness'] if witness else [])], env=environment,
                                   stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 30
            while not marker.exists():
                if process.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError(program + ' did not start')
                time.sleep(.05)
            cancelled_at = time.monotonic()
            process.send_signal(signal.SIGTERM)
            assert process.wait(timeout=25) == 143
            if nested:
                assert time.monotonic() - cancelled_at < 20, 'ownership grace stacked'
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
                process.wait(timeout=25)
    formatter_pid = int(marker.read_text())
    deadline = time.monotonic() + 5
    while Path(f'/proc/{formatter_pid}').exists() and time.monotonic() < deadline:
        time.sleep(.05)
    assert not Path(f'/proc/{formatter_pid}').exists(), program + ' survived cancellation'
    history = events(lifecycle)
    booted = any(e['event'] == 'vmm_started' for e in history)
    assert booted == (program in ('chown', 'chown-stall', 'chmod')), history
    if program in ('chown', 'chown-stall', 'chmod'):
        assert history[-1]['event'] == 'cleanup_done'
        result = json.loads((out / 'result.json').read_text())
        assert result['status'] == 'succeeded' and result['exit_code'] == 0
    claim = next(e for e in history if e['event'] == 'slot_allocated')
    state = Path.home() / '.local/state/rooms'
    assert not (state / claim['room_id']).exists()
    assert not (state / 'jailer/firecracker' / claim['room_id']).exists()
    assert not (state / 'slots' / str(claim['slot'])).exists()
    print(tools.name + ': 143, no child/slot/jail residue', flush=True)


def interrupted_patch_export(args, timed_out=False):
    directory = args.out / ('patch-timeout' if timed_out else 'patch-cancel')
    directory.mkdir()
    tools = directory / 'tools'
    tools.mkdir()
    marker = directory / 'export-started'
    shim = tools / 'ssh'
    shim.write_text('#!/bin/sh\ncase "$*" in *"git add -A"*) echo $$ > ' +
                    shlex.quote(str(marker)) + '; exec sleep 120;; esac\nexec /usr/bin/ssh "$@"\n')
    shim.chmod(0o755)
    out = directory / 'out'
    lifecycle = directory / 'lifecycle.ndjson'
    expected = 124 if timed_out else 143
    argv = [str(args.rooms), 'run', '--image', str(args.image), '--disk', '1',
            '--repo', args.repo, '--command', 'echo FINISHED; exit 7',
            '--out', str(out), '--lifecycle', str(lifecycle)]
    if timed_out:
        argv += ['--max-wall', '30s']
    environment = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ['PATH'])
    with (directory / 'host.log').open('w') as log:
        process = subprocess.Popen(argv, env=environment, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 30
            while not marker.exists():
                assert process.poll() is None and time.monotonic() < deadline, 'patch export never started'
                time.sleep(.05)
            if not timed_out:
                process.send_signal(signal.SIGTERM)
            assert process.wait(timeout=50) == expected
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=25)
    result = json.loads((out / 'result.json').read_text())
    assert result['exit_code'] == 7 and result['status'] == 'failed', result
    assert 'patch_path' not in result
    history = events(lifecycle)
    exits = [e for e in history if e['event'] == 'workload_exited']
    assert len(exits) == 1 and exits[0]['exit_code'] == 7, history
    failure = next(e for e in history if e['event'] == 'workload_failed')
    assert exits[0]['seq'] < failure['seq'] and history[-1]['event'] == 'cleanup_done'
    assert 'FINISHED' in (out / 'logs/stdout.log').read_text()
    assert not Path('/proc/' + marker.read_text().strip()).exists(), 'export client survived'
    print(directory.name + ': command exit7 retained, CLI' + str(expected) + ', cleanup complete', flush=True)


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
    script = directory / 'overlay-init'
    script.write_text('#!/bin/sh\n# rooms.scratch=1\n')
    for command in ['mkdir /sbin', f'write {script} /sbin/overlay-init',
                    'set_inode_field /sbin/overlay-init mode 0100644']:
        subprocess.run(['debugfs', '-w', '-R', command, str(image)], check=True,
                       capture_output=True)
    for mode in ['0100644', '040755']:
        subprocess.run(['debugfs', '-w', '-R',
                        f'set_inode_field /sbin/overlay-init mode {mode}', str(image)],
                       check=True, capture_output=True)
        lifecycle = directory / (mode + '.ndjson')
        result = subprocess.run([str(args.rooms), 'run', '--image', str(image),
                                 '--disk', '1', '--command', 'true', '--lifecycle', str(lifecycle)],
                                capture_output=True, timeout=15)
        assert result.returncode == 2 and not events(lifecycle), result
    print('missing init: scratch and repo modes rejected before claim', flush=True)


def idle_output_untouched(args):
    directory = args.out / 'idle-unused'
    directory.mkdir()
    sentinel = directory / 'sentinel'
    sentinel.write_text('untouched')
    before = [(p.stat().st_uid, p.stat().st_gid, p.stat().st_mode) for p in [directory, sentinel]]
    subprocess.run([str(args.rooms), 'run', '--image', str(args.image), '--readonly-rootfs',
                    '--out', str(directory)], check=True, capture_output=True, timeout=30)
    after = [(p.stat().st_uid, p.stat().st_gid, p.stat().st_mode) for p in [directory, sentinel]]
    assert before == after and sentinel.read_text() == 'untouched'
    print('idle unused output preserves ownership and content', flush=True)


def command_output_parents(args):
    out = args.out / 'command-parent' / 'nested-parent' / 'run'
    lifecycle = args.out / 'command-parents.ndjson'
    before = (args.out.stat().st_uid, args.out.stat().st_gid, args.out.stat().st_mode)
    subprocess.run([str(args.rooms), 'run', '--image', str(args.image), '--disk', '1',
                    '--command', 'echo PARENTS_OK', '--out', str(out),
                    '--lifecycle', str(lifecycle)], check=True, capture_output=True, timeout=60)
    uid = int(os.environ.get('SUDO_UID', os.getuid()))
    assert all(p.stat().st_uid == uid for p in [out, out.parent, out.parent.parent, *out.rglob('*')])
    assert before == (args.out.stat().st_uid, args.out.stat().st_gid, args.out.stat().st_mode)
    moved = out.with_name('renamed-by-caller')
    subprocess.run(['sudo', '-u', '#' + str(uid), 'mv', str(out), str(moved)], check=True)
    subprocess.run(['sudo', '-u', '#' + str(uid), 'mv', str(moved), str(out)], check=True)
    assert json.loads((out / 'result.json').read_text())['exit_code'] == 0
    assert events(lifecycle)[-1]['event'] == 'cleanup_done'
    print('nested command output is owned and renameable by caller', flush=True)


def witness_output_ownership(args):
    existing = args.out / 'witness-existing'
    (existing / 'nested').mkdir(parents=True)
    sentinel = existing / 'nested/sentinel'
    sentinel.write_text('untouched')
    unrelated = [existing, existing / 'nested', sentinel]
    before = [(p.stat().st_uid, p.stat().st_gid, p.stat().st_mode) for p in unrelated]
    fresh = args.out / 'witness-parent' / 'nested-parent' / 'witness-fresh'
    for directory in [existing, fresh]:
        subprocess.run([str(args.rooms), 'run', '--image', str(args.image), '--readonly-rootfs',
                        '--witness', '--out', str(directory)], check=True, capture_output=True, timeout=30)
        for name in ['witness.json', 'witness.pcap']:
            assert (directory / name).stat().st_uid == int(os.environ.get('SUDO_UID', os.getuid()))
    after = [(p.stat().st_uid, p.stat().st_gid, p.stat().st_mode) for p in unrelated]
    assert before == after and sentinel.read_text() == 'untouched'
    assert all(p.stat().st_uid == int(os.environ.get('SUDO_UID', os.getuid()))
               for p in [fresh, fresh.parent, fresh.parent.parent])
    print('witness owns generated entries only; unrelated directory preserved', flush=True)


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
    out, result = run_case(args, 'moved-base-ref',
                           'echo retained-base > rooms-proof.txt; git add -A; '
                           'git -c user.name=probe -c user.email=probe@rooms.local commit -qm edit; '
                           'git update-ref refs/rooms/base HEAD', 0, repo)
    assert '+retained-base' in (out / result['patch_path']).read_text()
    out, result = run_case(args, 'unreadable-artifacts',
                           'mkdir -p /workspace/out/private; echo readable > /workspace/out/private/value; '
                           'chmod 000 /workspace/out/private/value /workspace/out/private', 0)
    assert (out / 'private/value').read_text().strip() == 'readable'
    assert (out / 'private/value').stat().st_mode & 0o600 == 0o600
    assert (out / 'private').stat().st_mode & 0o700 == 0o700
    out, result = run_case(args, 'without-guest-sudo',
                           'sudo mv /usr/bin/sudo /usr/bin/sudo.disabled; echo NO_SUDO_OK', 0)
    assert 'NO_SUDO_OK' in (out / 'logs/stdout.log').read_text()
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
    history = events(args.out / 'patch-failed.ndjson')
    exited = next(e for e in history if e['event'] == 'workload_exited')
    failed = next(e for e in history if e['event'] == 'workload_failed')
    assert exited['exit_code'] == 7 and exited['seq'] < failed['seq']
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
    cancellation_probe(args, 'getent')
    cancellation_probe(args, 'mkfs.ext4')
    cancellation_probe(args, 'curl')
    cancellation_probe(args, 'chown')
    cancellation_probe(args, 'chmod')
    cancellation_probe(args, 'chown-stall')
    cancellation_probe(args, 'chown-stall', witness=True)
    cancellation_probe(args, 'chown-stall', witness=True, nested=True)
    interrupted_patch_export(args)
    interrupted_patch_export(args, timed_out=True)
    reject_missing_init(args)
    idle_output_untouched(args)
    witness_output_ownership(args)
    command_output_parents(args)
    assert sha256(args.image) == before, 'shared image changed'
    print('PASS: resources, repository, patch, isolation, timeout, SIGTERM, ownership, collection failure, patch failure, boot/finalization cancellation, image admission, cleanup, image hash')


if __name__ == '__main__':
    main()
