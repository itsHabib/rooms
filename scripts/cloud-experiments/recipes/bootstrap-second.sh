#!/bin/bash
set -euo pipefail
cd "$HOME/src"
mkdir -p "$HOME/lab"
exec > >(tee "$HOME/lab/bootstrap.log") 2>&1
export DEBIAN_FRONTEND=noninteractive
bash scripts/setup-rooms-host.sh
bash scripts/setup-nix-host.sh
sudo apt-get install -y squashfs-tools musl-tools time sysstat
mkdir -p "$HOME/.ssh"
if [[ ! -f "$HOME/.ssh/id_rooms" ]]; then ssh-keygen -q -t ed25519 -N '' -f "$HOME/.ssh/id_rooms"; fi
sudo bash scripts/setup-tap.sh --host
source "$HOME/.cargo/env"
git apply scripts/cloud-experiments/density-lab.patch
cargo fmt
cargo test --locked -j24 > "$HOME/lab/rust-tests.log" 2>&1
cargo build --release --locked -j24
sudo bash scripts/build-rootfs-alpine.sh --out "$HOME/rooms/images/agent.ext4" --ssh-key "$HOME/.ssh/id_rooms.pub"
sudo -u "$USER" python3 scripts/build-toolstore.py --preset python --out "$HOME/lab/python"
sha256sum target/release/rooms "$HOME/rooms/images/agent.ext4" "$HOME/rooms/images/vmlinux.bin" "$HOME/lab/python/toolstore.sqfs" > "$HOME/lab/hashes.txt"
sudo mkdir -p /root/.ssh
sudo cp "$HOME/.ssh/id_rooms" /root/.ssh/id_rooms
sudo chmod 600 /root/.ssh/id_rooms
sudo target/release/rooms doctor --image "$HOME/rooms/images/agent.ext4" --json > "$HOME/lab/doctor.json"
touch "$HOME/lab/bootstrap.done"
