"""Real network benchmark, isolated TLS sidecars, local dashboard. Python 3.8+ stdlib."""
import argparse
import collections
import copy
import ctypes
import datetime
import hashlib
import http.server
import ipaddress
import json
import math
import os
import pathlib
import random
import re
import secrets
import shutil
import socket
import statistics
import subprocess
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent
RUNS = HERE/'runs'
WORK = HERE/'work'
BIN = HERE/'binaries'
CASES = json.loads((HERE/'cases.json').read_text(encoding='utf-8'))
CLIENT = urllib.request.build_opener(urllib.request.ProxyHandler({}))
CREATE_FLAGS = 0x08000000 if os.name == 'nt' else 0
TOKEN = secrets.token_hex(24)

def utc():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()

def digest(path):
    h = hashlib.sha256()
    with open(str(path), 'rb') as f:
        for block in iter(lambda: f.read(1024*1024), b''):
            h.update(block)
    return h.hexdigest()

def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_suffix('.tmp')
    temp.write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding='utf-8')
    os.replace(str(temp), str(path))

def local(url, method='GET'):
    req = urllib.request.Request(url, method=method,
        headers={'X-GitHub-Turbo-Request': '1'} if method == 'POST' else {})
    with CLIENT.open(req, timeout=5) as r:
        return json.load(r)

def environment():
    env = dict(os.environ)
    for k in list(env):
        if k.lower() in ('http_proxy','https_proxy','all_proxy','no_proxy') or k.startswith('GIT_CONFIG_'):
            env.pop(k, None)
    env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_SYSTEM=os.devnull,
        GIT_TERMINAL_PROMPT='0', GCM_INTERACTIVE='Never')
    return env

class Cancelled(Exception):
    pass

class CancelFlag(threading.Event):
    def __init__(self, path):
        super().__init__()
        self.path = path
    def is_set(self):
        return super().is_set() or self.path.exists()
    def set(self):
        super().set()
        self.path.parent.mkdir(parents=True,exist_ok=True)
        self.path.touch()

class RunLease:
    """OS releases this lock on process exit, including a killed CLI."""
    def __init__(self,path):
        path.parent.mkdir(parents=True,exist_ok=True)
        self.file = open(str(path),'a+b')
        if path.stat().st_size==0:
            self.file.write(b'1'); self.file.flush()
        self.file.seek(0)
        try:
            if os.name=='nt':
                import msvcrt
                msvcrt.locking(self.file.fileno(),msvcrt.LK_NBLCK,1)
            else:
                import fcntl
                fcntl.flock(self.file,fcntl.LOCK_EX|fcntl.LOCK_NB)
        except OSError:
            self.file.close()
            raise RuntimeError('Another benchmark owns the network test lock')
    def close(self):
        self.file.close()

def process_alive(pid):
    if not isinstance(pid,int) or pid<=0:
        return False
    if os.name=='nt':
        from ctypes import wintypes
        kernel=ctypes.WinDLL('kernel32',use_last_error=True)
        kernel.OpenProcess.restype=wintypes.HANDLE
        kernel.OpenProcess.argtypes=(wintypes.DWORD,wintypes.BOOL,wintypes.DWORD)
        kernel.GetExitCodeProcess.argtypes=(wintypes.HANDLE,ctypes.POINTER(wintypes.DWORD))
        kernel.CloseHandle.argtypes=(wintypes.HANDLE,)
        handle=kernel.OpenProcess(0x1000,False,pid)
        if not handle:return False
        code=wintypes.DWORD()
        try:return bool(kernel.GetExitCodeProcess(handle,ctypes.byref(code)) and code.value==259)
        finally:kernel.CloseHandle(handle)
    try:
        os.kill(pid,0)
        return True
    except OSError:return False

def command(args, budget, cancel=None, cwd=None):
    start = time.monotonic()
    p = subprocess.Popen([str(x) for x in args], cwd=cwd, env=environment(),
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, creationflags=CREATE_FLAGS)
    timed_out = False
    try:
        while True:
            if cancel is not None and cancel.is_set():
                raise Cancelled()
            try:
                out, error = p.communicate(timeout=0.15)
                break
            except subprocess.TimeoutExpired:
                if time.monotonic()-start > budget:
                    timed_out = True
                    kill_tree(p)
                    out, error = p.communicate()
                    break
        return {'exit': p.returncode, 'stdout': out.decode('utf-8', 'replace'),
            'stderr': error.decode('utf-8', 'replace'), 'wall_seconds': time.monotonic()-start,
            'timeout': timed_out}
    finally:
        if p.poll() is None:
            kill_tree(p)
            p.communicate()

def kill_tree(process):
    if process.poll() is None:
        if os.name == 'nt':
            subprocess.run(['taskkill.exe','/PID',str(process.pid),'/T','/F'],
                stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,creationflags=CREATE_FLAGS)
        else:
            process.kill()

def available_port(udp=False):
    for _ in range(50):
        with socket.socket() as s:
            s.bind(('127.0.0.1', 0))
            port = s.getsockname()[1]
            if udp:
                try:
                    with socket.socket(type=socket.SOCK_DGRAM) as u:
                        u.bind(('127.0.0.1', port))
                except OSError:
                    continue
            return port
    raise RuntimeError('No free loopback port')

def listening(port):
    try:
        with socket.create_connection(('127.0.0.1',port),timeout=.2):
            return True
    except OSError:
        return False

class Sidecar:
    def __init__(self, label, exe, folder, overrides=None, fixed_ips=None):
        self.label, self.exe, self.folder = label, pathlib.Path(exe).resolve(), folder
        self.port = available_port()
        self.control = 'http://127.0.0.1:%s' % available_port()
        self.p = None
        self.overrides = overrides or {}
        self.fixed_ips = fixed_ips or {}

    def start(self, cancel):
        self.folder.mkdir(parents=True, exist_ok=True)
        if self.exe==(BIN/'baseline.exe').resolve():
            original=json.loads((BIN/'baseline.json').read_text(encoding='utf-8'))
            if digest(self.exe)!=original['sha256']:
                raise RuntimeError('Frozen baseline binary hash changed')
        template = self.exe.with_suffix('.config.json')
        if not template.exists():
            template = self.exe.parent/'config.json'
        if not template.exists():
            template = ROOT/'config.json'
        cfg = json.loads(template.read_text(encoding='utf-8-sig'))
        cfg['optimization'].update(self.overrides)
        cfg.update(listen=self.control[7:], mode='direct', upstream=None, host_modes={}, ip_overrides=self.fixed_ips)
        cfg['transparent'].update(enabled=True, https_listen='127.0.0.1:%s'%self.port,
            http_listen='127.0.0.1:%s'%available_port(), dns_listen='127.0.0.1:%s'%available_port(True),
            state_dir=str((self.folder/'state').resolve()))
        self.config = self.folder/'config.json'
        write_json(self.config, cfg)
        result = command([self.exe, 'init-ca', self.config], 15, cancel)
        if result['exit']:
            raise RuntimeError('Cannot generate isolated CA: '+result['stderr'][:200])
        self.ca = self.folder/'state/ca.pem'
        self.log = open(str(self.folder/'engine.log'), 'wb')
        self.p = subprocess.Popen([str(self.exe), 'serve', str(self.config)], cwd=str(self.folder),
            stdout=self.log, stderr=self.log, env=environment(), creationflags=CREATE_FLAGS)
        for _ in range(80):
            if cancel.is_set():
                raise Cancelled()
            if self.p.poll() is not None:
                raise RuntimeError('Sidecar stopped; see '+str(self.folder/'engine.log'))
            try:
                self.initial = local(self.control+'/status')
                return self
            except (OSError, ValueError):
                time.sleep(0.1)
        raise RuntimeError('Sidecar readiness timeout')

    def close(self):
        if self.p is not None and self.p.poll() is None:
            self.p.terminate()
            try:
                self.p.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.p.kill()
                self.p.wait()
        if hasattr(self, 'log'):
            self.log.close()

    def reset(self):
        return local(self.control+'/api/reset', 'POST')

    def status(self):
        s = local(self.control+'/status')
        return {k:s[k] for k in ('pool','cache','health','downloaded_bytes','uploaded_bytes','range_memo','warm_tls','transfers','routes','host_sources','transfer_history') if k in s}

def resolve(host, cancel):
    # Dedicated bootstrap bypasses Windows hosts/NRPT and all proxy environment variables.
    url = 'https://dns.alidns.com/resolve?'+urllib.parse.urlencode({'name':host,'type':'A'})
    p = command(['curl.exe','--noproxy','*','--silent','--show-error','--fail',
        '--resolve','dns.alidns.com:443:223.5.5.5','--max-time','10', url], 12, cancel)
    if p['exit']:
        raise RuntimeError('Independent DNS failed for '+host)
    doc = json.loads(p['stdout'])
    addresses = []
    for answer in doc.get('Answer', []):
        try:
            ip = ipaddress.ip_address(answer.get('data',''))
            if answer.get('type') == 1 and ip.is_global:
                addresses.append(str(ip))
        except ValueError:
            pass
    if not addresses:
        raise RuntimeError('No public A record for '+host)
    return list(dict.fromkeys(addresses))

SAFE_HEADERS = ('content-range','content-length','content-encoding','etag','age','cache-control',
    'x-github-turbo-route','x-github-turbo-split','x-github-turbo-cache','x-github-turbo-upstream')

def final_headers(path):
    # Never keep Location, cookies, signed redirects or response body in the exported report.
    raw = path.read_text(encoding='utf-8', errors='replace') if path.exists() else ''
    blocks = re.split(r'\r?\n\r?\n', raw)
    result = {}
    for block in blocks:
        if not block.startswith('HTTP/'):
            continue
        result = {}
        for line in block.splitlines()[1:]:
            key, sep, value = line.partition(':')
            if sep and key.lower() in SAFE_HEADERS:
                result[key.lower()] = value.strip()
    return result

def validate(case, path, code, headers):
    reasons = []
    if code != case['status']:
        reasons.append('HTTP %s, expected %s'%(code, case['status']))
    length = path.stat().st_size if path.exists() else 0
    if 'bytes' in case and length != case['bytes']:
        reasons.append('incomplete_or_wrong_length')
    actual = digest(path) if path.exists() else None
    if 'sha256' in case and actual != case['sha256']:
        reasons.append('sha256_mismatch')
    if case.get('range') and headers.get('content-range') != 'bytes %s/%s'%(case['range'],case['total']):
        reasons.append('content_range_mismatch')
    if case.get('validator') and path.exists():
        data = path.read_bytes()
        if case['validator'] == 'html' and not (b'<html' in data[:8192].lower() and b'Hello-World' in data):
            reasons.append('invalid_repository_html')
        if case['validator'] == 'repository':
            try:
                if json.loads(data).get('full_name') != 'octocat/Hello-World':
                    reasons.append('wrong_repository')
            except ValueError:
                reasons.append('invalid_json')
    return reasons, length, actual

def summary(rows):
    groups = collections.defaultdict(list)
    for row in rows:
        groups[(row['case'],row['route'],row['phase'])].append(row)
    output = []
    for (case,route,phase), values in groups.items():
        good = [r for r in values if r['success']]
        times = sorted(r['seconds'] for r in good)
        output.append({'case':case,'route':route,'phase':phase,'attempts':len(values),
            'successes':len(good),'success_rate':len(good)/len(values),
            'median_seconds':statistics.median(times) if times else None,
            'p95_seconds':times[max(0,math.ceil(len(times)*.95)-1)] if times else None,
            'median_bps':statistics.median(r['bps'] for r in good) if good else None,
            'timeouts':sum(r.get('timeout',False) for r in values)})
    return output

def paired(rows, baseline='baseline', candidate='candidate'):
    index = {(r['round'],r['case'],r['phase'],r['route']):r for r in rows}
    groups = collections.defaultdict(lambda: {'pairs':0,'candidate_wins':0,'ratios':[],'baseline_failures':0,'candidate_failures':0})
    for key, a in index.items():
        if key[-1] != baseline:
            continue
        b = index.get(key[:-1]+(candidate,))
        if not b:
            continue
        group = groups[key[1:3]]
        group['baseline_failures'] += not a['success']
        group['candidate_failures'] += not b['success']
        if a['success'] and b['success']:
            group['pairs'] += 1
            group['candidate_wins'] += b['seconds'] < a['seconds']
            group['ratios'].append(a['seconds']/max(b['seconds'],.000001))
    return [dict(case=k[0],phase=k[1],valid_pairs=v['pairs'],candidate_wins=v['candidate_wins'],
        baseline_failures=v['baseline_failures'],candidate_failures=v['candidate_failures'],
        median_speedup=statistics.median(v['ratios']) if v['ratios'] else None) for k,v in groups.items()]

def route_order(routes, round_no):
    shift = (round_no//2)%len(routes)
    order = list(routes[shift:])+list(routes[:shift])
    return list(reversed(order)) if round_no%2 else order

class Monitor:
    def __init__(self, processes):
        self.processes = processes
        self.stop = threading.Event()
        self.peaks = {}
        self.thread = threading.Thread(target=self.loop, daemon=True)

    def loop(self):
        if os.name != 'nt':
            return
        from ctypes import wintypes
        class Memory(ctypes.Structure):
            _fields_ = [('cb',wintypes.DWORD),('faults',wintypes.DWORD)]+[(n,ctypes.c_size_t) for n in
                ('peak_ws','ws','peak_paged','paged','peak_nonpaged','nonpaged','pagefile','peak_pagefile','private')]
        kernel = ctypes.WinDLL('kernel32', use_last_error=True)
        kernel.OpenProcess.restype = wintypes.HANDLE
        kernel.OpenProcess.argtypes = (wintypes.DWORD,wintypes.BOOL,wintypes.DWORD)
        kernel.K32GetProcessMemoryInfo.argtypes = (wintypes.HANDLE,ctypes.POINTER(Memory),wintypes.DWORD)
        kernel.CloseHandle.argtypes = (wintypes.HANDLE,)
        while not self.stop.is_set():
            for name,pid in self.processes.items():
                handle = kernel.OpenProcess(0x410,False,pid)
                if handle:
                    m = Memory(); m.cb = ctypes.sizeof(m)
                    if kernel.K32GetProcessMemoryInfo(handle,ctypes.byref(m),m.cb):
                        peak = self.peaks.setdefault(name,{'peak_ws_bytes':0,'peak_private_bytes':0})
                        peak['peak_ws_bytes'] = max(peak['peak_ws_bytes'],m.ws)
                        peak['peak_private_bytes'] = max(peak['peak_private_bytes'],m.private)
                    kernel.CloseHandle(handle)
            self.stop.wait(.25)

class Runner:
    def __init__(self, options, run_id=None):
        self.options = options
        self.id = run_id or datetime.datetime.now().strftime('%Y%m%d-%H%M%S')+'-'+secrets.token_hex(3)
        self.dir = RUNS/self.id
        self.cancel = CancelFlag(self.dir/'cancel.request')
        self.report = {'id':self.id,'pid':os.getpid(),'started':utc(),'state':'preparing','options':options,
            'rows':[],'events':[],'summary':[],'paired':[],'routes':{},
            'method':'Sequential balanced order (AB/BA); TLS verification required. Cold resets Turbo routing/pool/cache only. Warm repeats the same case on its previous connection/cache (other routes run between repeats); warm-only runs add unmeasured primers. OS TLS, DNS and remote CDN caches are not reset. Direct uses pinned independent DoH addresses; DNS preparation time is reported separately. Git uses isolated config and HTTP/1.1 frontend; clone and fetch timings are separate. Timeouts and invalid content remain failures; latency statistics include only successful, validated samples.'}
        self.lock = threading.RLock()
        self.sidecars = {}
        self.addresses = {}

    def save(self):
        with self.lock:
            self.report['summary'] = summary(self.report['rows'])
            self.report['paired'] = paired(self.report['rows'])
            write_json(self.dir/'report.json',self.report)

    def event(self, message):
        with self.lock:
            self.report['events'].append({'utc':utc(),'message':message})
            self.report['events'] = self.report['events'][-100:]
            self.save()

    def snapshot(self):
        with self.lock:
            return copy.deepcopy(self.report)

    def http(self, case, route, tag):
        output, header = self.dir/(tag+'.body'), self.dir/(tag+'.headers')
        timeout = self.options['timeout']
        args = ['curl.exe','--silent','--show-error','--fail','--location','--compressed',
            '--proto','=https','--proto-redir','=https','--connect-timeout','8','--max-time',str(timeout),
            '--max-filesize','33554432','--dump-header',str(header),'--output',str(output),
            '--user-agent','GitHubTurbo-Benchmark/1','--write-out','%{json}']
        if route in self.sidecars:
            app = self.sidecars[route]
            args += ['--noproxy','*','--connect-to','::127.0.0.1:%s'%app.port,'--cacert',str(app.ca)]
        elif route == 'direct':
            args += ['--noproxy','*']
            for host, ips in self.addresses.items():
                if ips:
                    args += ['--resolve','%s:443:%s'%(host,ips[0])]
        else:
            proxy = '127.0.0.1:7810' if route == 'gitport' else '127.0.0.1:10808'
            args += ['--noproxy','','--proxy','http://'+proxy]
        if 'range' in case:
            args += ['--range',case['range']]
        p = command(args+[case['url']],timeout+4,self.cancel)
        try:
            metrics = json.loads(p['stdout'])
        except ValueError:
            metrics = {}
        headers = final_headers(header)
        reasons,length,actual = validate(case,output,metrics.get('http_code'),headers)
        final_host = urllib.parse.urlsplit(metrics.get('url_effective','')).hostname
        remote_ip = metrics.get('remote_ip')
        local_ca = 'GitHubTurbo' in metrics.get('certs','')
        if route in self.sidecars and not local_ca:
            reasons.append('not_through_expected_local_CA')
        if route == 'direct' and local_ca:
            reasons.append('direct_contaminated_by_turbo')
        if route == 'direct' and remote_ip not in self.addresses.get(final_host,[]):
            reasons.append('direct_dns_bypass_not_confirmed')
        seconds = metrics.get('time_total',p['wall_seconds'])
        result = {'success':p['exit']==0 and not reasons,'exit':p['exit'],
            'timeout':p['timeout'] or p['exit']==28,'errors':reasons,
            'error':re.sub(r'https?://\S+','[URL]',p['stderr'])[:500],
            'code':metrics.get('http_code'),'seconds':seconds,'wall_seconds':p['wall_seconds'],
            'bytes':length,'sha256':actual,'bps':length/max(seconds,.000001),
            'local_ca':local_ca,'headers':headers,
            'timing':{k:metrics.get(k) for k in ('time_namelookup','time_connect','time_appconnect',
                'time_pretransfer','time_starttransfer','time_redirect','num_connects','num_redirects','http_version')},
            'expected_sha256':case.get('sha256'),'verification': ({'github_release_digest':'publisher_sha256',
                'verified_release_prefix':'verified_asset_prefix_sha256'}.get(case.get('digest_source'),'pinned_sha256')) if 'sha256' in case else
                ('range_length_and_bounds_only' if 'range' in case else 'semantic_content')}
        result.update(final_host=final_host,remote_ip=remote_ip)
        if result['success'] and case['id']=='release-full':
            with open(str(output),'rb') as f:
                result['prefix_1m_sha256']=hashlib.sha256(f.read(1048576)).hexdigest()
        # Sensitive redirect headers and large/partial payloads are temporary, not exported.
        for path in (header,output):
            if path.exists():
                path.unlink()
        return result

    def git(self, case, route, tag):
        target = self.dir/(tag+'-repo')
        args = ['git','-c','http.version=HTTP/1.1','-c','credential.helper=',
            '-c','http.lowSpeedLimit=1','-c','http.lowSpeedTime=%s'%self.options['timeout']]
        url = case['url']
        if route in self.sidecars:
            app = self.sidecars[route]
            args += ['-c','http.proxy=','-c','http.sslCAInfo='+str(app.ca),
                '-c','http.curloptResolve=github.com:%s:127.0.0.1'%app.port,
                '-c','http.extraHeader=Host: github.com']
            url = url.replace('github.com/','github.com:%s/'%app.port)
        elif route == 'direct':
            args += ['-c','http.proxy=','-c','http.curloptResolve=github.com:443:'+self.addresses['github.com'][0]]
        else:
            args += ['-c','http.proxy=http://127.0.0.1:'+('7810' if route=='gitport' else '10808')]
        branch=['--branch',case['branch']] if case.get('branch') else []
        clone = command(args+['clone','--depth=1','--no-checkout']+branch+[url,str(target)],self.options['timeout'],self.cancel)
        row = {'success':False,'seconds':clone['wall_seconds'],'wall_seconds':clone['wall_seconds'],
            'exit':clone['exit'],'timeout':clone['timeout'],'bytes':0,'bps':0,'errors':[],
            'error':re.sub(r'https?://\S+','[URL]',clone['stderr'])[-500:],'verification':'pinned_commit_and_git_fsck'}
        if clone['exit']!=0:
            row['errors'].append('git_clone_timeout' if clone['timeout'] else 'git_clone_failed')
        if clone['exit']==0:
            head = command(['git','-C',str(target),'rev-parse','HEAD'],5,self.cancel)
            fsck = command(['git','-C',str(target),'fsck','--no-reflogs'],10,self.cancel)
            fetch = command(args+['-C',str(target),'fetch','--depth=1','origin'],self.options['timeout'],self.cancel)
            row.update(head=head['stdout'].strip(),fetch_seconds=fetch['wall_seconds'],fetch_exit=fetch['exit'],
                fsck_exit=fsck['exit'],
                bytes=sum(p.stat().st_size for p in (target/'.git/objects').rglob('*') if p.is_file()))
            row['bps'] = row['bytes']/max(row['seconds'],.000001)
            row['success'] = row['head']==case['head'] and fsck['exit']==0 and fetch['exit']==0
            if not row['success']:
                row['errors'].append('commit_fsck_or_fetch_failed')
                row['fetch_error']=re.sub(r'https?://\S+','[URL]',fetch['stderr'])[-500:]
                row['timeout']=row['timeout'] or fetch['timeout'] or fsck['timeout']
        objects=target/'.git/objects'
        if objects.is_dir():
            row['bytes']=sum(p.stat().st_size for p in objects.rglob('*') if p.is_file())
            row['bps']=row['bytes']/max(row['seconds'],.000001)
        return row

    def run(self):
        monitor = None
        lease = None
        self.dir.mkdir(parents=True,exist_ok=True)
        self.save()
        try:
            lease = RunLease(WORK/'run.lock')
            self.report['tools'] = {tool:command([tool,'--version'],5,self.cancel)['stdout'].splitlines()[0]
                for tool in ('curl.exe','git')}
            cases = [c for c in CASES if c['id'] in self.options['cases']]
            self.report['manifest']=cases
            self.report['manifest_sha256']=digest(HERE/'cases.json')
            routes = self.options['routes']
            for route in routes:
                if route in ('baseline','candidate'):
                    exe = pathlib.Path(self.options.get('baseline') or BIN/'baseline.exe') if route=='baseline' else pathlib.Path(self.options.get('candidate') or ROOT/'dist/github-turbo.exe')
                    app = Sidecar(route,exe,self.dir/('private-'+route),self.options.get(route+'_options'),self.options.get('fixed_ips'))
                    self.sidecars[route] = app
                    app.start(self.cancel)
                    self.report['routes'][route] = {'exe_sha256':digest(exe),'version':app.initial['version'],
                        'optimization':app.initial.get('optimization'),'upstream':None,'fixed_ips':self.options.get('fixed_ips',{}),'isolation':'private CA, no system integration'}
                elif route == 'gitport':
                    s = local('http://127.0.0.1:7811/api/status')
                    self.report['routes'][route] = {'version':s.get('version'),'mitm':s.get('mitm'),
                        'note':'As currently configured; no settings changed'}
                else:
                    self.report['routes'][route] = {'note':'Independent DoH, first returned public IPv4' if route=='direct' else 'User optional HTTP proxy 127.0.0.1:10808'}
            if 'direct' in routes:
                before = time.monotonic()
                hosts = set(urllib.parse.urlsplit(c['url']).hostname for c in cases)
                if any('/releases/download/' in c['url'] for c in cases):
                    hosts.add('release-assets.githubusercontent.com')
                    hosts.add('objects.githubusercontent.com')
                for host in sorted(hosts):
                    try:
                        self.addresses[host] = resolve(host,self.cancel)
                    except Exception as e:
                        if isinstance(e,Cancelled):
                            raise
                        self.event(str(e))
                        self.addresses[host] = []
                self.report['dns'] = {'seconds':time.monotonic()-before,'addresses':self.addresses,'utc':utc()}
            monitor = Monitor({name:app.p.pid for name,app in self.sidecars.items()})
            monitor.thread.start()
            self.report['state'] = 'running'
            phases = ['cold','warm'] if self.options['phases']=='both' else [self.options['phases']]
            for round_no in range(self.options['rounds']):
                order = route_order(routes,round_no)
                self.report.setdefault('order',[]).append(order)
                for case in cases:
                    for phase in phases:
                        for route in order:
                            if self.cancel.is_set():
                                raise Cancelled()
                            if phase=='cold' and route in self.sidecars:
                                self.sidecars[route].reset()
                            tag = '%s-%s-%s-%s'%(round_no+1,case['id'],phase,route)
                            self.event('测试 '+tag)
                            before = self.sidecars[route].status() if route in self.sidecars else None
                            try:
                                if route=='direct' and not self.addresses.get(urllib.parse.urlsplit(case['url']).hostname):
                                    raise RuntimeError('Independent DNS unavailable; refuse contaminated fallback')
                                if phase=='warm' and self.options['phases']=='warm':
                                    primer = self.git(case,route,tag+'-primer') if case['kind']=='git' else self.http(case,route,tag+'-primer')
                                    self.report.setdefault('primers',[]).append({'tag':tag,'success':primer['success']})
                                result = self.git(case,route,tag) if case['kind']=='git' else self.http(case,route,tag)
                            except Cancelled:
                                raise
                            except Exception as e:
                                result = {'success':False,'seconds':0,'bytes':0,'bps':0,'errors':[str(e)],'timeout':False}
                            result.update(case=case['id'],route=route,phase=phase,round=round_no+1,utc=utc())
                            if before is not None:
                                result['app_before'] = before
                                result['app_after'] = self.sidecars[route].status()
                            with self.lock:
                                self.report['rows'].append(result)
                                self.save()
            self.report['state'] = 'completed'
        except (Cancelled,KeyboardInterrupt):
            self.report['state'] = 'cancelled'
        except Exception as e:
            self.report['state'] = 'failed'
            self.report['error'] = str(e)
        finally:
            if monitor:
                monitor.stop.set()
                monitor.thread.join(timeout=3)
                self.report['resources'] = monitor.peaks
            for app in self.sidecars.values():
                app.close()
            for pattern in ('*.body','*.headers'):
                for temporary in self.dir.glob(pattern):
                    temporary.unlink()
            if lease:
                lease.close()
            self.report['finished'] = utc()
            self.save()
            (self.dir/'report.html').write_text(offline_html(self.report),encoding='utf-8')

def offline_html(report):
    import html
    rows = ''.join('<tr>'+''.join('<td>%s</td>'%html.escape(str(s.get(k,''))) for k in
        ('case','route','phase','successes','attempts','median_seconds','p95_seconds','median_bps'))+'</tr>'
        for s in report['summary'])
    return '<!doctype html><meta charset="utf-8"><title>GitHubTurbo 测速报告</title><style>body{font:16px system-ui;margin:32px;background:#101722;color:#e3edf9}table{border-collapse:collapse}td,th{padding:10px;border:1px solid #40516a}p{max-width:1000px;line-height:1.6}</style><h1>GitHubTurbo 真实网络测速</h1><p>'+html.escape(report['id']+' · '+report['state'])+'</p><p>'+html.escape(report['method'])+'</p><table><tr><th>场景</th><th>路线</th><th>状态</th><th>成功</th><th>尝试</th><th>中位秒</th><th>P95 秒</th><th>字节/秒</th></tr>'+rows+'</table><p>完整配置、逐次结果、失败原因与二进制哈希见相邻 report.json。样本较少时 P95 只是观测分位数。</p>'

ACTIVE = None
ACTIVE_LOCK = threading.Lock()

def options(value):
    routes = value.get('routes',['baseline','candidate'])
    cases = value.get('cases',[c['id'] for c in CASES if c['suite']=='quick'])
    if not isinstance(routes,list) or not routes or len(set(routes))!=len(routes) or any(r not in ('direct','baseline','candidate','gitport','proxy') for r in routes):
        raise ValueError('Invalid routes')
    if not isinstance(cases,list) or not cases or any(c not in [x['id'] for x in CASES] for c in cases):
        raise ValueError('Invalid cases')
    rounds,timeout = int(value.get('rounds',3)),int(value.get('timeout',30))
    phases = value.get('phases','both')
    if not 1<=rounds<=10 or not 5<=timeout<=300 or phases not in ('both','cold','warm'):
        raise ValueError('Rounds 1..10; timeout 5..300; phases cold/warm/both')
    feature = value.get('feature','version')
    if feature not in ('version','probe_reuse','range_memo','range_pipeline','adaptive_split','segmented_downloads'):
        raise ValueError('Unknown experiment')
    fixed_ips=value.get('fixed_ips')
    if fixed_ips is None:
        fixed_ips={}
    allowed={urllib.parse.urlsplit(case['url']).hostname for case in CASES}|{'release-assets.githubusercontent.com','objects.githubusercontent.com'}
    if not isinstance(fixed_ips,dict):
        raise ValueError('fixed_ips must map GitHub hosts to public IP lists')
    for host,addresses in fixed_ips.items():
        if host not in allowed or not isinstance(addresses,list) or not 1<=len(addresses)<=4:
            raise ValueError('Invalid fixed host or address count')
        if any(not ipaddress.ip_address(address).is_global for address in addresses):
            raise ValueError('Pinned addresses must be public')
    if feature != 'version':
        value = dict(value)
        value['baseline'] = value.get('candidate') or str(ROOT/'dist/github-turbo.exe')
        common = {'range_memo':False,'segmented_downloads':True} if feature=='adaptive_split' else {}
        if feature=='range_pipeline':
            common={'range_memo':False,'adaptive_split':False,'segmented_downloads':True}
        if feature=='range_memo':
            common={'segmented_downloads':True}
        value['baseline_options'] = dict(common,**{feature:False})
        value['candidate_options'] = dict(common,**{feature:True})
    return {'routes':routes,'cases':cases,'rounds':rounds,'timeout':timeout,'phases':phases,
        'feature':feature,
        **({'fixed_ips':fixed_ips} if fixed_ips else {}),
        **{k:value[k] for k in ('candidate','baseline','candidate_options','baseline_options') if value.get(k)}}

class Handler(http.server.BaseHTTPRequestHandler):
    def setup(self):
        super().setup()
        self.connection.settimeout(10)

    def log_message(self,*args):
        pass

    def send(self,code,body,kind='application/json; charset=utf-8'):
        raw = json.dumps(body,ensure_ascii=False).encode() if not isinstance(body,bytes) else body
        self.send_response(code)
        self.send_header('Content-Type',kind)
        self.send_header('Content-Length',str(len(raw)))
        self.send_header('Cache-Control','no-store')
        self.send_header('X-Content-Type-Options','nosniff')
        self.send_header('X-Frame-Options','DENY')
        self.end_headers()
        self.wfile.write(raw)

    def safe_host(self):
        return self.headers.get('Host') in ('127.0.0.1:%s'%self.server.server_port,'localhost:%s'%self.server.server_port)

    def do_GET(self):
        if not self.safe_host():
            return self.send(403,{'error':'Loopback Host required'})
        path = urllib.parse.urlsplit(self.path).path
        if path=='/':
            return self.send(200,(HERE/'index.html').read_bytes().replace(b'__TOKEN__',TOKEN.encode()),'text/html; charset=utf-8')
        if path=='/api/info':
            return self.send(200,{'name':'GitHubTurbo Benchmark','cases':CASES,'baseline':(BIN/'baseline.exe').exists(),'candidate':(ROOT/'dist/github-turbo.exe').exists(),
                'available':{'gitport':listening(7810) and listening(7811),'proxy':listening(10808)}})
        if path=='/api/runs':
            runs = []
            for file in sorted(RUNS.glob('*/report.json'),reverse=True)[:100]:
                try:
                    doc = json.loads(file.read_text(encoding='utf-8'))
                    runs.append({k:doc.get(k) for k in ('id','state','started','options')})
                except (OSError,ValueError):
                    pass
            return self.send(200,runs)
        if path=='/api/current':
            with ACTIVE_LOCK:
                if ACTIVE and ACTIVE.report['state'] in ('preparing','running'):
                    return self.send(200,ACTIVE.snapshot())
            files = sorted(RUNS.glob('*/report.json'),reverse=True)
            return self.send(200,json.loads(files[0].read_text(encoding='utf-8')) if files else None)
        match = re.fullmatch(r'/(api/report|report)/([0-9a-z-]+)(?:/(report\.json|report\.html))?',path)
        if match:
            run_id = match[2]
            suffix = match[3] or 'report.json'
            file = RUNS/run_id/suffix
            if file.is_file():
                return self.send(200,file.read_bytes(),'text/html; charset=utf-8' if suffix.endswith('.html') else 'application/json; charset=utf-8')
        return self.send(404,{'error':'Not found'})

    def do_POST(self):
        global ACTIVE
        try:
            length=int(self.headers.get('Content-Length','0'))
            if length<0 or length>16384:
                return self.send(413,{'error':'Body too large'})
            payload=self.rfile.read(length)
        except (ValueError,OSError):
            return self.send(400,{'error':'Invalid or incomplete body'})
        origin = self.headers.get('Origin')
        if not self.safe_host() or self.headers.get('X-Benchmark-Token')!=TOKEN or (origin and origin not in
                ('http://127.0.0.1:%s'%self.server.server_port,'http://localhost:%s'%self.server.server_port)):
            return self.send(403,{'error':'Local page token and origin required'})
        try:
            value = json.loads(payload or b'{}')
            with ACTIVE_LOCK:
                if self.path=='/api/stop':
                    if ACTIVE and ACTIVE.report['state'] in ('preparing','running'):
                        ACTIVE.cancel.set()
                    else:
                        files = sorted(RUNS.glob('*/report.json'),reverse=True)
                        if files and json.loads(files[0].read_text(encoding='utf-8'))['state'] in ('preparing','running'):
                            (files[0].parent/'cancel.request').touch()
                    return self.send(200,{'ok':True})
                if self.path=='/api/run':
                    if ACTIVE and ACTIVE.report['state'] in ('running','preparing'):
                        return self.send(409,{'error':'A benchmark is already running'})
                    for file in RUNS.glob('*/report.json'):
                        doc = json.loads(file.read_text(encoding='utf-8'))
                        if doc.get('state') in ('preparing','running') and process_alive(doc.get('pid')):
                            return self.send(409,{'error':'A CLI benchmark is already running: '+doc['id']})
                    if any(k in value for k in ('candidate','baseline','candidate_options','baseline_options')):
                        raise ValueError('Web UI uses the built distribution only')
                    ACTIVE = Runner(options(value))
                    threading.Thread(target=ACTIVE.run,daemon=True).start()
                    return self.send(200,{'id':ACTIVE.id})
        except (ValueError,TypeError,KeyError) as e:
            return self.send(400,{'error':str(e)})
        return self.send(404,{'error':'Not found'})

class LoopbackServer(http.server.ThreadingHTTPServer):
    # Windows SO_REUSEADDR permits a second listener, which would split page/API
    # requests between different tokens. Reserve this loopback endpoint exclusively.
    allow_reuse_address = False
    def server_bind(self):
        if os.name=='nt':
            self.socket.setsockopt(socket.SOL_SOCKET,socket.SO_EXCLUSIVEADDRUSE,1)
        super().server_bind()

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='action',required=True)
    serve = sub.add_parser('serve'); serve.add_argument('--port',type=int,default=18190); serve.add_argument('--open',action='store_true')
    freeze = sub.add_parser('freeze'); freeze.add_argument('--exe',default=str(ROOT/'dist/github-turbo.exe'))
    run = sub.add_parser('run')
    run.add_argument('--routes',default='baseline,candidate')
    run.add_argument('--cases',default='raw,css,release-small')
    run.add_argument('--rounds',type=int,default=3)
    run.add_argument('--timeout',type=int,default=30)
    run.add_argument('--phases',default='both')
    run.add_argument('--candidate')
    run.add_argument('--baseline')
    run.add_argument('--candidate-options',type=json.loads)
    run.add_argument('--baseline-options',type=json.loads)
    run.add_argument('--fixed-ips',type=json.loads,help='Controlled comparison: same public IP mapping on both Turbo instances')
    run.add_argument('--feature',default='version',choices=['version','probe_reuse','range_memo','range_pipeline','adaptive_split','segmented_downloads'])
    run.add_argument('--require-success',action='store_true')
    args = parser.parse_args()
    if args.action=='freeze':
        BIN.mkdir(parents=True,exist_ok=True)
        if (BIN/'baseline.exe').exists():
            raise SystemExit('Baseline already exists; keep it immutable or archive it manually.')
        shutil.copy2(args.exe,str(BIN/'baseline.exe'))
        shutil.copy2(str(ROOT/'config.json'),str(BIN/'baseline.config.json'))
        write_json(BIN/'baseline.json',{'utc':utc(),'sha256':digest(BIN/'baseline.exe')})
        print('Frozen baseline: '+digest(BIN/'baseline.exe'))
    elif args.action=='serve':
        try:
            server = LoopbackServer(('127.0.0.1',args.port),Handler)
        except OSError:
            info=local('http://127.0.0.1:%s/api/info'%args.port)
            if info.get('name')!='GitHubTurbo Benchmark':
                raise
            if args.open:
                import webbrowser
                webbrowser.open('http://127.0.0.1:%s/'%args.port)
            print('Benchmark already running at http://127.0.0.1:%s/'%args.port)
            return
        print('GitHubTurbo Benchmark http://127.0.0.1:%s/'%args.port,flush=True)
        if args.open:
            import webbrowser
            webbrowser.open('http://127.0.0.1:%s/'%args.port)
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            if ACTIVE:
                ACTIVE.cancel.set()
    else:
        value = vars(args)
        value['routes'],value['cases'] = args.routes.split(','),args.cases.split(',')
        runner = Runner(options(value))
        print('Run '+runner.id,flush=True)
        try:
            runner.run()
        except KeyboardInterrupt:
            runner.cancel.set()
        print(str(runner.dir/'report.html'))
        print(json.dumps({'state':runner.report['state'],'paired':runner.report['paired'],'error':runner.report.get('error')},ensure_ascii=False))
        if runner.report['state']!='completed':
            raise SystemExit(1)
        if args.require_success and any(not r['success'] for r in runner.report['rows']):
            raise SystemExit(2)

if __name__=='__main__':
    main()
