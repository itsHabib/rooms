"""Export public lab evidence only; never walk snapshots, images or credentials."""
import hashlib,json,os,tarfile
from pathlib import Path
home=Path('/home/rooms');lab=home/'lab'
blocked_dirs={'tmpfs-never','tmpfs-always','copied-disk','go','go-ci','go-ci-v2','go-ci-extra','go-ci-extra.building','python','fake-store','__pycache__'}
blocked_suffixes={'.ext4','.sqfs','.mem','.vmstate','.gz','.bin'}
paths=[]
for root,dirs,files in os.walk(lab):
 dirs[:]=sorted(d for d in dirs if d not in blocked_dirs)
 for name in sorted(files):
  p=Path(root)/name
  if p.is_symlink() or p.suffix in blocked_suffixes or 'private' in name or name.startswith('rooms-'):continue
  assert p.stat().st_size<50*1024*1024,(str(p),'oversize evidence needs inspection')
  paths.append(p)
for p in home.iterdir():
 if p.is_file() and p.suffix in {'.py','.sh','.log','.rs'} and 'private' not in p.name and p.name != 'export_evidence.py':paths.append(p)
for p in (home/'ci-preset').iterdir():
 if p.is_file() and p.name in {'flake.nix','flake.lock','codex-origin.json'}:paths.append(p)
manifest=[]
for p in sorted(set(paths)):
 data=p.read_bytes()
 for marker in [b'-----BEGIN OPENSSH PRIVATE KEY-----',b'-----BEGIN PRIVATE KEY-----',b'X-Goog-Signature=',b'Authorization: Bearer ']:
  assert marker not in data,(str(p),'private content marker; inspect before export')
 manifest.append({'path':str(p.relative_to(home)),'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()})
out=home/'main-evidence.tar.gz'
(home/'public-evidence-manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
with tarfile.open(out,'w:gz') as t:
 for row in manifest:t.add(home/row['path'],arcname=row['path'],recursive=False)
 t.add(home/'public-evidence-manifest.json',arcname='public-evidence-manifest.json')
print(json.dumps({'archive':str(out),'bytes':out.stat().st_size,'sha256':hashlib.sha256(out.read_bytes()).hexdigest(),'files':len(manifest)}))
