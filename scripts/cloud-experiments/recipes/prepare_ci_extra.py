import base64,hashlib,io,json,tarfile,urllib.request
from pathlib import Path
p=Path('/home/rooms/ci-preset')
meta=json.load(urllib.request.urlopen('https://registry.npmjs.org/@openai/codex/0.153.4-linux-x64'))
data=urllib.request.urlopen(meta['dist']['tarball']).read()
assert 'sha512-'+base64.b64encode(hashlib.sha512(data).digest()).decode()==meta['dist']['integrity']
with tarfile.open(fileobj=io.BytesIO(data),mode='r:gz') as tar:
 names=[m for m in tar.getmembers() if m.isfile() and m.name.endswith('/bin/codex')]
 assert len(names)==1
 (p/'codex-bin').write_bytes(tar.extractfile(names[0]).read())
(p/'codex-bin').chmod(0o755)
(p/'codex-origin.json').write_text(json.dumps({'version':meta['version'],'tarball':meta['dist']['tarball'],'integrity':meta['dist']['integrity'],'binary_sha256':hashlib.sha256((p/'codex-bin').read_bytes()).hexdigest()},indent=2))
flake=p/'flake.nix';s=flake.read_text();needle='go = [ pkgs.go pkgs.gcc pkgs.gh ('
assert needle in s
s=s.replace(needle, 'go = [ pkgs.go pkgs.gcc pkgs.gh pkgs.python3 pkgs.nodejs_22 pkgs.jq pkgs.git pkgs.bash pkgs.coreutils pkgs.findutils pkgs.gnused\n            (pkgs.runCommand "rooms-codex-rule-checker" {} \'\'\n              mkdir -p $out/bin\n              cp ${./codex-bin} $out/bin/codex\n              chmod +x $out/bin/codex\n            \'\') (')
flake.write_text(s)
print(json.dumps({'version':meta['version'],'binary_bytes':(p/'codex-bin').stat().st_size}),flush=True)
