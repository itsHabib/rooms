import json
from pathlib import Path
import remote as r
r.LAB=r.READY/'lab/chaos';r.LAB.mkdir(exist_ok=True)
root=r.READY/'lab/travel';r.BIN=root/'bin/rooms';r.IMAGE=root/'images/agent.ext4';r.STORE=root/'python';r.PATCH=r.READY/'author.patch'
marker=json.dumps({'attempt':'chaos-c-v1','phase':'patch-applied-before-tests','base':r.BASE})
command=r.restored_command().replace('status=0',"printf '%s' '"+marker+"' > /workspace/out/attempt-started.json\nsleep 120\nstatus=0",1)
out,rec=r.run('interrupted',[str(r.BIN),'restore',str(root/'snapshot'),'--image',str(r.IMAGE),'--toolstore',str(r.STORE),'--command',command,'--max-wall','300s','--out',str(r.LAB/'interrupted/out'),'--json'],timeout=420)
print(json.dumps(rec),flush=True)
