import hashlib,json,subprocess,tarfile,time,pwd,os
from pathlib import Path
home=Path('/home/rooms');lab=home/'lab';lab.mkdir(exist_ok=True)
private=json.loads((home/'transfer-download-private.json').read_text())
url=private[0]['signed_url'];bundle=lab/'snapshot.tar.gz'
config='url = "'+url+'"\noutput = "'+str(bundle)+'"\nfail\nsilent\nshow-error\n'
started=time.monotonic();p=subprocess.run(['curl','--config','-','--max-time','300'],input=config,text=True,capture_output=True)
result={'download_seconds':time.monotonic()-started,'download_exit':p.returncode}
assert p.returncode==0,'download failed (URL withheld)'
def digest(p):
 with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
result.update(bundle_sha256=digest(bundle),bundle_bytes=bundle.stat().st_size)
assert result['bundle_sha256']=='c6c0364dda062c70efe42bd28eba129f0fb49314be98a2d78976c2af928a9d23'
root=lab/'travel';root.mkdir(exist_ok=False);started=time.monotonic()
with tarfile.open(bundle) as tar:tar.extractall(root,filter='data')
manifest=json.loads((root/'manifest.json').read_text())
assert all(digest(root/k)==v['sha256'] and (root/k).stat().st_size==v['bytes'] for k,v in manifest.items())
result.update(unpack_verify_seconds=time.monotonic()-started,verified_files=len(manifest))
for p in [root/'bin/rooms',root/'bin/lab-import-snapshot']:p.chmod(0o755)
fc=pwd.getpwnam('firecracker')
for p in (root/'snapshot').iterdir():os.chown(p,fc.pw_uid,fc.pw_gid)
for p in [*list((root/'snapshot').iterdir()),root/'images/agent.ext4',*list((root/'python').iterdir())]:
 p.chmod(0o400 if p.name=="snapshot.mem" else 0o444);subprocess.run(['chattr','+i',str(p)],check=True)
for p in [root/'snapshot',root/'python']:subprocess.run(['chattr','+i',str(p)],check=True)
subprocess.run(['bash',str(root/'scripts/setup-tap.sh'),'--host'],check=True,stdout=(lab/'travel-network.log').open('w'),stderr=subprocess.STDOUT)
key=Path('/root/.ssh');key.mkdir(exist_ok=True)
subprocess.run(['install','-m','600',str(home/'migration-key-private'),str(key/'id_rooms')],check=True)
started=time.monotonic();p=subprocess.run([str(root/'bin/lab-import-snapshot'),str(root/'snapshot'),str(root/'images/agent.ext4'),str(root/'python')],capture_output=True,text=True)
result.update(import_seconds=time.monotonic()-started,import_exit=p.returncode,import_stdout=p.stdout,import_stderr=p.stderr)
(lab/'transfer-download-result.json').write_text(json.dumps(result,indent=2));print(json.dumps(result),flush=True)
assert p.returncode==0
