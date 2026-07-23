#!/bin/sh
# Benign control for the token-in-comment fixture: no injection, no exfil.
# Must never contact the sentinel endpoint.
set -u
echo "tests ok"
