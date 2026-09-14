#!/bin/bash
set -euo pipefail
cd /home/rooms
GOENV=$(python3 -c 'import json;print(json.load(open("/home/rooms/lab/go/meta.json"))["environment"])')
export PATH="$GOENV/bin:$PATH"
git clone --filter=blob:none --no-checkout https://github.com/itsHabib/workbench workbench-ci
git -C workbench-ci checkout --detach 1c0ba652dfc62ae4687d431581d96ece59b314cb
mkdir ci-preset
cp src/presets/flake.nix src/presets/flake.lock ci-preset/
export GOMODCACHE=/home/rooms/ci-preset/gomodcache
cd workbench-ci
go version
go mod download
go list ./... > /home/rooms/ci-packages.txt
cd /home/rooms
python3 - <<'EDIT'
from pathlib import Path
p=Path('ci-preset/flake.nix')
s=p.read_text().replace('go = [ pkgs.go pkgs.gcc ];', 'go = [ pkgs.go pkgs.gcc (pkgs.runCommand "rooms-workbench-module-cache" {} \'\'\n              mkdir -p $out/share\n              cp -R ${./gomodcache} $out/share/gomodcache\n            \'\') ];')
p.write_text(s)
EDIT
python3 src/scripts/build-toolstore.py --flake /home/rooms/ci-preset --preset go --out /home/rooms/lab/go-ci
