"""Read-only virtio-blk host-cache fallback from experiment 2, at 16/32/64 guests."""
import json
import subprocess
import threading
import time
from pathlib import Path
import remote as r
r.LAB = r.READY / "lab/density-v3"
snapshot=json.loads((r.LAB/'base/summary.json').read_text())['snapshot']['result']['directory']
command='''set -eu
export PATH=/nix/var/rooms/env/bin:$PATH
python3 - <<'GUEST'
import hashlib,json,pathlib,time
before=pathlib.Path('/proc/meminfo').read_text()
total=0
h=hashlib.sha256()
for p in sorted(pathlib.Path('/nix/store').rglob('*')):
    if p.is_symlink() or not p.is_file(): continue
    with p.open('rb') as f:
        while b:=f.read(1024*1024): h.update(b); total+=len(b)
pathlib.Path('/workspace/out/cache.json').write_text(json.dumps({'bytes_read':total,'digest':h.hexdigest(),'before':before,'after':pathlib.Path('/proc/meminfo').read_text()}))
time.sleep(10)
GUEST
'''
rows=[]
for n in [16,32,64]:
    name=f'shared-store-{n}'
    samples=[]
    stop=threading.Event()
    def sample():
        while not stop.is_set():
            pss=[]
            for p in Path('/proc').glob('[0-9]*'):
                try:
                    if p.joinpath('comm').read_text().strip()!='firecracker':continue
                    pss.append(sum(int(v.split()[1]) for v in p.joinpath('smaps_rollup').read_text().splitlines() if v.startswith('Pss:')))
                except OSError:continue
            cache=subprocess.run(['fincore','--json',str(r.STORE/'toolstore.sqfs')],capture_output=True,text=True)
            samples.append({'unix':time.time(),'vmm_pss_kib':pss,'host_meminfo':Path('/proc/meminfo').read_text(),'fincore_exit':cache.returncode,'fincore':cache.stdout})
            stop.wait(1)
    t=threading.Thread(target=sample);t.start()
    try:
        out,rec=r.run(name,[str(r.BIN),'clone',snapshot,'--image',str(r.IMAGE),'--toolstore',str(r.STORE),'-n',str(n),'--command',command,'--max-wall','180s','--out',str(r.LAB/name/'out'),'--json'],timeout=900)
    finally:stop.set();t.join()
    (r.LAB/name/'memory.json').write_text(json.dumps(samples))
    receipts=[json.loads(p.read_text()) for p in (out/'out').glob('*/cache.json')]
    row={'n':n,'elapsed_seconds':rec['elapsed_seconds'],'cli_exit':rec['cli_exit'],'receipt_count':len(receipts),'unique_store_digests':sorted(set(x['digest'] for x in receipts)),'host_after':r.host_state()}
    rows.append(row);(r.LAB/'shared-store-results.json').write_text(json.dumps(rows,indent=2))
    print('SHARED_STORE',n,rec['cli_exit'],len(receipts),flush=True)
    if rec['cli_exit'] or len(receipts)!=n:break
