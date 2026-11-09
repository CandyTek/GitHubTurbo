"""Start/cancel an owned live run and verify isolation; does not stop someone else's run."""
import hashlib
import json
import os
import pathlib
import re
import time
import urllib.error
import urllib.request

ROOT=pathlib.Path(__file__).resolve().parents[1]
BASE='http://127.0.0.1:18190'
client=urllib.request.build_opener(urllib.request.ProxyHandler({}))
def get(path):
    with client.open(BASE+path,timeout=5) as r:return json.load(r)
active=get('/api/current')
if active and active['state'] in ('preparing','running'):
    raise SystemExit('A benchmark is already active; leave it undisturbed.')
with client.open(BASE+'/',timeout=5) as r:
    token=re.search(r"const token='([0-9a-f]+)'",r.read().decode()).group(1)
def post(path,value):
    request=urllib.request.Request(BASE+path,data=json.dumps(value).encode(),method='POST',
        headers={'X-Benchmark-Token':token,'Origin':BASE,'Content-Type':'application/json'})
    with client.open(request,timeout=5) as r:return json.load(r)
hosts=pathlib.Path(os.environ['SystemRoot'])/'System32/drivers/etc/hosts'
before=hashlib.sha256(hosts.read_bytes()).hexdigest()
payload={'routes':['baseline','candidate'],'cases':['release-full'],'rounds':10,'phases':'cold','timeout':30}
run_id=post('/api/run',payload)['id']
try:
    post('/api/run',payload)
    raise AssertionError('Concurrent request was accepted')
except urllib.error.HTTPError as e:
    assert e.code==409
for _ in range(100):
    status=get('/api/current')
    assert status['id']==run_id
    if status['events']:break
    if status['state']=='failed':raise RuntimeError(status.get('error'))
    time.sleep(.1)
assert status['events'], 'No transfer started'
post('/api/stop',{})
for _ in range(100):
    status=get('/api/current')
    if status['state']=='cancelled' and status.get('finished'):break
    time.sleep(.1)
assert status['state']=='cancelled' and status.get('finished')
folder=ROOT/'bench/runs'/run_id
assert not list(folder.glob('*.body')) and not list(folder.glob('*.headers'))
assert hashlib.sha256(hosts.read_bytes()).hexdigest()==before
report={'run_id':run_id,'start_stop':'passed','concurrent_run':'rejected',
    'temporary_payloads_removed':True,'hosts_unchanged':True,'state':status['state']}
(ROOT/('reports/bench-api-'+re.search(r'^version = "([^"]+)"',(ROOT/'Cargo.toml').read_text(),re.M).group(1)+'.json')).write_text(json.dumps(report,indent=2),encoding='utf-8')
print(json.dumps(report))
