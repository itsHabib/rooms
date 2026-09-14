import hashlib,json,os,subprocess,tarfile,time,urllib.request
from pathlib import Path
import remote as r
r.LAB=r.READY/'lab/density-v3'
snap=Path(json.loads((r.LAB/'base/summary.json').read_text())['snapshot']['result']['directory'])
files={f'snapshot/{n}':snap/n for n in ['snapshot.json','snapshot.vmstate','snapshot.mem']}
files.update({'images/agent.ext4':r.IMAGE,'images/vmlinux.bin':r.IMAGE.with_name('vmlinux.bin'),'python/toolstore.sqfs':r.STORE/'toolstore.sqfs','python/meta.json':r.STORE/'meta.json','bin/rooms':r.BIN,'bin/lab-import-snapshot':r.BIN.parent/'examples/lab-import-snapshot','scripts/setup-tap.sh':r.READY/'src/scripts/setup-tap.sh'})
started=time.monotonic()
manifest={name:{'sha256':r.digest(path),'bytes':path.stat().st_size} for name,path in files.items()}
manifest_path=r.READY/'lab/transfer-manifest.json'
manifest_path.write_text(json.dumps(manifest,indent=2))
bundle=r.READY/'lab/snapshot.tar.gz'
with tarfile.open(bundle,'w:gz',compresslevel=1) as tar:
 for name,path in files.items(): tar.add(path,arcname=name,recursive=False)
 tar.add(manifest_path,arcname='manifest.json',recursive=False)
result={'bundle_sha256':r.digest(bundle),'bundle_bytes':bundle.stat().st_size,'logical_bytes':sum(x['bytes'] for x in manifest.values()),'pack_seconds':time.monotonic()-started,'snapshot_id':json.loads((snap/'snapshot.json').read_text())['snapshot_id']}
private=json.loads(Path('/home/rooms/transfer-upload-private.json').read_text())
url=private[0]['signed_url']
# Pass signed URL through curl config stdin, never argv/logs or evidence.
started=time.monotonic()
config='url = "'+url+'"\nrequest = "PUT"\nupload-file = "'+str(bundle)+'"\nfail\nsilent\nshow-error\n'
p=subprocess.run(['curl','--config','-','--max-time','300'],input=config,text=True,capture_output=True)
result.update(upload_seconds=time.monotonic()-started,upload_exit=p.returncode)
(r.READY/'lab/transfer-upload-result.json').write_text(json.dumps(result,indent=2))
print(json.dumps(result),flush=True)
if p.returncode: raise RuntimeError('Snapshot upload failed; signed URL intentionally withheld')
