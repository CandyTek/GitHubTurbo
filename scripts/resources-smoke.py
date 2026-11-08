import hashlib,json,pathlib,re,subprocess
root=pathlib.Path(__file__).resolve().parents[1]
html=(root/'reports/v02-home.body').read_text(encoding='utf-8',errors='replace')
links=re.findall(r'https://github\.githubassets\.com/[^"\s<>]+\.css',html)
cases=[('api-followup','https://api.github.com/repos/octocat/Hello-World')]
if links:cases.append(('css',links[0]))
rows=[]
for name,url in cases:
    path=root/('reports/v02-'+name+'.bin')
    result=subprocess.run(['curl.exe','--noproxy','*','-fSs','--max-time','40','-o',str(path),'-w','%{json}',url],capture_output=True,text=True,timeout=45)
    metrics=json.loads(result.stdout)
    row={'case':name,'exit':result.returncode,'http_code':metrics['http_code'],'seconds':metrics['time_total'],'bytes':metrics['size_download'],'error':result.stderr.strip()}
    if result.returncode==0:row['sha256']=hashlib.sha256(path.read_bytes()).hexdigest()
    rows.append(row);print(json.dumps(row),flush=True)
(root/'reports/resources-0.2.json').write_text(json.dumps(rows,indent=2),encoding='utf-8')
