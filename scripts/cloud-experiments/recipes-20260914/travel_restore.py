import json
from pathlib import Path
import remote as r
r.LAB=r.READY/'lab/travel-runs';r.LAB.mkdir(exist_ok=True)
root=r.READY/'lab/travel';r.BIN=root/'bin/rooms';r.IMAGE=root/'images/agent.ext4';r.STORE=root/'python';r.PATCH=r.READY/'author.patch'
snap=root/'snapshot'
for trial in range(2):
 name=f'resume-{trial}'
 out,rec=r.run(name,[str(r.BIN),'restore',str(snap),'--image',str(r.IMAGE),'--toolstore',str(r.STORE),'--command',r.restored_command(),'--max-wall','90s','--out',str(r.LAB/name/'out'),'--json'])
 result={**rec,'collected':r.collected(out/'out')};(out/'summary.json').write_text(json.dumps(result,indent=2));print(json.dumps(result),flush=True)
 assert rec['cli_exit']==0 and result['collected']['patch_matches_input']
