"""Bounded, sequential network comparisons. Python 3.8+, curl; no extra packages."""
import argparse
import datetime
import hashlib
import json
import pathlib
import subprocess
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
URLS = [
    ("web", "https://github.com/"),
    ("api", "https://api.github.com/repos/octocat/Hello-World"),
    ("raw", "https://raw.githubusercontent.com/octocat/Hello-World/master/README"),
    ("archive", "https://codeload.github.com/octocat/Hello-World/zip/refs/heads/master"),
    ("release", "https://github.com/cli/cli/releases/download/v2.103.3/gh_2.103.3_checksums.txt"),
]

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rounds", type=int, default=1)
    parser.add_argument("--turbo", default="http://127.0.0.1:18080")
    parser.add_argument("--upstream", default="http://127.0.0.1:10808")
    args = parser.parse_args()
    report = {"utc": datetime.datetime.now(datetime.timezone.utc).isoformat(), "note": "Sequential samples, not a controlled throughput benchmark. SHA256 covers received content; compare only successful immutable downloads.", "results": []}
    routes = [("direct", None), ("upstream", args.upstream), ("turbo", args.turbo)]
    with tempfile.TemporaryDirectory(prefix="github-turbo-bench-") as folder:
        body = pathlib.Path(folder) / "body.bin"
        for round_no in range(args.rounds):
            for name, url in URLS:
                # Rotate order to reduce consistently favoring a warm upstream cache.
                order = routes[round_no % 3:] + routes[:round_no % 3]
                for route, proxy in order:
                    if body.exists():
                        body.unlink()
                    cmd = ["curl.exe", "--silent", "--show-error", "--fail", "--location", "--proto", "=https", "--proto-redir", "=https", "--connect-timeout", "10", "--max-time", "20", "--max-filesize", "4194304", "--output", str(body), "--write-out", "%{json}", "--user-agent", "GitHubTurbo-benchmark/0.1"]
                    cmd += ["--noproxy", ""] + ["--proxy", proxy] if proxy else ["--noproxy", "*"]
                    result = subprocess.run(cmd + [url], capture_output=True, text=True, timeout=25)
                    try:
                        metrics = json.loads(result.stdout)
                    except ValueError:
                        metrics = {}
                    row = {"round": round_no + 1, "case": name, "route": route, "exit": result.returncode, "error": result.stderr.strip(), **{key: metrics.get(key) for key in ["http_code", "time_namelookup", "time_connect", "time_appconnect", "time_starttransfer", "time_total", "size_download", "speed_download", "num_redirects"]}}
                    if body.exists():
                        row["sha256"] = hashlib.sha256(body.read_bytes()).hexdigest()
                    report["results"].append(row)
                    print(json.dumps(row), flush=True)
    destination = ROOT / "reports" / "network.json"
    destination.parent.mkdir(exist_ok=True)
    destination.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print("Saved:", destination)

if __name__ == "__main__":
    main()
