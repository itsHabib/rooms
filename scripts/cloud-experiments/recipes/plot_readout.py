import json,statistics,sys
from pathlib import Path
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
source=Path(sys.argv[1]);out=Path(sys.argv[2]);d=json.loads(source.read_text());counts=[8,16,32,64,128]
throughput=[statistics.median(r['completed_per_minute'] for r in d['density'] if r['requested']==n) for n in counts]
pss=[max(r['peak_full_density_pss_kib'] for r in d['density'] if r['requested']==n)/1024**2 for n in counts]
plt.rcParams.update({'font.family':'DejaVu Sans','font.size':11,'axes.spines.top':False,'axes.spines.right':False})
fig,axes=plt.subplots(1,2,figsize=(11,5.1));fig.patch.set_facecolor('#fafaf8')
for ax in axes:ax.set_facecolor('#fafaf8');ax.set_xticks(range(5),[str(n) for n in counts]);ax.set_xlabel('Concurrent restored Rooms');ax.grid(axis='y',alpha=.17);ax.set_axisbelow(True)
axes[0].bar(range(5),throughput,color=['#597c91','#597c91','#176f55','#597c91','#597c91'],width=.63)
axes[0].set_title('Useful throughput peaks at 32',loc='left',weight='bold',pad=15);axes[0].set_ylabel('Successful jobs / minute');axes[0].set_ylim(0,145)
for i,v in enumerate(throughput):axes[0].text(i,v+3,f'{v:.1f}',ha='center')
axes[1].bar(range(5),pss,color='#597c91',width=.63);axes[1].set_title('128 Rooms: 7.4 GiB summed PSS',loc='left',weight='bold',pad=15);axes[1].set_ylabel('Peak summed VMM PSS (GiB)');axes[1].set_ylim(0,8.7)
for i,v in enumerate(pss):axes[1].text(i,v+.16,f'{v:.2f}',ha='center')
fig.suptitle('Rooms density lab · 496 / 496 jobs passed',x=.07,ha='left',fontsize=19,weight='bold')
fig.text(.07,.045,'32 vCPU / 128 GiB GCP N2 · nested KVM · fixed Workbench patch + 16 Python tests\nTwo runs per size. Throughput: median. PSS: largest full-density sample. Preparation excluded.',fontsize=9,color='#555')
fig.subplots_adjust(left=.075,right=.98,top=.77,bottom=.23,wspace=.28)
fig.savefig(out.with_suffix('.png'),dpi=170,facecolor=fig.get_facecolor());fig.savefig(out.with_suffix('.svg'),facecolor=fig.get_facecolor())
