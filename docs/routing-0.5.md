# 0.5.0 Routing Fixes and Real-World Testing

This release prioritizes routing improvements. It maintains standard single-stream downloads by default and does not introduce additional Range-request probes.

## Fixes

- When a DoH (DNS-over-HTTPS) query returns a valid address, the system proceeds immediately—waiting at most 30 ms for other concurrent resolution tasks to complete in the background—rather than waiting for the slowest provider.
- Real-time DNS resolution is initiated first; a 400 ms delay is applied before falling back to `hosts` entries or built-in address lists. Built-in lists are now sampled from the entire candidate pool, rather than being limited to the first 8 entries.
- For web requests, the system continues to use a latency-based competition model, with all connections undergoing certificate validation. Separate routing paths and connection pools are used for `release`, `objects`, `codeload`, and Git operations. During downloads, the system follows the DNS-resolved order, introducing a 150 ms delay between attempts for each fallback address to prevent minor TLS handshake timing differences from dictating the chosen egress path.
- Throughput is only recorded based on complete "200 OK" responses with a payload of at least 4 MiB. Short fragments, "206 Partial Content" responses, and client-initiated cancellations are excluded from bandwidth sampling. Errors occurring during the transfer of the response body are flagged as failures. If the most recent complete transfer is slow, this immediately impacts future routing decisions—ensuring that a decline in performance is not masked by historical high-speed averages.
- Observed metrics represent end-to-end throughput, which may be influenced by the specific file, time of day, and downstream read speeds. Historical data is segmented by domain and egress point (up to 512 entries) and retained in-memory for a maximum of six hours; routing decisions are reset upon network changes.
- The system does not switch IP addresses mid-stream, does not replay Git POST requests, and does not bundle third-party `hosts` entries, certificates, or proxy configurations.
- Routing caches from version 0.4 are invalidated. New web-related caches are restored for the short term, retaining their original age metadata, while download-specific routing paths and throughput metrics are re-learned from scratch upon each application launch.

## Three Sources

The application fetches `hosts` files from the following user-specified repositories at runtime:

- [jianboy/github-host](https://github.com/jianboy/github-host): `master/hosts`
- [521xueweihan/GitHub520](https://github.com/521xueweihan/GitHub520): `main/hosts`
- [maxiaof/github-hosts](https://github.com/maxiaof/github-hosts): `master/hosts`

Updates occur in the background upon startup and subsequently every hour. If an update fails, the previously successful data is retained; however, if no successful update has occurred for over 24 hours, the feature is disabled. Each response is limited to a maximum size of 256 KiB, supporting up to 512 domain names, with a limit of 16 public IP addresses per domain. Loopback addresses, private network addresses, invalid domain names, out-of-scope domains, and redirects are rejected. Candidate entries are not written to the system `hosts` file in their entirety; the original GitHub hostnames are still verified via TLS during upstream connections.

In the current local environment, 36, 33, and 31 in-scope domain names were retrieved from the respective sources. This represents a snapshot of the content at the time of retrieval, which may change as the source repositories are updated. Neither the source code nor the full `hosts` tables are embedded in the release package; however, information regarding the sources—along with the Mulan License provided by `maxiaof`—is included.

## Local `hosts`

Legacy static GitHub mappings and old `GitHub520` blocks have been removed (affecting 56 lines in total), while unrelated mappings remain untouched. A byte-for-byte backup of the original file has been created at:

`I:\RcProject\GitHubTurbo\state\hosts-backups\20261108-210919-122-6e85ab9a.json`

A full backup in `.bin` format is also available in the same directory. This directory is not included in the release package. You may use `scripts/hosts.ps1` to view, clean, or restore the `hosts` file. The restoration process verifies the original SHA256 hash and ensures that the current file matches the state following the cleanup, preventing the accidental overwriting of any subsequent manual edits.

## Verification Methodology

The engine passed 29 existing and newly added tests, while the platform passed 12 tests. The `hosts` utility was verified for handling irrelevant entries, mixed aliases, CRLF sequences, byte-by-byte recovery, and protection against subsequent edits. Existing features—including TLS origin domain validation, streaming, HTTP/2 multiplexing, prevention of POST request replays, and content consistency during experimental segmentation—remain intact.

Public-network testing was conducted using both legacy and current versions of the instances, deployed on private ports with independent Certificate Authorities (CAs). No upstream proxies were used, and no modifications were made to system-level network access. File downloads were verified by length and SHA256 hash; web pages were checked for HTML integrity; and "Raw" or CSS content was verified against fixed reference data. For repository cloning, the resulting repositories were inspected and compared against the expected commits. The test rotation order was varied in each round, with failures and timeouts recorded.

The file provided by the user is `fgit.exe` (version 1.2.0 from `fastgh/fgit`), with a size of 7,531,520 bytes; the file was downloaded but not executed. The SHA256 hash—`b867b472001a9222bb5b6f89a09e73567b6583f9f75a7aff6304a49ef14d0995`—serves as a local consistency reference for the downloaded file (verified via TLS) and does not represent a digital signature from the publisher.

The `direct` mode utilizes a direct connection following the resolution of the first IPv4 address via an independent DNS-over-HTTPS (DoH) service; this is distinct from the system DNS, caching, or HTTP/3 mechanisms used by Edge. While the client-side `curl` interface operates via HTTP/1.1, the two "Turbo" instances are capable of negotiating HTTP/2 with the upstream server. Consequently, these results cannot be directly interpreted as a performance ranking for Edge.

## Iteration Log

Initial intermediate version comparison: `20261108-211249-e2927d`, candidate SHA256 `d4a4ae192f8bd64e99d387e852d92a3a032119bb15de599dddf8c57d9e45829f`. Six full-download tests were conducted for each configuration: the legacy version succeeded 3 times, the new version succeeded 3 times, and the independent DoH direct-connection method succeeded 4 times. Observed durations ranged from just over 1 second to 30-second timeouts; therefore, it cannot be claimed that the new version is faster overall. Based on these results, the download process has been adjusted to prioritize the order of DNS addresses, while web-page requests continue to rely on a latency-based competition model.

Final version comparison: `20261108-212112-f4a38c`, candidate SHA256 `d025544d68a3161ba2637a153a7ec15d678a94d98a49c40ff382141f20399e36`. The complete report is provided in both raw JSON and offline HTML formats; final statistics are presented below.

Each scenario was tested four times (across two rounds, covering both "cold" and "warm" states), with a 30-second timeout threshold. Values ​​in parentheses indicate the number of successful attempts; time figures represent the median duration among successful samples. Note that success rates must also be considered when comparing performance. 

| Item | 0.4.1 | 0.5.0 | Standalone DoH (Direct Connection) |
| --- | --- | --- | --- |
| Web Page | 1.274 s (4/4) | 0.778 s (4/4) | 0.631 s (4/4) |
| Raw | 0.160 s (4/4) | 0.158 s (4/4) | 0.276 s (4/4) |
| CSS | 0.131 s (4/4) | 0.131 s (4/4) | 0.273 s (4/4) |
| Shallow Clone & Fetch | 1.610 s (4/4) | 1.227 s (4/4) | 1.336 s (4/4) |
| fgit Full Download | Incomplete within 30 s (0/4) | 3.082 s (2/4); other 2 attempts timed out | Incomplete within 30 s (0/4) |

The two successful `fgit` attempts took 1.712 s and 4.452 s, respectively; both the file size and SHA256 hash matched, with the actual egress point being `185.199.109.133`. Failed samples returned a 200 status code along with a partial body—indicating the transfer was simply incomplete within the allotted time—so these cannot be considered successful, nor can it be inferred that the files were tampered with. The new version successfully captured two valid throughput samples during full download attempts. This IP address has not been hardcoded as the default route. Peak engine resource usage in this round: Version 0.4.1—working set: 14,176,256 bytes, private memory: 5,365,760 bytes; Version 0.5.0—working set: 14,184,448 bytes (~13.53 MiB), private memory: 5,705,728 bytes (~5.44 MiB). These figures account only for the native engine—excluding the browser, benchmarking platform, system tray, and daemon processes—and do not represent an upper limit for all concurrent load scenarios.

Conclusion: Web-based operations and cold-start cloning have shown improvement. The success rate for "Release" operations has also improved, though it remains somewhat unstable. In some intermediate versions, performance is similar between newer and older builds. Given the small sample size and significant variability in network conditions over time, it cannot be claimed that access to GitHub will be faster in all regions or for all users. Segmented downloading remains disabled by default, and the option to use an existing local proxy continues to be available.