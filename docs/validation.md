# Local Verification Results

Date: 2026-11-08. The main program is a Windows x64 Release build, configured to use a local Xray HTTP proxy endpoint at `127.0.0.1:10808`. These local network results cannot be extrapolated to other regions or nodes.

## Delivery and Resource Usage

- The executable build artifact is located at `dist/github-turbo.exe`. This initial official release is 551,424 bytes (~539 KiB). Please refer to `reports/artifact.json` for the final file size and hash after the inclusion of status fields and subsequent testing.
- Initial proxy mode: Working set 6.74 MiB, private memory 1.55 MiB.
- After automatic mode completes multi-domain probing and actual requests: Working set 10.46 MiB, private memory 3.07 MiB.
- The figures above are sampled values ​​obtained via the Windows `Get-Process` command; they do not represent peak load or results from long-term memory leak testing. No other software candidates were measured, so no cross-application memory usage ranking is provided.
- The main program utilizes streaming forwarding and does not load entire responses into memory. It defaults to 64 tunnels, with two 16 KiB buffers per tunnel; note that the total process memory usage is not limited solely to the size of these buffers.

## Network Samples

Final policy: Global `auto` mode, with `github.com` explicitly set to `proxy` and a cache duration of 120 seconds. The data below is sourced from `reports/network.json` (units in seconds). 

| Request | Direct Connection | Existing Proxy | GitHubTurbo | Observations |
| --- | --- | --- | --- | --- |
| Full GitHub Homepage HTML | 11.434 | 5.110 | 12.952 | All returned 200; the prototype was slower in this run—no claim of universal acceleration can be made. |
| GitHub API | 0.786 | 4.048 | 1.640 | Prototype opted for a direct connection; initial probing added overhead. |
| Raw README | 0.893 | 3.697 | 0.246 | Prototype opted for a direct connection; a previous direct-connection attempt timed out, indicating network instability. |
| Hello-World ZIP | 0.583 | 4.609 | 1.549 | Prototype opted for a direct connection; content hashes match across all three. |
| GitHub CLI Release Checksum List | 504 | 18.055 | 7.491 | Both the proxy and the prototype returned 200 (after one redirect); file hashes match. |

This table represents a small sample of sequential, real-world requests and does not serve as a statistical benchmark. Additionally, diagnostic/Git smoke tests were running during some of the sampling periods, which may have affected network load and cache state. Since webpage HTML varies with each request, comparing full-page SHA256 hashes is not feasible. For the API, Raw, and ZIP files, the successful responses share identical hashes; for the Release file, both successful paths yielded an SHA256 hash of `f756e7859d90dbb5e85aae5194206c37da220bec9bac23d3d659078a8e766fdc`. The initial test of `3proxy 1.0.3 x64 Release` returned a 504 status code across all three tested routes (see `reports/network-proxy-baseline.json`); consequently, it was not recorded as a success, and no actual memory usage data could be obtained. Subsequently, the GitHub CLI’s official release verification manifest was used to validate the redirection of release domain requests. Note that this test involved a file of approximately 2 KiB; it does not demonstrate download speeds for large releases or the performance of resumed downloads.

## Functionality and Reliability

- Rust unit tests cover: domain boundaries/ports/input injection, rejection of internal network (LAN) IPs, HTTP header limits, handling of tunnel bytes following reserved headers, bidirectional streams/half-closed connections, configuration scope and exact-match domain patterns, and cache capacity.
- Passed `cargo clippy --all-targets --locked -- -D warnings`.
- Local black-box integration tests confirm: requests to non-GitHub targets return 403; upstream 407 errors are converted to 502; the status interface functions correctly; 512 KiB binary data is transmitted with complete integrity; no data sent early by the upstream (within the same packet) is lost; and half-closed connections are handled properly.
- Successful execution of a live `git ls-remote` (HEAD: `7fd1a60b01f91b314f59955a4e4d4e80d8edf11d`).
- Successful execution of a live `git clone --depth=1`, with the checked-out `README` containing "Hello World!" (sample available in `reports/hello-world-smoke`).
- The download script successfully retrieved the raw `README` (13 bytes) via the prototype; no modifications were made to the user's global Git configuration.
- DNS comparison: System-based resolution and Cloudflare DoH returned different GitHub addresses (corresponding to different geographic regions). The raw DoH response provided four candidate IPv4 addresses, whereas the system-level resolution returned only one at the time. These discrepancies do not, in themselves, prove DNS poisoning; furthermore, these short-TTL addresses were not written into the default configuration.
- The `diagnose` tool considers a 4xx response from the CDN root directory as evidence that the TLS/HTTP connection is reachable; therefore, a "successful" diagnostic result does not necessarily imply that any specific file was successfully downloaded.

## Pending Verification

Several aspects remain unverified, including: browser login state persistence alongside full page resource loading; private repository operations (including `push`); Git LFS; resumable/chunked uploads and downloads for large files; long-term operational stability; performance under a peak load of 64 concurrent streams; and cross-regional comparative analysis. Furthermore, the current implementation cannot automatically recover via local transparent forwarding in scenarios where the upstream proxy is unavailable, an established TLS stream is interrupted, or the upstream returns a 5xx error.

Conclusion: The deliverable is a functional, lightweight routing prototype that successfully handles Git operations, raw file access, source code archives, and redirects to release assets. The current performance gains are achieved by reusing existing egress points and offloading traffic for domains better suited for direct connections. There remains significant potential for further optimization regarding automated throughput strategies for web pages and large files.