# GitHubTurbo

A lightweight prototype designed to accelerate access to GitHub on Windows. It listens on `127.0.0.1:18080` by default and leverages an existing HTTP proxy at `127.0.0.1:10808`. The project is built using Rust and Tokio, utilizing the Windows SChannel API for TLS probing.

It functions as an **HTTP CONNECT forward proxy** specifically for GitHub. Browsers and Git clients connect to the original domain names and perform their own verification of GitHub’s TLS certificates. Since a full GitHub web session involves logins, cookies, Content Security Policy (CSP) headers, redirects, and various resource domains, a simple localhost-based reverse proxy mapping is not the primary approach here.

## Usage

Run the following command in PowerShell:

```powershell
& 'I:\RcProject\GitHubTurbo\dist\github-turbo.exe'
```

The program looks for a `config.json` file located next to the executable; if not found, it checks the current working directory. If an instance is already running on port 18080, there is no need to start another. Press `Ctrl+C` to stop a foreground instance. The program does not modify system proxy settings, the `hosts` file, root certificates, global Git configurations, or startup items.

To start or stop the program in the background, use `scripts/start.ps1` or `scripts/stop.ps1`. Stopping the program will immediately terminate any active tunnels.

Status page: <http://127.0.0.1:18080/status>.

For browser proxy extensions or system Proxy Auto-Configuration (PAC) scripts, you can use: <http://127.0.0.1:18080/proxy.pac>. The PAC file routes only the specified GitHub domains through this proxy, while all other traffic connects directly. If you already have other proxy rules in place, you should merge these GitHub-specific rules into your existing configuration. The PAC file is not installed automatically. Note that if you are already using an Xray TUN or a transparent proxy, "direct" connections may still be routed through those systems. Single Git operation:

```powershell
git -c http.https://github.com/.proxy=http://127.0.0.1:18080 ls-remote https://github.com/octocat/Hello-World.git HEAD
git -c http.https://github.com/.proxy=http://127.0.0.1:18080 clone --depth=1 https://github.com/octocat/Hello-World.git
```

Alternatively, you can use `scripts/git-turbo.ps1 clone <URL>`. For large repositories, consider using `--filter=blob:none`, `--sparse`, or `--depth=1` based on your specific needs; these options alter local history or file availability and are not enabled by default.

Downloading Releases / Raw files:

```powershell
& 'I:\RcProject\GitHubTurbo\scripts\download.ps1' -Url 'https://raw.githubusercontent.com/octocat/Hello-World/master/README' -Output '.\README.download'
# Optional: Use local aria2 for multi-threaded (4-segment) downloading and resuming
& 'I:\RcProject\GitHubTurbo\scripts\download.ps1' -Url '<actual Release HTTPS URL>' -Output '.\package.zip' -Aria2 'D:\DengYuhua\Aria2\aria2c.exe'
```

The download script maintains certificate verification; for large files, you should also verify the SHA256 hash provided by the publisher. Multi-segment downloading may improve speeds only if the server supports the `Range` header and there is a throughput bottleneck in your network connection. `aria2` is an optional external tool; the main program does not depend on it.

## Implemented Strategies

| Aspect | Behavior |
| --- | --- |
| Domain Routing | GitHub, API, Raw, avatars, static assets, `codeload`, Release CDN, and specified legacy S3 domains are routed; all other destinations and non-443 ports are rejected. |
| Automatic Path Selection | Up to 8 public IP candidates (resolved via system DNS) are probed in parallel alongside the HTTP upstream. Selection is based on successful TLS certificate validation and HTTP HEAD responses. |
| Default Local Policy | `github.com` is pinned to the proxy; other permitted domains default to `auto`. This follows observations that web page content loads more slowly via proxy, while API/`codeload` requests are faster via direct connection. |
| DNS Failures | For proxied routes, domain resolution is delegated to the upstream. For direct connections, `ip_overrides` can be configured for specific domains, while SNI and certificate validation are still performed using the original domain name. |
| Reconnection & Fallback | In `auto` mode, if a connection attempt fails before receiving a `CONNECT 200` response, the system may attempt an alternative route that has passed the probing phase. |
| Resource Limits | 2 Tokio worker threads, a default limit of 64 concurrent connections, 4 parallel "cold" probes, 128 cache entries, and two 16 KiB copy buffers per tunnel. |
| Streaming | Content bodies are not cached; full-duplex streaming and half-close states are supported. Idle timers are reset during data transfer, with no hard limit on the total duration of a download. |
| Observability | Local JSON status, byte-count statistics, routing logs, diagnostic commands, and reproducible comparison scripts. |

`host_modes` relies on exact hostname matching and does not apply to subdomains. Supported modes include `auto`, `proxy`, and `direct`. The global `mode` setting applies only to domains without an explicit override; to enforce a single global mode, clear the `host_modes` configuration. Changes take effect after a restart.

## Diagnostics and Verification

```powershell
& 'I:\RcProject\GitHubTurbo\dist\github-turbo.exe' diagnose
python 'I:\RcProject\GitHubTurbo\scripts\benchmark.py'
& 'I:\RcProject\GitHubTurbo\scripts\dns-diagnose.ps1'
```

The `diagnose` command actively tests both direct and proxied connections to six specific domains. Note that a 4xx HTTP status code from a CDN root path only confirms that the HTTPS endpoint is reachable; it does not verify the existence of any specific file. The `benchmark.py` script performs actual GET requests for web pages, APIs, raw files, source code archives, and official release assets, recording the time taken, bytes downloaded, exit codes, and content hashes. It utilizes the system's `curl` utility, with timeouts and a 4 MiB size limit applied to each request. Tests are executed sequentially and do not constitute a controlled bandwidth benchmark. By default, this requires a local service to be running.

The DNS diagnostic script compares system DNS resolution against Cloudflare DNS-over-HTTPS (DoH) accessed via your existing proxy. It does not modify your system DNS settings or `hosts` file, nor does it permanently hardcode IP addresses based on a single snapshot. If you need to test `ip_overrides`, select candidate addresses from the diagnostic results and update them as needed; avoid long-term hardcoding of potentially outdated CDN IP addresses.

## Build

Verified environment: Rust/Cargo 1.96.0, MSVC x64 toolchain, Windows. No Rust, Python, Node, or Go runtime is required for execution. Dependencies are locked via `Cargo.lock`; the primary direct dependencies are `tokio`, `tokio-native-tls`, `serde`, and `serde_json`. On Windows, no external OpenSSL installation is required; builds on other platforms may require a system-installed OpenSSL.

```powershell
cd I:\RcProject\GitHubTurbo
# If a proxy is required to download crates, set it for the current terminal session only
$env:HTTPS_PROXY = 'http://127.0.0.1:10808'
& .\scripts\build.ps1
cargo clippy --all-targets --locked -- -D warnings
python .\scripts\integration.py
```

The build script preserves any existing `dist/config.json` file; the default configuration is copied only during the initial build.

## Prototype Limitations

- HEAD request latency is not equivalent to throughput for large files; the automatic mode might select a route with a fast handshake but slow data transfer. You can use `host_modes` to pin a specific domain and then re-test the full download. The core program does not claim to increase the egress bandwidth of any existing proxy you may already be using.
- Route probing uses independent connections, meaning the actual tunnel will be re-established; the first request after a cache entry expires incurs additional overhead. Established TLS streams cannot be seamlessly switched to a different route, nor will the system automatically replay Git push operations or login requests.
- Re-testing occurs upon the next access after a cache entry expires; there is currently no background continuous speed testing, throughput learning, automatic DoH cache refreshing, or support for a pool of multiple upstream nodes.
- The upstream only supports unauthenticated local HTTP CONNECT; the software does not natively provide SOCKS, TUN, UDP/QUIC, SSH, a GUI, or a Windows service installer. Browsers use this proxy for TCP-based HTTPS; the HTTPS tunnel can carry HTTP/2 traffic as negotiated by the client.
- Plain-text HTTP requests will return a 405 error; please use HTTPS URLs. Requests to domains not explicitly listed—such as those used by Git LFS, package managers, or third-party storage services—will be rejected; end-to-end validation for these specific use cases has not yet been completed.
- TLS is handled end-to-end by the client, so no HTTPS payload caching is provided. Disk caching for public, immutable resources should be implemented via a separate download service; content containing cookies or authorization headers should not be stored in a public cache.
- If an existing local proxy serves as the source for cross-region connectivity, and both a direct connection and that proxy are unavailable, the local forwarding mechanism cannot independently create a functional egress path.

For a comprehensive overview of design choices, dependencies, and reference materials, please refer to `docs/research.md`; for empirical test results, see `docs/validation.md`.