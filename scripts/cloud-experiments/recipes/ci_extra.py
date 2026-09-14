import json,shlex
from pathlib import Path
import remote as r
r.LAB=r.READY/'lab/ci-extra';r.LAB.mkdir(exist_ok=False)
r.STORE=r.READY/'lab/go-ci-extra';r.IMAGE=r.READY/'lab/image-warm-fixed.ext4';r.BASE='1c0ba652dfc62ae4687d431581d96ece59b314cb'
env='export PATH=/nix/var/rooms/env/bin:/usr/local/bin:/usr/bin:/bin; export GOMODCACHE=/nix/var/rooms/env/share/gomodcache; export GOCACHE=/tmp/go-build; export GOPROXY=off GOSUMDB=off GOTOOLCHAIN=local; '
warm='set -eu; '+env+'cd /workspace/repo; test "$(git rev-parse HEAD)" = '+r.BASE+'; go version; codex --version; python3 --version; go test -race -run \'^$\' -count=1 ./...; test -z "$(git status --porcelain)"'
out,rec=r.run('prepare',[str(r.BIN),'base-create','--image',str(r.IMAGE),'--toolstore',str(r.STORE),'--cpus','2','--memory','4096','--repo',r.REPO,'--base-sha',r.BASE,'--warm',warm,'--json'],timeout=900)
assert rec['cli_exit']==0,rec
base=json.loads((out/'stdout.json').read_text())
sout,srec=r.run('snapshot',[str(r.BIN),'snapshot',base['room_id'],'--json'],timeout=300);assert srec['cli_exit']==0
snapshot=json.loads((sout/'stdout.json').read_text())['directory']
tests=[('fleet-reference','bash cmd/fleet/testdata/run-suite.sh'),('fleet-codex-adapter','bash cmd/fleet/testdata/run-suite.sh codex'),('corpus-build-vet','go build ./... && go vet ./... && go run ./cmd/tracelens eval ./cmd/tracelens/testdata/corpus')]
for pkg,fn in [('contracts/driverstate','FuzzDecodeEvent'),('contracts/driverstate','FuzzReadLedger'),('driverstate','FuzzDecodeLedger'),('driverstate','FuzzTornTailHeal')]:
 tests.append((fn,'go test ./'+pkg+" -run '^$' -fuzz '"+fn+"$' -fuzztime 500000x"))
cases=[{'id':name,'command':'set -eu; '+env+'cd /workspace/repo; test "$(git rev-parse HEAD)" = '+r.BASE+'; '+command} for name,command in tests]
manifest=r.LAB/'cases.json';manifest.write_text(json.dumps({'schema':'rooms.matrix.v1','cases':cases},indent=2))
name='workflow-tests';out,rec=r.run(name,[str(r.BIN),'matrix',snapshot,'--image',str(r.IMAGE),'--toolstore',str(r.STORE),'--cases',str(manifest),'--egress','none','--max-wall','900s','--out',str(r.LAB/name/'out'),'--json'],timeout=1200)
results=[json.loads(p.read_text()) for p in (out/'out').glob('*/result.json')]
row={'base':r.BASE,'cli_exit':rec['cli_exit'],'elapsed_seconds':rec['elapsed_seconds'],'receipts':len(results),'successful':sum(x['exit_code']==0 for x in results),'host_after':r.host_state()}
(out/'ci-result.json').write_text(json.dumps(row,indent=2));print('CI_EXTRA_COMPLETE',json.dumps(row),flush=True)
