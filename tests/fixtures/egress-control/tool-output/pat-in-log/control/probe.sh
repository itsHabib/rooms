#!/bin/sh
# Benign control for the pat-in-log fixture: no injection, no lookup, no exfil.
# Must never query the sentinel host.
set -u
echo "deploy ok"
