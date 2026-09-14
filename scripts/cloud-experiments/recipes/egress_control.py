import json,socket,threading,time,shlex
from pathlib import Path
import remote as r
r.LAB=r.READY/'lab/density-v3'
snapshot=json.loads((r.LAB/'base/summary.json').read_text())['snapshot']['result']['directory']
server=socket.socket();server.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);server.bind(('10.81.0.2',18080));server.listen();server.settimeout(.5)
stop=threading.Event();connections=[]
def serve():
 while not stop.is_set():
  try:peer,address=server.accept()
  except TimeoutError:continue
  with peer:peer.sendall(b'rooms-lab-echo-v1');connections.append({'at':time.time(),'address':address})
t=threading.Thread(target=serve);t.start();rows=[]
try:
 for trial,mode in enumerate(['observe','none','observe']):
  guest="""import json,pathlib,socket
result={'connected':False}
try:
 with socket.create_connection(('10.81.0.2',18080),timeout=2) as s:result={'connected':True,'reply':s.recv(100).decode()}
except OSError as e:result['error']=str(e)
pathlib.Path('/workspace/out/egress-control.json').write_text(json.dumps(result))
assert result['connected']==EXPECTED
if result['connected']:assert result['reply']=='rooms-lab-echo-v1'
""".replace('EXPECTED',str(mode=='observe'))
  command='export PATH=/nix/var/rooms/env/bin:$PATH; python3 -c '+shlex.quote(guest)
  name=f'egress-control-{trial}-{mode}'
  argv=[str(r.BIN),'clone',snapshot,'--image',str(r.IMAGE),'--toolstore',str(r.STORE),'-n','1','--command',command,'--max-wall','30s','--out',str(r.LAB/name/'out'),'--json']
  if mode=='none':argv+=['--egress','none']
  out,rec=r.run(name,argv)
  receipts=[json.loads(p.read_text()) for p in (out/'out').glob('*/egress-control.json')]
  rows.append({'mode':mode,'cli_exit':rec['cli_exit'],'receipts':receipts})
  assert rec['cli_exit']==0 and len(receipts)==1
finally:
 stop.set();t.join();server.close()
 (r.LAB/'egress-control-results.json').write_text(json.dumps({'rows':rows,'server_connections':connections,'host_after':r.host_state()},indent=2))
print(json.dumps(rows),flush=True)
