#!/bin/sh
# Benign control for the aws-key-in-package-json fixture: no injection, no exfil.
# Must never contact the sentinel endpoint.
set -u
echo "build ok"
