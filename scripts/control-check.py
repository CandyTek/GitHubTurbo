"""Black-box checks for the running loopback management API; restores options."""
import json
from concurrent.futures import ThreadPoolExecutor
import pathlib
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[1]
BASE = 'http://127.0.0.1:18080'
client = urllib.request.build_opener(urllib.request.ProxyHandler({}))

def request(path, method='GET', headers=None, data=None):
    req = urllib.request.Request(BASE + path, method=method, headers=headers or {}, data=data)
    try:
        with client.open(req, timeout=8) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as response:
        return response.code, response.read()

def status():
    code, body = request('/status')
    assert code == 200
    return json.loads(body)

before = status()['optimization']
checks = {}
assert request('/status', headers={'Host': 'evil.example:18080'})[0] == 403
checks['foreign_host'] = 'rejected'
assert request('/api/reset', 'POST')[0] == 403
checks['missing_marker'] = 'rejected'
marker = {'X-GitHub-Turbo-Request': '1', 'Origin': BASE}
assert request('/api/options?http2=false', 'POST', {**marker, 'Origin': 'https://evil.example'})[0] == 403
checks['foreign_origin'] = 'rejected'
assert request('/api/options?http2=invalid', 'POST', marker)[0] == 400
assert status()['optimization'] == before
checks['invalid_option'] = 'rejected_without_mutation'
try:
    assert request('/api/options?http2=true&public_cache=true', 'POST', marker)[0] == 200
    with ThreadPoolExecutor(max_workers=2) as executor:
        jobs = [executor.submit(request, '/api/options?' + key + '=false', 'POST', marker)
                for key in ['http2', 'public_cache']]
        assert all(job.result()[0] == 200 for job in jobs)
    changed = status()['optimization']
    assert not changed['http2'] and not changed['public_cache']
    assert json.loads((ROOT/'dist/config.json').read_text(encoding='utf-8-sig'))['optimization'] == changed
    checks['live_and_persisted'] = 'passed'
    checks['concurrent_updates_preserved'] = 'passed'
finally:
    query = '&'.join(f'{key}={str(value).lower()}' for key, value in before.items())
    assert request('/api/options?' + query, 'POST', marker)[0] == 200
    assert status()['optimization'] == before
assert request('/api/cache/clear', 'POST', marker)[0] == 200
code, body = request('/api/diagnostics')
assert code == 200 and json.loads(body)['version'] == '0.3.0'
checks['options_restored'] = 'passed'
(ROOT/'reports/control-0.3.json').write_text(json.dumps(checks, indent=2), encoding='utf-8')
print(json.dumps(checks))
