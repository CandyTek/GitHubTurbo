"""Direct IP throughput diagnosis; never enables an accelerator or modifies DNS/hosts."""
import argparse
import hashlib
import ipaddress
import json
import pathlib
import re
import secrets
import socket
import subprocess
import time
import urllib.parse
import bench

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--url',default='https://github.com/fastgh/fgit/releases/download/v1.2.0/fgit.exe')
    parser.add_argument('--rounds',type=int,default=3)
    parser.add_argument('--full-rounds',type=int,default=2)
    parser.add_argument('--timeout',type=int,default=15)
    args=parser.parse_args()
    if not 1<=args.rounds<=5 or not 0<=args.full_rounds<=3 or not 5<=args.timeout<=60:
        raise SystemExit('Invalid workload limit')
    url=urllib.parse.urlsplit(args.url)
    if url.scheme!='https' or url.hostname!='github.com' or url.username or url.password or url.query or url.fragment or not re.fullmatch(r'/[\w.-]+/[\w.-]+/releases/download/[^/]+/[^/]+',url.path):
        raise SystemExit('Use a public GitHub release download URL')
    # Refuse to run with active local reverse proxies; preserve the user-disabled state.
    for port in [18080,7810]:
        if bench.listening(port):raise SystemExit('An accelerator still listens on %s; leave it off for this diagnosis'%port)
    run_id=time.strftime('%Y%m%d-%H%M%S')+'-'+secrets.token_hex(3)
    folder=bench.RUNS/run_id;folder.mkdir(parents=True)
    report={'id':run_id,'pid':__import__('os').getpid(),'started':bench.utc(),'state':'preparing',
        'options':{'routes':[],'cases':['fgit-range'],'rounds':args.rounds,'phases':'cold','timeout':args.timeout},
        'routes':{},'rows':[],'events':[],'summary':[],'paired':[],
        'method':'Direct HTTP/2, original TLS hostname verification, new client connection per sample. Same temporary CDN URL, sequential rotated/reversed IP order. 1 MiB screening is followed by full-file checks. No accelerator, hosts or DNS settings changed. Reference hash comes from a TLS-verified system download; it is not a publisher checksum.'}
    def save():
        report['summary']=bench.summary(report['rows'])
        bench.write_json(folder/'report.json',report)
    def event(message):
        report['events'].append({'utc':bench.utc(),'message':message});save();print(message,flush=True)
    lease=bench.RunLease(bench.WORK/'run.lock')
    cancel=bench.CancelFlag(folder/'cancel.request')
    try:
        reference=folder/'reference.bin'
        event('获取用户文件的系统直连参考，不运行下载文件')
        p=bench.command(['curl.exe','--disable','--noproxy','*','--silent','--show-error','--fail','--location',
            '--proto','=https','--proto-redir','=https','--max-time','45','--max-filesize','33554432',
            '--output',str(reference),'--write-out','%{json}',args.url],50,cancel)
        metrics=json.loads(p['stdout'])
        if p['exit']!=0 or 'GitHubTurbo' in metrics.get('certs','') or not reference.is_file():
            raise RuntimeError('Cannot obtain a verified direct reference')
        signed=metrics['url_effective'];host=urllib.parse.urlsplit(signed).hostname
        issued=time.monotonic()
        if not host or not host.endswith('.githubusercontent.com'):raise RuntimeError('Unexpected asset host')
        total=reference.stat().st_size
        if total<1048576:raise RuntimeError('Asset smaller than 1 MiB')
        full_sha=bench.digest(reference)
        with reference.open('rb') as f:prefix_sha=hashlib.sha256(f.read(1048576)).hexdigest()
        reference.unlink()
        system=list(dict.fromkeys(x[4][0] for x in socket.getaddrinfo(host,443,type=socket.SOCK_STREAM)))
        system=[ip for ip in system if ipaddress.ip_address(ip).is_global]
        dns=bench.resolve(host,cancel)
        pinned={'system':None,'dns-108':'185.199.108.133','dns-110':'185.199.110.133',
            'seed-154':'185.199.110.154','seed-215':'185.199.108.215'}
        report['reference']={'url':args.url,'host':host,'bytes':total,'sha256':full_sha,
            'prefix_1m_sha256':prefix_sha,'seconds':metrics.get('time_total'),'remote_ip':metrics.get('remote_ip'),
            'publisher_digest':None}
        report['dns']={'system':system,'independent_doh':dns}
        report['routes']={key:{'ip':value,'note':'System name resolution' if value is None else 'Per-request pin; settings unchanged'} for key,value in pinned.items()}
        report['options']['routes']=list(pinned)
        report['state']='running';save()
        def sample(route,round_no,full=False):
            nonlocal signed,issued
            if cancel.is_set():raise bench.Cancelled()
            if time.monotonic()-issued>120:
                event('刷新临时 CDN 授权地址，报告不保存签名参数')
                renewal=bench.command(['curl.exe','--disable','--noproxy','*','--silent','--show-error','--fail',
                    '--location','--proto','=https','--proto-redir','=https','--range','0-0','--max-filesize','1024',
                    '--max-time','15','--output',__import__('os').devnull,'--write-out','%{json}',args.url],20,cancel)
                renewed=json.loads(renewal['stdout'])
                if renewal['exit'] or urllib.parse.urlsplit(renewed['url_effective']).hostname!=host:
                    raise RuntimeError('Unable to refresh the same asset host')
                signed=renewed['url_effective'];issued=time.monotonic()
            case='fgit-full' if full else 'fgit-range'
            event('%s round %s %s'%(case,round_no,route))
            payload={'url':signed,'ip':pinned[route],'timeout':args.timeout,
                'status':200 if full else 206,'bytes':total if full else 1048576,
                'total':total,'sha256':full_sha if full else prefix_sha}
            if not full:payload['range']='0-1048575'
            proc=subprocess.Popen(['node',str(bench.HERE/'ip-http2.js')],stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,stderr=subprocess.PIPE,env=bench.environment(),creationflags=bench.CREATE_FLAGS)
            try:
                proc.stdin.write(json.dumps(payload).encode());proc.stdin.close();proc.stdin=None
                deadline=time.monotonic()+args.timeout+5
                while proc.poll() is None:
                    if cancel.is_set():raise bench.Cancelled()
                    if time.monotonic()>deadline:raise RuntimeError('Probe child timeout')
                    time.sleep(.05)
                out,error=proc.communicate()
                if proc.returncode:raise RuntimeError('HTTP/2 probe error: '+error.decode('utf-8','replace')[:150])
                row=json.loads(out)
            finally:
                if proc.poll() is None:bench.kill_tree(proc);proc.communicate()
            row.update(case=case,route=route,phase='cold',round=round_no,utc=bench.utc(),
                verification='same_tls_verified_reference_sha256')
            if row.get('tls_verified') is not True:row['success']=False
            report['rows'].append(row);save()
            print(json.dumps({k:row.get(k) for k in ['case','route','remote_ip','seconds','success','error','protocol']}),flush=True)
        for round_no in range(args.rounds):
            for route in bench.route_order(list(pinned),round_no):sample(route,round_no+1)
        report['state']='completed';report['finished']=bench.utc();save()
        (folder/'report.html').write_text(bench.offline_html(report),encoding='utf-8')
        if args.full_rounds:
            parent_id=report['id']
            full_id=time.strftime('%Y%m%d-%H%M%S')+'-'+secrets.token_hex(3)
            folder=bench.RUNS/full_id;folder.mkdir()
            cancel=bench.CancelFlag(folder/'cancel.request')
            report=dict(report,id=full_id,state='running',started=bench.utc(),rows=[],summary=[],paired=[],events=[])
            report.pop('finished',None)
            report['screening_run']=parent_id
            report['options']=dict(report['options'],routes=['dns-108','seed-154'],cases=['fgit-full'],rounds=args.full_rounds)
            save()
            for round_no in range(args.full_rounds):
                for route in bench.route_order(['dns-108','seed-154'],round_no):sample(route,round_no+1,True)
            report['state']='completed'
    except (bench.Cancelled,KeyboardInterrupt):report['state']='cancelled'
    except Exception as e:report['state']='failed';report['error']=str(e)
    finally:
        for temporary in folder.glob('*.bin'):temporary.unlink()
        report['finished']=bench.utc();save()
        (folder/'report.html').write_text(bench.offline_html(report),encoding='utf-8')
        lease.close()
    print(json.dumps({'id':report['id'],'state':report['state'],'error':report.get('error'),'summary':report['summary']},ensure_ascii=False))

if __name__=='__main__':main()
