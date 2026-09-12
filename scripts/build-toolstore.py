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
    args.out.parent.mkdir(parents=True, exist_ok=True)
    if args.out.exists() or args.out.is_symlink():
        parser.error('output already exists; use a fresh directory')
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
                        lock_sha256=digest(frozen / 'flake.lock'))
        (publish / 'meta.json').write_text(json.dumps(manifest, indent=2) + '\n')
        shutil.copyfile(frozen / 'flake.nix', publish / 'flake.nix')
        shutil.copyfile(frozen / 'flake.lock', publish / 'flake.lock')
        for path in publish.iterdir():
            with path.open('rb') as artifact:
                os.fsync(artifact.fileno())
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
    raise SystemExit(143)


if __name__ == '__main__':
    signal.signal(signal.SIGTERM, terminate)
    main()
