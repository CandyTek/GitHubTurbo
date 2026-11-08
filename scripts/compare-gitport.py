"""Read-only, alternating real-network samples of the user's running GitPort and Turbo.
Does not change GitPort settings or install its CA. Results describe current configurations.
"""
import datetime,hashlib,json,pathlib,subprocess,urllib.request
ROOT=pathlib.Path(__file__).resolve().parents[1]
OUT=ROOT/'reports';OUT.mkdir(exist_ok=True)
def local(url):
    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(url,timeout=4) as r:return json.load(r)
reference=local('http://127.0.0.1:7811/api/status')
turbo=local('http://127.0.0.1:18080/status')
results=[]
report={'utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'reference':{'version':reference.get('version'),'mitm':reference.get('mitm'),'exe_sha256':hashlib.sha256(pathlib.Path(r'D:\Download3\gitport-github.exe').read_bytes()).hexdigest()},'turbo':{'version':turbo['version'],'optimization':turbo.get('optimization'),'upstream':turbo['upstream']},'limitations':'Alternating sequential live samples, two rounds. GitPort tested as currently configured without changing its MITM/PAC settings. Not a controlled bandwidth comparison; warm cache and route reuse are reported separately. TLS validation stays enabled.','results':results}
report['turbo']['exe_sha256']=hashlib.sha256((ROOT/'dist/github-turbo.exe').read_bytes()).hexdigest()
cases=[('raw','https://raw.githubusercontent.com/octocat/Hello-World/master/README'),('css','https://github.githubassets.com/assets/light-0011b1fed09cb294.css'),('release','https://github.com/cli/cli/releases/download/v2.103.3/gh_2.103.3_checksums.txt')]
routes=['turbo','gitport']
for round_no in range(2):
    for name,url in cases:
        order=routes if round_no==0 else list(reversed(routes))
        for route in order:
            output=OUT/('compare-'+route+'-'+name+'.bin')
            args=['curl.exe','--silent','--show-error','--fail','--location','--proto','=https','--proto-redir','=https','--connect-timeout','8','--max-time','30','--max-filesize','4194304','--output',str(output),'--write-out','%{json}','--user-agent','GitHubTurbo-comparison/0.3']
            args+=['--noproxy','*'] if route=='turbo' else ['--noproxy','','--proxy','http://127.0.0.1:7810']
            p=subprocess.run(args+[url],capture_output=True,text=True,timeout=35)
            try:metrics=json.loads(p.stdout)
            except ValueError:metrics={}
            row={'round':round_no+1,'case':name,'route':route,'exit':p.returncode,'error':p.stderr.strip(),'code':metrics.get('http_code'),'seconds':metrics.get('time_total'),'bytes':metrics.get('size_download'),'local_ca':('GitHubTurbo' in metrics['certs']) if metrics.get('certs') else None}
            if p.returncode==0:row['sha256']=hashlib.sha256(output.read_bytes()).hexdigest()
            results.append(row);print(json.dumps(row),flush=True)
            (OUT/'comparison-0.3.json').write_text(json.dumps(report,indent=2),encoding='utf-8')
