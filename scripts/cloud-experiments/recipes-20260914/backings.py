import json,shutil,subprocess,time
from pathlib import Path
import remote as r
r.LAB=r.READY/'lab/density-v3'
original=Path(json.loads((r.LAB/'base/summary.json').read_text())['snapshot']['result']['directory'])
variants={'disk':original}
setup=[]
for mode in ['never','always']:
 mount=r.LAB/('tmpfs-'+mode);mount.mkdir(exist_ok=False)
 subprocess.run(['mount','-t','tmpfs','-o','size=1600m,huge='+mode+',mode=0700','tmpfs',str(mount)],check=True)
 snap=mount/'snapshot';snap.mkdir()
 started=time.monotonic()
 for p in original.iterdir():
  if p.name in ['snapshot.json','snapshot.vmstate','snapshot.mem']:
   shutil.copyfile(p,snap/p.name);subprocess.run(['chmod','444',str(snap/p.name)],check=True)
 subprocess.run(['chattr','+i',*[str(p) for p in snap.iterdir()],str(snap)],check=True)
 variants['tmpfs-'+mode]=snap
 setup.append({'mode':mode,'copy_and_seal_seconds':time.monotonic()-started,'mount':subprocess.run(['findmnt','--json',str(mount)],capture_output=True,text=True).stdout})
(r.LAB/'backing-setup.json').write_text(json.dumps(setup,indent=2))
rows=[]
# Alternate order over three rounds; retain preparation separately from reuse.
for trial,order in enumerate([list(variants),list(reversed(variants)),['tmpfs-never','disk','tmpfs-always']]):
 for kind in order:
  name=f'backing-{kind}-{trial}'
  out,rec=r.run(name,[str(r.BIN),'restore',str(variants[kind]),'--image',str(r.IMAGE),'--toolstore',str(r.STORE),'--command',r.restored_command(),'--max-wall','90s','--out',str(r.LAB/name/'out'),'--json'],timeout=180)
  row={'backing':kind,'trial':trial,'elapsed_seconds':rec['elapsed_seconds'],'cli_exit':rec['cli_exit'],'collected':r.collected(out/'out'),'host_after':r.host_state()}
  rows.append(row);(r.LAB/'backing-results.json').write_text(json.dumps(rows,indent=2));print('BACKING',kind,trial,rec['cli_exit'],rec['elapsed_seconds'],flush=True)
  assert rec['cli_exit']==0 and row['collected']['patch_matches_input']
