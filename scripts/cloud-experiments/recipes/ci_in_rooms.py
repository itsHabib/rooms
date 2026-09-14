import json,shlex,subprocess,time
from pathlib import Path
import remote as r
r.LAB=r.READY/'lab/ci-runs';r.LAB.mkdir(exist_ok=True)
r.STORE=r.READY/'lab/go-ci';r.BASE='1c0ba652dfc62ae4687d431581d96ece59b314cb'
env="export PATH=/nix/var/rooms/env/bin:/usr/local/bin:/usr/bin:/bin; export GOMODCACHE=/nix/var/rooms/env/share/gomodcache; export GOCACHE=/tmp/go-build; export GOPROXY=off GOSUMDB=off GOTOOLCHAIN=local; "
warm='set -eu; '+env+'cd /workspace/repo; test "$(git rev-parse HEAD)" = '+r.BASE+'; go version; go list ./... > /tmp/packages.txt; go test -race -run \'^$\' -count=1 ./...; test -z "$(git status --porcelain)"'
out,rec=r.run('prepare',[str(r.BIN),'base-create','--image',str(r.IMAGE),'--toolstore',str(r.STORE),'--cpus','2','--memory','4096','--repo',r.REPO,'--base-sha',r.BASE,'--warm',warm,'--json'],timeout=900)
assert rec['cli_exit']==0,rec['stdout']
base=json.loads((out/'stdout.json').read_text())
sout,srec=r.run('snapshot',[str(r.BIN),'snapshot',base['room_id'],'--json'],timeout=300)
assert srec['cli_exit']==0
snapshot=json.loads((sout/'stdout.json').read_text())['directory']
packages=(r.READY/'ci-packages.txt').read_text().splitlines()
assert len(packages)==len(set(packages)) and len(packages)>=32
shards=[packages[i::32] for i in range(32)]
assert sorted(p for shard in shards for p in shard)==sorted(packages)
cases=[]
for i,shard in enumerate(shards):
 command='set -eu; '+env+'cd /workspace/repo; test "$(git rev-parse HEAD)" = '+r.BASE+'; go test -race -count=1 -json -coverprofile=/workspace/out/coverage.out -covermode=atomic '+shlex.join(shard)+' > /workspace/out/test-events.ndjson'
 cases.append({'id':f'shard-{i:02}','command':command})
manifest=r.LAB/'cases.json';manifest.write_text(json.dumps({'schema':'rooms.matrix.v1','cases':cases}))
(r.LAB/'shards.json').write_text(json.dumps({'base':r.BASE,'packages':packages,'shards':shards,'snapshot':snapshot},indent=2))
for trial in range(2):
 name=f'ci-32-{trial}'
 out,rec=r.run(name,[str(r.BIN),'matrix',snapshot,'--image',str(r.IMAGE),'--toolstore',str(r.STORE),'--cases',str(manifest),'--egress','none','--max-wall','600s','--out',str(r.LAB/name/'out'),'--json'],timeout=900)
 results=[json.loads(p.read_text()) for p in (out/'out').glob('*/result.json')]
 row={'trial':trial,'cli_exit':rec['cli_exit'],'elapsed_seconds':rec['elapsed_seconds'],'receipts':len(results),'successful':sum(x['exit_code']==0 for x in results),'host_after':r.host_state()}
 (out/'ci-result.json').write_text(json.dumps(row,indent=2));print('CI_COMPLETE',json.dumps(row),flush=True)
 if rec['cli_exit'] or len(results)!=32:break
