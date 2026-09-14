#!/bin/bash
set -euo pipefail
cd "$HOME/src"
source "$HOME/.cargo/env"
exec > >(tee "$HOME/lab/network-fix.log") 2>&1
cp scripts/setup-tap.sh "$HOME/setup-tap-before-density.sh"
git apply "$HOME/density-network.patch"
mkdir -p examples
cp "$HOME/lab-import-snapshot.rs" examples/
cargo fmt
cargo test --locked -j24 > "$HOME/lab/rust-tests-v3.log" 2>&1
cargo build --release --locked -j24 --bin rooms --example lab-import-snapshot
sudo target/release/rooms gc > "$HOME/lab/network-recovery.log" 2>&1
sudo bash "$HOME/setup-tap-before-density.sh" --host --teardown
sudo bash scripts/setup-tap.sh --host
sudo target/release/rooms doctor --image "$HOME/rooms/images/agent.ext4" --json > "$HOME/lab/doctor-v3.json"
sha256sum target/release/rooms > "$HOME/lab/density-v3-binary.sha256"
touch "$HOME/lab/network-fix.done"
