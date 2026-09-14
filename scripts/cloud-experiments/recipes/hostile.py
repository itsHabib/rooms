"""Bounded destructive workloads only inside disposable guest VMs."""
import base64,gzip,json,shlex
import remote as r
r.LAB=r.READY/'lab/density-v3'
snapshot=json.loads((r.LAB/'base/summary.json').read_text())['snapshot']['result']['directory']
normal=r.restored_command().replace(r.patch_b64()+' | base64 -d',base64.b64encode(gzip.compress(r.PATCH.read_bytes(),mtime=0)).decode()+' | base64 -d | gzip -d')
payloads={
'memory':'''import time
blocks=[]
for _ in range(32): blocks.append(bytearray(64*1024*1024))
time.sleep(5)
''',
'disk':'''import errno,json,pathlib
n=0
error=None
try:
 with open('/tmp/fill','wb',buffering=0) as f:
  for _ in range(2048): n+=f.write(b'x'*(1024*1024))
except OSError as e:
 error=e.errno
finally:
 pathlib.Path('/tmp/fill').unlink(missing_ok=True)
pathlib.Path('/workspace/out/disk.json').write_text(json.dumps({'bytes':n,'errno':error}))
''',
'fork':'''import os,time
end=time.monotonic()+0.4
while time.monotonic()<end:
 try: os.fork()
 except OSError: break
time.sleep(5)
os._exit(0)
''',
'egress':'''import json,pathlib,socket
rows=[]
for host in ['169.254.169.254','10.81.0.2','172.17.0.6']:
 try:
  s=socket.create_connection((host,80),timeout=2);s.close();rows.append({'target':host,'connected':True})
 except OSError as e: rows.append({'target':host,'connected':False,'error':str(e)})
pathlib.Path('/workspace/out/egress.json').write_text(json.dumps(rows))
assert not any(r['connected'] for r in rows)
''',
'nix':'''import errno,json,pathlib
p=pathlib.Path('/nix/rooms-tamper-probe')
try:
 p.write_text('tamper');r={'write_succeeded':True}
except OSError as e:r={'write_succeeded':False,'errno':e.errno}
pathlib.Path('/workspace/out/nix.json').write_text(json.dumps(r))
assert not r['write_succeeded']
'''
}
cases=[{'id':f'normal-{i:02}','command':normal} for i in range(32)]
for kind,count in [('memory',8),('disk',8),('fork',8),('egress',4),('nix',4)]:
 for i in range(count):
  cmd='export PATH=/nix/var/rooms/env/bin:$PATH; python3 -c '+shlex.quote(payloads[kind])
  cases.append({'id':f'{kind}-{i:02}','command':cmd})
manifest=r.LAB/'hostile-cases.json';manifest.write_text(json.dumps({'schema':'rooms.matrix.v1','cases':cases}))
out,rec=r.run('hostile-64',[str(r.BIN),'matrix',snapshot,'--image',str(r.IMAGE),'--toolstore',str(r.STORE),'--cases',str(manifest),'--egress','none','--max-wall','30s','--out',str(r.LAB/'hostile-64/out'),'--json'],timeout=600)
normal_outputs=[r.collected(out/'out'/f'normal-{i:02}') for i in range(32)]
row={'cli_exit':rec['cli_exit'],'elapsed_seconds':rec['elapsed_seconds'],'normal_expected':32,'normal_successful':sum(bool(v['result'] and v['result']['exit_code']==0 and v['patch_matches_input']) for v in normal_outputs),'host_after':r.host_state(),'toolstore_unchanged':r.digest(r.STORE/'toolstore.sqfs')==json.loads((r.LAB/'inputs.json').read_text())['identities'][str(r.STORE/'toolstore.sqfs')]}
(r.LAB/'hostile-result.json').write_text(json.dumps(row,indent=2));print('HOSTILE_COMPLETE',json.dumps(row),flush=True)
