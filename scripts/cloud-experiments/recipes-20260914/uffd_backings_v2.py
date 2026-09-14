import json,shutil,subprocess,time
from pathlib import Path
import remote as r
r.LAB=r.READY/'lab/density-v3'
original=Path(json.loads((r.LAB/'base/summary.json').read_text())['snapshot']['result']['directory'])
variants={'disk':original,'tmpfs-never':r.LAB/'tmpfs-never/snapshot','tmpfs-always':r.LAB/'tmpfs-always/snapshot'}
variants['disk']=r.LAB/'copied-disk'
r.BIN=r.READY/'lab/rooms-uffd-v2'
rows=[]
# Alternate order over three rounds; retain preparation separately from reuse.
for trial,order in enumerate([list(variants),list(reversed(variants)),['tmpfs-never','disk','tmpfs-always']]):
 for kind in order:
  name=f'uffd-v2-{kind}-{trial}'
  out,rec=r.run(name,[str(r.BIN),'restore',str(variants[kind]),'--image',str(r.IMAGE),'--toolstore',str(r.STORE),'--command',r.restored_command(),'--max-wall','90s','--out',str(r.LAB/name/'out'),'--json'],timeout=180)
  row={'backing':kind,'trial':trial,'elapsed_seconds':rec['elapsed_seconds'],'cli_exit':rec['cli_exit'],'collected':r.collected(out/'out'),'host_after':r.host_state()}
  rows.append(row);(r.LAB/'uffd-v2-results.json').write_text(json.dumps(rows,indent=2));print('BACKING',kind,trial,rec['cli_exit'],rec['elapsed_seconds'],flush=True)
  assert rec['cli_exit']==0 and row['collected']['patch_matches_input']
