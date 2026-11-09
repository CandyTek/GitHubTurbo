# Selecting a Local Acceleration Solution for GitHub

Research date: 2026-11-08. Target environment: Windows x64, with an existing HTTP egress point at `127.0.0.1:10808`. The current directory is initially empty, containing no existing Git repository or prior implementation.

## Comparison of Tools

The resource usage estimates provided below are intended for architectural selection purposes; they do not represent benchmark rankings based on side-by-side testing. Where data was not measured, the corresponding memory usage figures are left blank.

| Solution | Use Case & Dependencies | Assessment for This Project |
| --- | --- | --- |
| [3proxy](https://github.com/3proxy/3proxy) | C-based implementation, Windows binaries; supports HTTP CONNECT, SOCKS, SNI forwarding, DNS caching, and upstream proxy chaining. | A top candidate for a minimalist, ready-to-use core; requires the development of a health-check mechanism and a domain-based routing policy controller. Attempting to retrieve the official 1.0.3 x64 package resulted in a 504 error; no in-situ memory usage or throughput benchmarks have been conducted yet. |
| [GOST v3](https://github.com/go-gost/gost) | A standalone Go-based application; supports proxy chaining, forwarding, DNS, routing, and multiple protocols. Includes a [selector](https://gost.run/en/concepts/selector/) feature that supports both fallback and competitive (race) strategies. | A strong candidate when rapid reuse of mature functionality is required. However, its feature set exceeds current needs, and it cannot be claimed to have a lower memory footprint than other options without empirical testing. |
| [Caddy](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy) | Written in Go; its reverse proxy supports connection pooling, upstream TLS, timeouts, and network proxy configurations. | Well-suited for reverse-proxying standalone public file downloads. However, handling the full GitHub site—which involves cross-origin requests and authentication—would require more than just a simple `upstream` configuration change to ensure a proper user experience. |
| [Nginx (Windows)](https://nginx.org/en/docs/windows.html) | Written in C; a mature HTTP reverse proxy. The official Windows documentation labels it as "beta," and it relies on `select`/`poll` mechanisms, which implies limitations on scalability. | An older version was found locally; however, one should not assume that its performance on Linux will translate to Windows. I have not launched the existing installation. |
| [Pingora](https://github.com/cloudflare/pingora#system-requirements) | A Rust-based proxy framework; primarily designed for Linux, with only preliminary community support for Windows. | Not an ideal choice for this initial Windows prototype. |
| Custom core (Rust + Tokio) | Utilizes Tokio for asynchronous I/O, `serde` for configuration, and system TLS (exclusively for health checks); no additional daemon processes required. | The chosen approach for this implementation. It offers a limited scope, making it easy to enforce strict concurrency and cache capacity caps; long-term maintenance costs will be managed as part of the project. |

In the table above, "reverse proxy" and "proxy chaining" serve different purposes. For full GitHub access (via browser) and Git HTTPS operations, a `CONNECT` tunnel using the original domain is employed to preserve authentication and TLS semantics. For public, static-URL download caching, a Caddy-based download gateway could be integrated at a later stage.

## Local Environment and Dependencies

Development tools and related binaries were located using the user-provided Everything CLI; no private documents were scanned. For precise filename matching, the `wfn:` prefix is ​​used—for example:

```powershell
& 'D:\Download3\ES-1.1.0.30.x64\es.exe' /a-d -n 20 'wfn:aria2c.exe'
```

- Rust/Cargo 1.96.0: `C:\Users\Administrator\.cargo\bin`.
- Visual Studio 2022 Community: `E:\Microsoft Visual Studio\2022\Community` (MSVC builds verified).
- Git 2.45.1.windows.1; Windows curl 8.13.0 (Schannel); Python available for test scripts.
- Port 10808 is being listened on by Xray: `D:\DengYuhua\v2rayN-windows-64\bin\xray\xray.exe`.
- aria2: `D:\DengYuhua\Aria2\aria2c.exe` (an additional copy exists within Aria2Manager).
- Nginx: `E:\nginx 1.23.3.3\nginx.exe` (an additional copy, version 1.16.0, is also present).
- No executables for Go, 3proxy, Caddy, or GOST were found. This does not definitively rule out the existence of unindexed versions elsewhere on the disk.

The main program has direct dependencies on: Tokio 1.x, tokio-native-tls 0.3, serde 1, and serde_json 1. Specific versions are locked in `Cargo.lock`; for this build, these are Tokio 1.54.1, native-tls 0.2.18, serde 1.0.229, and serde_json 1.0.154. The lock file also includes conditional dependencies for other platforms; OpenSSL is not required for the Windows build. According to the [official `native-tls` documentation](https://docs.rs/native-tls/latest/native_tls/), the Windows backend utilizes SChannel. [tokio-native-tls](https://docs.rs/tokio-native-tls/latest/tokio_native_tls/) provides an asynchronous wrapper and performs hostname verification by default. The application directory requires only the executable and its configuration files; the main program has no dependencies on Python, Node.js, or Go runtimes. Please review the licenses of both direct and transitive dependencies before selecting a distribution license for your project; no specific distribution license has been assigned to the project in this iteration.

## Multi-Layer Acceleration Strategy and Progress

| Layer | Completed in This Iteration | Future Scope |
| --- | --- | --- |
| DNS | Proxy-side resolution; scripts to compare system DNS with DoH; optional IP overrides while maintaining SNI/certificate verification | Built-in DoH, TTL caching, health checks for multiple resolvers |
| Connection | Direct connection candidates competing with existing HTTP proxies; TLS+HEAD probing, domain-based patterns, TTL caching, and fallback on connection failure | Scoring based on throughput and failure rates, latency control, multi-node pools |
| Web | Precise traffic routing via PAC; coverage for resource domains; preservation of original site logins and TLS | Automated browser-based waterfall loading, login state persistence, and verification of additional GitHub features |
| Git | Proxy scripts for one-off Git operations; recommendations to minimize cloned data based on specific use cases | Git LFS support, private repositories, `push` operations, and long-term testing with large repositories |
| Downloads | Redirect tunneling, streaming, `curl` resume support, and optional 4-way segmented downloads via `aria2` | Disk caching for immutable public files, checksum verification, and quota management |
| SSH | Support for GitHub’s official `ssh.github.com:443` fallback entry point | Integration via standalone SSH `ProxyCommand`; current proxy does not automatically forward SSH traffic |

The [Git clone documentation](https://git-scm.com/docs/git-clone) covers options for shallow clones, partial clones, and sparse checkouts; the appropriate method should be selected based on whether the user requires the full repository history. The [GitHub SSH documentation](https://docs.github.com/en/authentication/troubleshooting-ssh/using-ssh-over-the-https-port) mentions `ssh.github.com:443`, which is not equivalent to standard HTTPS; therefore, conventional health checks are not applicable to it.

The [aria2 documentation](https://aria2.github.io/manual/en/html/aria2c.html) covers options for segmentation, concurrency, resuming downloads, and verification; however, increasing the number of connections does not guarantee faster speeds. The [Cloudflare DoH documentation](https://developers.cloudflare.com/1.1.1.1/encryption/dns-over-https/make-api-requests/dns-json/) details JSON-based GET queries and TTL (Time-to-Live) values; the script retains these TTL values ​​for monitoring purposes without permanently altering the system's DNS resolution.

No third-party public GitHub mirrors are being introduced in this iteration. An existing private proxy is already in place to handle encrypted tunnels, eliminating the need to expose GitHub cookies, tokens, or the contents of private repositories to untrusted mirror sites.