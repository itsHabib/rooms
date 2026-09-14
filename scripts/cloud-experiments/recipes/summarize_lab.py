import datetime,json,math,statistics
from pathlib import Path
lab=Path('/home/rooms/lab/density-v3')
def quantile(values,p):
 values=sorted(values)
 return values[max(0,math.ceil(len(values)*p)-1)] if values else None
def job_times(directory):
 rows=[]
 for p in directory.glob('*/result.json'):
  r=json.loads(p.read_text());a=datetime.datetime.fromisoformat(r['started_at'].replace('Z','+00:00'));b=datetime.datetime.fromisoformat(r['ended_at'].replace('Z','+00:00'));rows.append((b-a).total_seconds())
 return {'count':len(rows),'command_p50_seconds':quantile(rows,.5),'command_p99_seconds':quantile(rows,.99),'command_max_seconds':max(rows) if rows else None}
mem=[json.loads(x) for x in (lab/'density-memory.ndjson').read_text().splitlines()]
density=[]
for r in json.loads((lab/'density-results.json').read_text()):
 samples=[x for x in mem if r['started_unix']<=x['unix']<=r['started_unix']+r['elapsed_seconds'] and len(x['vmm'])==r['requested']]
 peak=max(samples,key=lambda x:sum(v['Pss'] for v in x['vmm'])) if samples else None
 density.append({k:v for k,v in r.items() if k!='host_after'}|job_times(lab/r['name']/'out')|{'full_density_samples':len(samples),'peak_full_density_pss_kib':sum(v['Pss'] for v in peak['vmm']) if peak else None,'peak_full_density_rss_kib':sum(v['Rss'] for v in peak['vmm']) if peak else None})
shared=[]
for r in json.loads((lab/'shared-store-v2-results.json').read_text()):
 samples=json.loads((lab/f"shared-store-v2-{r['n']}"/'memory.json').read_text());valid=[s for s in samples if len(s['vmm_pss_kib'])==r['n'] and s['fincore_exit']==0]
 peak=max(valid,key=lambda x:sum(x['vmm_pss_kib']))
 shared.append({k:v for k,v in r.items() if k!='host_after'}|{'peak_full_density_pss_kib':sum(peak['vmm_pss_kib']),'host_cache_at_peak':json.loads(peak['fincore'])})
normal=lab/'hostile-64/out';times=[]
for p in normal.glob('normal-*/result.json'):
 r=json.loads(p.read_text());times.append((datetime.datetime.fromisoformat(r['ended_at'].replace('Z','+00:00'))-datetime.datetime.fromisoformat(r['started_at'].replace('Z','+00:00'))).total_seconds())
hostile={'normal_count':len(times),'normal_command_p50_seconds':quantile(times,.5),'normal_command_p99_seconds':quantile(times,.99)}
backings={}
for prefix in ['backing-v3','uffd-v3']:
 for kind in ['disk','tmpfs-never','tmpfs-always']:
  values=[json.loads((lab/f'{prefix}-{kind}-{i}/summary.json').read_text())['elapsed_seconds'] for i in range(3)]
  backings[prefix+'-'+kind]={'trials_seconds':values,'median_seconds':statistics.median(values)}
result={'density':density,'shared_store':shared,'hostile':hostile,'backings':backings,'interpretation':'p99 is nearest-rank within this small batch of guest COMMAND durations, not admission-to-ready latency or a stable tail estimate.'}
(lab/'readout.json').write_text(json.dumps(result,indent=2));print(json.dumps(result,indent=2))
