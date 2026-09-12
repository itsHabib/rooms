#!/usr/bin/env python3
"""Build a pinned local flake output into a sealed, portable toolchain disk.

Run as a normal Linux user with Nix, squashfs-tools and passwordless sudo for
chattr. Nix evaluation/builds never run as root. Existing outputs are refused.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import sys
import subprocess
import tempfile
from urllib.parse import urlsplit


def run(argv, **kwargs):
    try:
        return subprocess.run(argv, check=True, text=True, **kwargs)
    except subprocess.CalledProcessError as error:
        if error.stderr:
            print(error.stderr[-16000:], file=sys.stderr)
        raise


def digest(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def reject_local_input(declaration):
    url = declaration.get('url', '')
    scheme = urlsplit(url).scheme.lower().rsplit('+', 1)[-1]
    if declaration.get('type') == 'path' or scheme == 'file' or url.startswith('/'):
        raise ValueError('local flake inputs are not supported; keep local modules inside the root flake')


def validate_locked_inputs(lock):
    for node in json.loads(lock.read_text())['nodes'].values():
        reject_local_input(node.get('locked', {}))
        reject_local_input(node.get('original', {}))


def write_manifest(path, manifest):
    data = (json.dumps(manifest, indent=2) + '\n').encode('utf-8')
    if len(data) > 1_048_576:
        raise ValueError('toolstore manifest exceeds 1 MiB')
    path.write_bytes(data)


def sync_directory(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def store_path(value):
    path = Path(value)
    if path.parent != Path('/nix/store') or not (path.exists() or path.is_symlink()):
        raise ValueError(f'not a direct Nix store object: {value}')
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--flake', type=Path, default=Path(__file__).resolve().parents[1] / 'presets')
    parser.add_argument('--preset', default='rust', choices=['rust', 'go', 'node', 'python', 'polyglot'])
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    if os.geteuid() == 0:
        parser.error('run as a normal user; only final inode sealing uses sudo')
    if platform.system() != 'Linux':
        parser.error('build on the Linux Rooms host, not the Mac')
    flake = args.flake.resolve(strict=True)
    lock = flake / 'flake.lock'
    if not lock.is_file():
        parser.error('flake.lock is required; lock the inputs before building')
    args.out = args.out.absolute()
    if args.out.exists() or args.out.is_symlink():
        parser.error('output already exists; use a fresh directory')
    if args.out.resolve().is_relative_to(flake):
        parser.error('output must be outside the flake directory')
    args.out.parent.mkdir(parents=True, exist_ok=True)
    # mkdir is the reservation; publication is one sibling-directory rename.
    reservation = args.out.with_name(args.out.name + '.building')
    reservation.mkdir()
    try:
        build(args, flake)
    finally:
        shutil.rmtree(reservation)


def build(args, flake):
    with tempfile.TemporaryDirectory(prefix='.toolstore-', dir=args.out.parent) as temporary:
        work = Path(temporary)
        # Freeze the local declaration so concurrent edits cannot split metadata
        # from the expression Nix evaluated. Store dependencies are pinned by lock.
        frozen = work / 'flake'
        shutil.copytree(flake, frozen, symlinks=True)
        for path in frozen.rglob('*'):
            if path.is_symlink():
                raise ValueError(f'symlinked local flake input is not supported: {path.relative_to(frozen)}')
        validate_locked_inputs(frozen / 'flake.lock')
        nix = ['nix', '--extra-experimental-features', 'nix-command flakes']
        output = run(nix + ['build', '--no-update-lock-file', '--no-write-lock-file',
                           '--json', '--out-link', str(work / 'result'),
                           f'path:{frozen}#{args.preset}'], capture_output=True)
        results = json.loads(output.stdout)
        if len(results) != 1 or set(results[0]['outputs']) != {'out'}:
            raise ValueError('expected exactly one buildEnv output named out')
        environment = store_path(results[0]['outputs']['out'])
        closure = sorted(run(['nix-store', '-qR', str(environment)], capture_output=True).stdout.splitlines())
        stage = work / 'nix'
        (stage / 'store').mkdir(parents=True)
        for value in closure:
            source = store_path(value)
            # Preserve internal absolute Nix symlinks. Never dereference them.
            run(['cp', '-a', '--reflink=auto', '--', str(source), str(stage / 'store')])
        (stage / 'var/rooms').mkdir(parents=True)
        (stage / 'var/rooms/env').symlink_to(environment)
        publish = work / 'publish'
        publish.mkdir()
        image = publish / 'toolstore.sqfs'
        run(['mksquashfs', str(stage), str(image), '-noappend', '-all-root',
             '-all-time', '1', '-mkfs-time', '1', '-no-xattrs', '-comp', 'zstd',
             '-processors', '2', '-no-progress', '-exit-on-error'], stdout=subprocess.DEVNULL)
        image.chmod(0o444)
        manifest = dict(schema_version=1, system=platform.machine() + '-linux',
                        preset=args.preset, environment=str(environment), closure=closure,
                        sha256=digest(image), flake_sha256=digest(frozen / 'flake.nix'),
                        lock_sha256=digest(frozen / 'flake.lock'),
                        source_files_sha256={str(path.relative_to(frozen)): digest(path)
                                             for path in sorted(frozen.rglob('*')) if path.is_file()})
        write_manifest(publish / 'meta.json', manifest)
        shutil.copytree(frozen, publish / 'flake')
        for directory, _, files in os.walk(publish, topdown=False):
            for name in files:
                with (Path(directory) / name).open('rb') as artifact:
                    os.fsync(artifact.fileno())
            sync_directory(Path(directory))
        try:
            run(['sudo', '-n', 'chattr', '+i', '--', str(image)])
            with image.open('rb') as sealed:
                os.fsync(sealed.fileno())
            sync_directory(publish)
            # Our reservation prevents concurrent cooperative builders; never
            # replace even an empty output directory created by another caller.
            if args.out.exists() or args.out.is_symlink():
                raise FileExistsError(args.out)
            run(['mv', '--no-clobber', '-T', '--', str(publish), str(args.out)])
            if publish.exists():
                raise FileExistsError(args.out)
        except BaseException:
            if image.exists():
                run(['sudo', '-n', 'chattr', '-i', '--', str(image)])
            raise
        sync_directory(args.out.parent)
        print(json.dumps(dict(directory=str(args.out), **manifest)))


def terminate(_signum, _frame):
    # subprocess.run catches BaseException (including SystemExit), calls
    # process.kill(), then Popen.__exit__ waits before staging unwinds.
    # See Lib/subprocess.py:run in CPython; verified with a live Nix derivation.
    raise SystemExit(143)


if __name__ == '__main__':
    signal.signal(signal.SIGTERM, terminate)
    main()
