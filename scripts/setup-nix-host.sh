#!/usr/bin/env bash
# Optional toolstore prerequisites for the Ubuntu Linux Rooms host.
set -euo pipefail

[[ "$(uname -s)" == Linux ]] || { echo 'run this on the Linux Rooms host' >&2; exit 1; }
[[ "$EUID" -ne 0 ]] || { echo 'run as your normal host user, with sudo available' >&2; exit 1; }

sudo apt-get update -qq
sudo apt-get install -y nix-bin squashfs-tools python3
sudo usermod -aG nix-users "$(id -un)"
sudo systemctl enable --now nix-daemon.service
# sudo starts a fresh group context; an already-open SSH session can still
# carry its pre-install supplementary groups until the next login.
sudo -u "$(id -un)" nix --extra-experimental-features 'nix-command flakes' eval --expr 1
echo 'Nix is ready. Open a fresh login (or run newgrp nix-users) before building toolstores.'
