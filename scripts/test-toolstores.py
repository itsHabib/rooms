#!/usr/bin/env python3
"""Real Linux/KVM toolstore checks. Run through sudo with a dedicated short HOME."""
import argparse
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time


POLYGLOT = r'''set -eu
while sudo ip route del default 2>/dev/null; do :; done
if ip route get 1.1.1.1; then exit 92; fi
test ! -e /workspace/lab
mkdir /workspace/lab
cd /workspace/lab
cc --version | head -1
go version
rustc --version
cargo --version
python3 --version
node --version
cat > main.c <<'EOF'
#include <stdio.h>
int main(void) { long long s=0; for(long long i=1;i<=10000;i++) s+=i*i; printf("%lld\n",s); }
EOF
cat > main.go <<'EOF'
package main
import "fmt"
func main() { var s int64; for i:=int64(1);i<=10000;i++ {s+=i*i}; fmt.Println(s) }
EOF
cat > main.rs <<'EOF'
fn main() { println!("{}", (1..=10000u64).map(|n| n*n).sum::<u64>()); }
EOF
cc main.c -o c
go build -o g main.go
rustc main.rs -o r
test "$(./c)" = 333383335000
test "$(./g)" = 333383335000
test "$(./r)" = 333383335000
test "$(python3 -c 'print(sum(n*n for n in range(1,10001)))')" = 333383335000
test "$(node -e 'let s=0;for(let n=1;n<=10000;n++)s+=n*n;console.log(s)')" = 333383335000
grep ' /nix squashfs ro' /proc/mounts
if sudo touch /nix/ROOMS_WRITE_MUST_FAIL 2> /workspace/out/readonly-error.txt; then exit 91; fi
test ! -e /nix/ROOMS_WRITE_MUST_FAIL
echo POLYGLOT_OK
'''


def digest(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def capture_console(lifecycle, console_log):
    try:
        lines = lifecycle.read_text().splitlines()
        if not lines:
            return
        room_id = json.loads(lines[0])['room_id']
        source = Path.home() / '.local/state/rooms' / room_id / 'firecracker.log'
        console_log.write_bytes(source.read_bytes())
    except (FileNotFoundError, json.JSONDecodeError):
        # Startup may precede the first event; teardown removes the console.
        return


def wait_with_console(process, lifecycle, console_log):
    deadline = time.monotonic() + 660
    while process.poll() is None:
        if time.monotonic() > deadline:
            raise TimeoutError('room exceeded the outer test deadline')
        capture_console(lifecycle, console_log)
        time.sleep(.5)
    return process.returncode


def run(args, name, command, expected=0, store=None, extra=(), memory=1024):
    lifecycle = args.out / (name + '.ndjson')
    output = args.out / name
    argv = [str(args.rooms), 'run', '--image', str(args.image), '--toolstore',
            str(store or args.toolstore), '--cpus', '2', '--memory', str(memory),
            '--out', str(output), '--lifecycle', str(lifecycle), '--max-wall', '600s',
            '--command', command, *extra]
    started = time.monotonic()
    with (args.out / (name + '.host.log')).open('w') as log:
        process = subprocess.Popen(argv, stdout=log, stderr=subprocess.STDOUT)
        try:
            code = wait_with_console(process, lifecycle, args.out / (name + '.guest.log'))
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=45)
    history = [json.loads(line) for line in lifecycle.read_text().splitlines()] if lifecycle.exists() else []
    assert code == expected, (name, code, expected)
    evidence = dict(case=name, cli_exit=code, seconds=round(time.monotonic()-started, 3))
    if not history:
        assert expected == 2, (name, 'missing lifecycle')
        evidence['refused_before_claim'] = True
        return evidence
    assert history[-1]['event'] == 'cleanup_done', (name, history)
    result = json.loads((output / 'result.json').read_text())
    assert result['exit_code'] == expected, (name, result)
    assert all(p.lstat().st_uid == int(os.environ['SUDO_UID']) for p in [output, *output.rglob('*')])
    attached = next(event for event in history if event['event'] == 'toolstore_attached')
    assert attached['sha256'] == json.loads(((store or args.toolstore) / 'meta.json').read_text())['sha256']
    evidence['room_id'] = attached['room_id']
    evidence['workload_started'] = next(event['ts'] for event in history if event['event'] == 'workload_started')
    evidence['workload_exited'] = next(event['ts'] for event in history if event['event'] == 'workload_exited')
    evidence['status'] = result['status']
    evidence['stdout'] = (output / 'logs/stdout.log').read_text()
    if command == POLYGLOT:
        assert 'POLYGLOT_OK' in evidence['stdout'], (name, 'missing workload proof')
        assert 'Read-only file system' in (output / 'readonly-error.txt').read_text()
    print(name, 'passed', flush=True)
    return evidence


def invalid_inputs(args):
    cases = []
    metadata = json.loads((args.toolstore / 'meta.json').read_text())
    for name, change in [('wrong-arch', {'system':'not-this-machine'}),
                         ('wrong-hash', {'sha256':'0'*64}),
                         ('wrong-version', {'schema_version':999})]:
        directory = args.out / (name + '-input')
        directory.mkdir()
        (directory / 'meta.json').write_text(json.dumps(metadata | change))
        target = directory / 'toolstore.sqfs'
        target.touch()
        subprocess.run(['mount', '--bind', str(args.toolstore / 'toolstore.sqfs'), str(target)], check=True)
        try:
            cases.append(run(args, name, 'true', 2, directory))
        finally:
            subprocess.run(['umount', str(target)], check=True)
    directory = args.out / 'mutable-input'
    directory.mkdir()
    (directory / 'meta.json').write_text(json.dumps(metadata))
    (directory / 'toolstore.sqfs').write_bytes(b'hsqs-not-sealed')
    cases.append(run(args, 'mutable', 'true', 2, directory))
    for case, message in [('wrong-arch', 'architecture mismatch'), ('wrong-hash', 'hash mismatch'),
                          ('wrong-version', 'unsupported toolstore'), ('mutable', 'not kernel-immutable')]:
        assert message in (args.out / (case + '.host.log')).read_text()
    return cases


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rooms', type=Path, required=True)
    parser.add_argument('--image', type=Path, required=True)
    parser.add_argument('--toolstore', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(parents=True)
    before = {str(p):digest(p) for p in [args.image, args.toolstore / 'toolstore.sqfs']}
    evidence = []
    try:
        evidence.append(run(args, 'polyglot-disk', POLYGLOT, extra=['--disk','2']))
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            futures = [pool.submit(run, args, 'shared-' + str(i), POLYGLOT, extra=['--disk','2']) for i in range(2)]
            shared = [f.result() for f in futures]
            assert shared[0]['room_id'] != shared[1]['room_id']
            assert max(case['workload_started'] for case in shared) < min(case['workload_exited'] for case in shared)
            evidence.extend(shared)
        no_scratch = run(args, 'without-scratch', 'set -eu; python3 -c "print(6*7)"; grep " /nix squashfs ro" /proc/mounts')
        assert '42' in no_scratch['stdout'] and 'squashfs ro' in no_scratch['stdout']
        evidence.append(no_scratch)
        failed = run(args, 'failed-command', 'echo RETAINED; exit 7', 7, extra=['--disk','1'])
        assert 'RETAINED' in failed['stdout']
        evidence.append(failed)
        evidence.extend(invalid_inputs(args))
        after = {path:digest(Path(path)) for path in before}
        assert before == after, 'shared backing image changed'
        state = Path.home() / '.local/state/rooms'
        assert not list(state.rglob('room.json')), 'room metadata leaked'
        assert not list(state.rglob('scratch.ext4')), 'scratch leaked'
        assert not list((state / 'jailer/firecracker').glob('*/root/toolstore.sqfs')), 'toolstore mount leaked'
    finally:
        (args.out / 'evidence.json').write_text(json.dumps(dict(inputs=before,cases=evidence),indent=2)+'\n')


if __name__ == '__main__':
    main()
