"""Download one bounded official asset, compare publisher digest, record split use."""
import hashlib
import json
import pathlib
import subprocess

ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT = ROOT/'reports'
metadata = json.loads((OUT/'cli-release-metadata.json').read_text(encoding='utf-8-sig'))
asset = next(a for a in metadata['assets'] if a['name'] == 'gh_2.103.3_linux_arm64.tar.gz')
assert 4*1024*1024 <= asset['size'] <= 32*1024*1024 and asset['digest'].startswith('sha256:')
output = OUT/'release-0.3.tar.gz'
headers = OUT/'release-0.3.headers'
p = subprocess.run(['curl.exe', '--noproxy', '*', '--silent', '--show-error', '--fail', '--location',
    '--proto', '=https', '--proto-redir', '=https', '--max-time', '120', '--max-filesize', '33554432',
    '--dump-header', str(headers), '--output', str(output), '--write-out', '%{json}',
    asset['browser_download_url']], capture_output=True, text=True, timeout=125)
try:
    metrics = json.loads(p.stdout)
except ValueError:
    metrics = {}
digest = hashlib.sha256(output.read_bytes()).hexdigest() if p.returncode == 0 else None
report = {'name': asset['name'], 'exit': p.returncode, 'error': p.stderr.strip(),
    'http_code': metrics.get('http_code'), 'seconds': metrics.get('time_total'),
    'bytes': metrics.get('size_download'), 'sha256': digest, 'publisher_digest': asset['digest'],
    'digest_matches': digest == asset['digest'][7:],
    'local_ca': 'GitHubTurbo' in metrics.get('certs', ''),
    'segmented': b'x-github-turbo-route: direct-split' in headers.read_bytes().lower()}
(OUT/'release-0.3.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
print(json.dumps(report), flush=True)
assert report['exit'] == 0 and report['digest_matches'] and report['bytes'] == asset['size']
