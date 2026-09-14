"""Density ramp for the explicit 128-clone research build; preserve every attempt."""
import datetime
import importlib.util
import json
from pathlib import Path
import statistics
import threading
import time
import remote as r

stop = threading.Event()

def sample():
    with (r.LAB/'density-memory.ndjson').open('x') as log:
        while not stop.is_set():
            samples=[]
            for proc in Path('/proc').glob('[0-9]*'):
                try:
                    if proc.joinpath('comm').read_text().strip() != 'firecracker': continue
                    lines=proc.joinpath('smaps_rollup').read_text().splitlines()
                    mem={v.split(':')[0]:int(v.split()[1]) for v in lines if v.startswith(('Pss:','Rss:'))}
                    samples.append({'pid':int(proc.name),**mem})
                except (OSError,ValueError): continue
            info={k:int(v.split()[0]) for k,v in (line.split(':',1) for line in Path('/proc/meminfo').read_text().splitlines())}
            log.write(json.dumps({'unix':time.time(),'vmm':samples,'host':info})+'\n')
            log.flush()
            stop.wait(.5)


def run():
    r.phase_setup()
    memory=threading.Thread(target=sample); memory.start()
    rows=[]
    try:
        r.phase_base('base')
        base=json.loads((r.LAB/'base/summary.json').read_text())
        snap=base['snapshot']['result']['directory']
        for n in [8,16,32,64,128]:
            for repeat in range(2):
                name=f'density-{n}-{repeat}'
                out, record=r.run(name,[str(r.BIN),'clone',snap,'--image',str(r.IMAGE),
                    '--toolstore',str(r.STORE),'-n',str(n),'--command',r.restored_command(),
                    '--max-wall','180s','--out',str(r.LAB/name/'out'),'--json'],timeout=900)
                outputs=[r.collected(p) for p in sorted((out/'out').iterdir()) if p.is_dir()] if (out/'out').exists() else []
                successes=sum(bool(v['result'] and v['result']['exit_code']==0 and v['patch_matches_input']) for v in outputs)
                row={'name':name,'requested':n,'completed':successes,'cli_exit':record['cli_exit'],
                     'elapsed_seconds':record['elapsed_seconds'],'started_unix':record['started_unix'],
                     'completed_per_minute':60*successes/record['elapsed_seconds'],
                     'identities_unique':len(set(v['identity'] for v in outputs))==len(outputs),
                     'host_after':r.host_state()}
                rows.append(row)
                (r.LAB/'density-results.json').write_text(json.dumps(rows,indent=2))
                print('DENSITY',json.dumps({k:v for k,v in row.items() if k!='host_after'}),flush=True)
                if record['cli_exit'] or successes!=n: return
    finally:
        stop.set(); memory.join()
        (r.LAB/'density-final-host.json').write_text(json.dumps(r.host_state(),indent=2))
        print('DENSITY_FINISHED',flush=True)

run()
