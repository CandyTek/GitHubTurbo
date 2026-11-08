"""Verify real URLs through default transparent integration, without a proxy flag."""
import datetime
import hashlib
import json
import pathlib
import re
import subprocess

ROOT=pathlib.Path(__file__).resolve().parents[1]
REPORTS=ROOT/'reports'
REPORTS.mkdir(exist_ok=True)
session=json.loads((ROOT/'dist/state/session.json').read_text(encoding='utf-8-sig'))
if session.get('phase')!='enabled':raise RuntimeError('Wait for system integration to finish before running network checks')
cases=[('web','https://github.com/'),('api','https://api.github.com/repos/octocat/Hello-World'),('raw','https://raw.githubusercontent.com/octocat/Hello-World/master/README'),('archive','https://codeload.github.com/octocat/Hello-World/zip/refs/heads/master'),('release','https://github.com/cli/cli/releases/download/v2.103.3/gh_2.103.3_checksums.txt')]
cases.insert(1,('repository','https://github.com/octocat/Hello-World'))
results=[]
for name,url in cases:
    path=REPORTS/('smoke-0.3-'+name+'.bin')
    result=subprocess.run(['curl.exe','--noproxy','*','--silent','--show-error','--fail','--location','--proto','=https','--proto-redir','=https','--connect-timeout','15','--max-time','90','--output',str(path),'--write-out','%{json}',url],capture_output=True,text=True,timeout=95)
    try: metrics=json.loads(result.stdout)
    except ValueError: metrics={}
    row={'case':name,'url':url,'exit':result.returncode,'error':result.stderr.strip(),'local_ca':('GitHubTurbo' in metrics['certs']) if metrics.get('certs') else None,**{key:metrics.get(key) for key in ['http_code','time_total','size_download','num_redirects']}}
    if result.returncode==0 and path.exists():row['sha256']=hashlib.sha256(path.read_bytes()).hexdigest()
    results.append(row)
    print(json.dumps(row),flush=True)
    if name in ('web','repository') and result.returncode==0 and not any(case[0]=='css' for case in cases):
        links=re.findall(r'https://github\.githubassets\.com/[^"\s<>]+\.css',path.read_text(encoding='utf-8',errors='replace'))
        if links:cases.append(('css',links[0]))
report={'utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'note':'Real sequential smoke samples, upstream=null, no explicit proxy, full TLS verification; not a throughput benchmark.','results':results}
(REPORTS/'smoke-0.3.json').write_text(json.dumps(report,indent=2),encoding='utf-8')
