#!/usr/bin/env python3
"""Exercise direct collection with real tar/chown and a fixture SSH transport.

Run under sudo, passing the Linux lib-test executable built by cargo test --lib --no-run.
"""
import argparse
import os
from pathlib import Path
import shlex
import subprocess
import tarfile
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('test_binary', type=Path)
    args = parser.parse_args()
    if os.geteuid() != 0 or 'SUDO_UID' not in os.environ:
        parser.error('run through sudo from the invoking user')
    test = 'runner::tests::direct_collection_returns_restrictive_output_to_caller'
    listed = subprocess.check_output([str(args.test_binary.resolve()), '--list'], text=True)
    if test + ': test' not in listed:
        parser.error('binary does not contain the direct-collection regression')
    with tempfile.TemporaryDirectory(prefix='rooms-collect-fixture-') as temporary:
        fixture = Path(temporary)
        private = fixture / 'private'
        private.write_text('private artifact\n')
        private.chmod(0o600)
        archive = fixture / 'fixture.tar'
        with tarfile.open(archive, 'w', format=tarfile.USTAR_FORMAT) as output:
            output.add(private, arcname='private')
        ssh = fixture / 'ssh'
        ssh.write_text('#!/bin/sh\nexec /usr/bin/cat ' + shlex.quote(str(archive)) + '\n')
        ssh.chmod(0o755)
        environment = dict(os.environ, PATH=str(fixture) + os.pathsep + os.environ['PATH'],
                           ROOMS_TEST_COLLECT_FIXTURE='1')
        subprocess.run([str(args.test_binary.resolve()),
                        test,
                        '--exact', '--ignored', '--nocapture'], env=environment, check=True)


if __name__ == '__main__':
    main()
