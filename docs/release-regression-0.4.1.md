# Release Download Performance Revisited & 0.4.1 Fixes

Following user feedback suggesting that release downloads in version 0.4 have become slower, we conducted a series of tests. Using the same 0.4 executable and a fixed set of upstream IP addresses, we performed four rounds of testing—alternating the `segmented_downloads` setting—without the use of a forward proxy. Each test involved a full download of the official GitHub CLI release (13,661,485 bytes), followed by a SHA256 checksum verification against the value provided by the GitHub Releases API.

Raw data: `bench/runs/20261108-194842-86016e/report.json`; corresponding offline report: `report.html`. The fixed IP addresses used for these tests—`20.205.243.166` for `github.com` and `185.199.110.133` for `release-assets.githubusercontent.com`—served solely as control variables for this experiment. They are not included in the default user configuration and are not recommended as reliable, long-term endpoints.

| Metric | Standard Single-Stream (Segmentation Disabled) | Default Probing (Segmentation Enabled) |
| --- | ---: | ---: |
| Successes | 4/4 | 3/4 |
| Median Time for Successful Samples | 2.409 seconds | 3.581 seconds |
| Timeouts | 0 | 1 (40 seconds) |
| Relative Performance (3 Trials) | All 3 trials faster | All 3 trials slower |

Duration per Trial (seconds):

| Trial | Segmentation Disabled | Segmentation Enabled |
| --- | ---: | ---: |
| 1 | 2.309 | 6.709 |
| 2 | 1.962 | 3.581 |
| 3 | 15.897 | 40.039 (Timeout) |
| 4 | 2.509 | 2.579 |

With segmentation enabled, the median time for successful samples increased by approximately 49%. Timeouts are excluded from this median and are listed separately. The SHA256 hashes for all successfully downloaded files were identical. These results corroborate user reports of slower performance, though the magnitude of this effect may vary by region and file.

Mechanistically, version 0.4 initiates four additional Range requests while maintaining the original response stream. Even if the system ultimately reverts to a single-stream download, this process can still generate extra connections, additional response data, and bandwidth contention. The outcome for the initial 1 MiB chunk does not guarantee sustained throughput for the entire file. In all four trials where segmentation was enabled, the process eventually reverted to a single-stream download, meaning the probing yielded no actual benefit. Network fluctuations were also present; in the third trial, even the standard single-stream download was noticeably slower.

## Remediation

- The configuration for the running instance has been immediately updated via the management interface to `segmented_downloads=false`, disabling automatic segmentation probing for subsequent requests.
- In version 0.4.1, segmentation is disabled by default (both in the built-in defaults and the release configuration) and is now designated as an experimental feature; the option to enable it manually remains available. - Retains HTTP/2, connection reuse for verified TLS probes, IP preference, and caching of public resources; standard Range requests/resumable downloads are handled according to the client's original request.
- The speed-testing platform now includes a master toggle for "segmentation" (chunked downloading) and a control group using a fixed public IP. In segmentation sub-experiments, the feature is enabled on both sides to ensure that a "disabled by default" setting doesn't lead to invalid test results.

The performance gains for small files observed during cold starts in version 0.4 cannot be used as evidence for improved throughput for large files. The previous default approach—initiating "probing" downloads—was overly optimistic; this version adopts a more conservative, standard single-stream strategy. Future optimizations will require evaluating both total download duration and failure rates.