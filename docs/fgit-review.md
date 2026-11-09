# Review of fgit

A review of [fgit](https://github.com/fastgh/fgit) (v1.2.0, Go, GPL-3.0), made on
2026-11-09. The whole client is about 900 lines; all of it was read. No code was
copied: the one idea adopted was reimplemented in Rust.

## How fgit works

fgit is a wrapper you call instead of `git`. For `clone`, `pull`, `fetch` and
`push` on `https://github.com/...` it:

1. Registers an account with its control server (`control.fgit.kailash.cloud`) and
   logs in for a short-lived token.
2. Fetches a list of mirror servers and relay (HTTP CONNECT) proxy servers from the
   control server and picks one of each **at random**.
3. Public read (clone/pull/fetch without a user name in the URL): rewrites the
   remote URL to the mirror (`git remote set-url`, or the clone argument).
   Push, or a URL with a user name: writes `http.https://github.com.proxy` into
   the global or local git config, pointing at the relay with the token.
4. Runs git, then restores the remote URL or proxy setting, also on Ctrl+C or panic.

The speed comes from the operators' servers having good international routes.
Nothing in the client measures or selects for speed.

## Comparison

| fgit feature | GitHubTurbo | Decision |
| --- | --- | --- |
| Per-command drop-in, no admin rights, no lasting system change | Needed `enable` (administrator, NRPT, local CA) or a manual proxy setting | **Adopted** as `github-turbo git <args>` |
| github.com only, other remotes untouched | Same, by domain allow-list | Already covered |
| Push and private repos through a relay that cannot decrypt | Engine relays or re-encrypts locally; nothing leaves the machine except to GitHub | Already covered, without a third party |
| Restore settings after crash or Ctrl+C | Session file, watchdog, recovery task | Already covered; the new wrapper writes no settings at all |
| Server list fetched at run time | `host_sources`, hourly, validated as GitHub addresses | Already covered |
| Public mirror for anonymous clones | Not used | **Rejected**: the mirror sees and can alter repository content (refs are not signed), and it is a third party |
| Operator relay servers with account and token | Optional `upstream` you control | **Rejected**: depends on a third-party service, now unreachable |
| Random server choice | Measured latency, throughput and failure history | Ours is stronger |
| `GIT_SSL_NO_VERIFY=1` on the git process | Never | **Rejected**: disables certificate checks. fgit also replaces git's whole environment with that one variable, dropping `PATH`, `HOME` and credentials settings |
| Rewrites `.git/config` or global `.gitconfig` during the command | Not done | **Rejected**: a crash can leave them changed; per-invocation `-c` avoids this |
| Option allow-list; refuses `--recurse-submodules` | Arguments passed through unchanged | Not needed: `-c` is inherited by child git processes |

## What was added: `github-turbo git`

```powershell
github-turbo.exe git clone https://github.com/OWNER/REPO.git
github-turbo.exe git push origin main
```

- If a system-wide GitHubTurbo is already running (transparent mode), git runs
  unchanged, because it is already accelerated.
- Otherwise a private engine starts on a random loopback port in explicit-proxy
  mode, and git runs as `git -c http.https://github.com/.proxy=http://127.0.0.1:<port> <args>`.
  The engine picks the GitHub address the same way the system engine does (DoH,
  probing, cached good routes from `state/`) and relays TLS bytes unchanged. git
  still verifies GitHub's real certificate, and credentials stay end to end.
- Nothing is written to git config, `hosts`, NRPT or the certificate store.
  Ctrl+C is left to git; git's exit code is returned. Engine notes are hidden
  unless `GITHUB_TURBO_DEBUG` is set.
- `config.json` beside the executable is used when present, otherwise the built-in
  defaults. An `upstream` proxy configured there is used as usual.

Limits: only `https://github.com/` URLs go through the engine. SSH remotes and Git
LFS object downloads (served from other hosts) are not affected. Explicit-proxy
mode relays opaque TLS, so the reverse-proxy features (HTTP/2 pooling, `git_race`,
flow rescue, caching) do not apply; the gain comes from route selection alone.

## Measurements

Shallow clone (`--depth=1`) of `curl/curl`, alternating plain `git` and
`github-turbo git`, 180 s limit, same machine and network, 2026-11-09. Plain git
used the system resolver (a static GitHub520 `hosts` entry was present).

| Pair | Plain git (s) | `github-turbo git` (s) |
| --- | ---: | ---: |
| 1 | 44.9 | 41.5 |
| 2 | failed (`early EOF`, then timed out) | 54.8 |
| 3 | 55.8 | 4.4 |
| 4 | 50.0 | 80.6 |

The wrapper finished all four clones and plain git finished three. Times varied
from 4 s to 81 s on both sides, so four pairs show no reliable speed difference.
The case for the command is that it brings GitHubTurbo's route choice to one git
command without administrator rights or system changes, not a measured speedup.
Overhead on a tiny request: `git ls-remote` of `octocat/Hello-World` took 2.1 s
in total, including starting the engine and choosing a route.
