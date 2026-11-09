# Empirical IP Testing for FGit Releases

Target: `https://github.com/fastgh/fgit/releases/download/v1.2.0/fgit.exe` (7,531,520 bytes), downloaded via the standard Microsoft Edge browser. During testing, all local GitHub acceleration engines were disabled. No modifications were made to the `hosts` file, NRPT (Name Resolution Policy Table), certificates, or system proxy settings, and the downloaded file was not executed.

## Methodology

A reference file was first obtained via a direct system connection. Since the GitHub Release API returns `null` for the `digest` field, the integrity of the content from each egress point was verified by comparing its SHA256 hash against that of the reference file (which was validated via TLS). Note that this does **not** constitute verification of the publisher’s digital signature or an official checksum.

Reference file SHA256: `b867b472001a9222bb5b6f89a09e73567b6583f9f75a7aff6304a49ef14d0995`. The full file download was performed using Node.js’s built-in HTTP/2 implementation, utilizing a single stream and targeting the same CDN object. A new connection was established for each attempt, with original domain SNI and certificate validation preserved. No local reverse proxies or third-party mirrors were used. This approach was chosen to control protocol-level and client-side variables, rather than relying on the automated behavior of the Edge browser itself.

The testing process involved an initial three-round screening (1 MiB chunks) across the system-resolved IP, two `.133` candidate IPs, and historical `.154`/`.215` candidate IPs. This was followed by two rounds of full-file downloads, alternating between the two egress points. Screening records are available under `20261108-201632-935f70`, and full-file download records under `20261108-201653-5adc49` (located in `bench/runs/<id>/report.json` and `report.html`).

## Full-File Retrieval Results

| Egress Point | Round 1 | Round 2 | Success Rate |
| --- | ---: | ---: | ---: |
| 185.199.108.133 (Current DNS Candidate) | 1.502 s | 1.378 s | 2/2 |
| 185.199.110.154 (Historical Candidate) | 15 s timeout (received ~3.24 MB) | 11.560 s | 1/2 |

For both sets, TLS completion time was approximately 0.12 seconds, and the Time to First Byte (TTFB) was roughly 0.19–0.21 seconds. The SHA256 hashes of the successfully retrieved full files were identical. The successful attempt via the `.154` address took about eight times longer than the attempts via the `.133` address; the timeout instance was excluded from the median calculation for successful attempts. These results reflect two rounds of testing from the local machine during the current session; this IP should not be considered a permanently optimal choice.

**Short-segment tests can be misleading:** The median time to retrieve a 1 MiB segment was 0.508 seconds for the `.154` address, compared to 0.720 seconds for the `.108.133` address. Relying solely on the initial segment might favor an egress point that is actually slower for full-file transfers. A test address, `.215`, was rejected in all three attempts due to a certificate mismatch for the target domain; therefore, it is not considered a viable, high-performance node.

## Environment and Routing Considerations

- During testing, the local GitHubTurbo/GitPort engine, relevant NRPT (Name Resolution Policy Table) entries, and common reverse-proxy listening ports were all inactive; proxy-related environment variables were unset. However, a static `hosts` file entry from "GitHub520" remained present, meaning that "disabling acceleration software" did not equate to a full restoration of default DNS resolution. This static table does not contain a specific entry for `release-assets.githubusercontent.com`. - The system returned IP addresses in the `185.199.108.133` range for the final download domain, and an established connection between the Edge process and `185.199.108.133` was observed. While this aligns with the candidate download domains, the process connection table alone is insufficient to definitively attribute a specific connection to the file in question.
- The application’s "release" seed list is ordered as follows: four addresses ending in `.154`, followed by four in `.215`, and finally four in `.133`. The `best_direct` logic truncates this list to the first eight entries, meaning the `.133` addresses—which belong to the correct DNS group—are excluded from the initial selection round.
- The DNS-based selection branch requires the completion of multiple resolution tasks, whereas the seed-based branch "wins" as soon as a valid TLS handshake is established. An HTTP 200 status is recorded as "healthy," but the actual throughput of the full payload is not yet factored into the routing score. Consequently, the application may persistently favor endpoints that offer fast handshakes but poor download speeds.

Conclusion: The observed speed advantage of the "raw" (direct) download is supported by empirical measurements, with IP/egress selection playing a significant role—it cannot be attributed solely to the overhead of segmented downloads. Future improvements should prioritize healthy candidates derived from standard DNS resolution, correct the truncation logic and DNS-related selection bias, and evaluate download paths based on sustained transfer rates, latency/pauses, and failure rates. Initial segment speed and TLS handshake latency are not reliable proxies for sustained throughput. This round was limited to diagnostics; the accelerator was not re-enabled, nor were the test IP addresses added to the production configuration.