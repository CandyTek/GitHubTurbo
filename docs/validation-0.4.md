# 0.4: Measure First, Then Improve Incrementally

Date: 2026-11-08. Environment: Windows x64; no forward proxy used by default. Tools: Rust 1.96, curl 8.13 (with Schannel), Git 2.45.1. The SHA256 hash for the frozen 0.3 executable is `d704dbef7b034087d5441bbb68f06995b62d2c7496a1c8f253f3155a14bb90c5`.

## A Reproducible, Realistic Network Platform

The entry point is `bench/Start.cmd`, defaulting to `http://127.0.0.1:18190/`. The platform supports direct connections (with independent DNS resolution), frozen versions, current builds, existing GitPort configurations, and optional HTTP proxies. Each accelerator instance runs on a dedicated port and utilizes a unique Certificate Authority (CA) scoped exclusively to that instance, without integrating into the host system. Functionality has been verified for web-based start/stop controls, prevention of concurrent execution, cleanup of processes and temporary files upon cancellation, and the preservation of the system `hosts` file.

The platform provides support for web pages, raw data, CSS, APIs, releases, byte-range requests, and Git operations (shallow clone, fetch, and fsck). Test datasets consist of fixed content (or semantically consistent data) with predefined expected sizes and hashes; for full releases, the SHA256 hash is verified against the value provided by the GitHub Releases API. Hashes for 1 MiB range requests are derived from the prefix of an official file that has been verified multiple times via full-file SHA256 checks. Git tests target specific commits; the medium-scale test uses the Git tag `curl-8_13_0` (commit `1c3149881769e7bd79b072e48374e4c2b3678b2f`). Data for every attempt—including success/failure status, timeouts, raw latency, effective throughput, content validation, application statistics, binary hashes, and configuration—is saved. Failed attempts are excluded from "successful latency" metrics but are always factored into the success rate. Results can be exported as JSON or offline HTML, with raw temporary response bodies and headers stripped out. An execution lock prevents simultaneous testing from the web interface and the CLI, ensuring they do not compete for bandwidth.

The initial formal A/A calibration was conducted using the same "0.3" executable version, incorporating an independent direct-connection test. The test spanned three rounds across four scenarios, covering both "cold" (initial) and "warm" (subsequent) states, with a 72/72 success rate. The comparison ratios were mostly close to 1; this serves to validate the noise levels and measurement methodology and should not be interpreted as an optimization result. Recorded under: `20261108-182005-137a68`.

## Item 1: Reusing Verified TLS Probe Connections

In previous versions, the TLS probe connection was discarded after the probe was completed, forcing the connection pool to perform a new handshake. Version 0.4 retains a small number of verified probe connections; actual requests retrieve these based on domain, route, and ALPN configuration, eliminating the need for a second connection setup. A maximum of 16 connections are retained, with a reuse window of three seconds; changing settings or reselecting routes clears the pool. Upstream raw hostname and certificate chain validation remain in effect.

Using the same build, with `probe_reuse` toggled between disabled and enabled, a three-round comparison in the "cold" state yielded a 24/24 success rate. Record `20261108-183131-6c9ff3`:

| Scenario | Median Duration (Disabled) | Median Duration (Enabled) | Median Pairwise Ratio |
| --- | ---: | ---: | ---: |
| Raw | 0.411 s | 0.238 s | 1.72× |
| CSS | 0.499 s | 0.249 s | 1.72× |
| Release Checklist | 1.805 s | 0.933 s | 1.93× |
| Small Repository Shallow Clone | 1.938 s | 1.422 s | 1.36× |

These results are based on a small-scale local sample; the same magnitude of improvement cannot be guaranteed for other regions. This feature is enabled by default. Separate TLS testing confirms that actual requests utilize the same verified connection, and that toggling the HTTP/2 setting does not lead to the improper reuse of incompatible probe connections.

## Item 2: Remembering Unfavorable Range Probing Attempts

For a given host, path, strong ETag, content length, and network path, the system will cache results—such as probe failures, excessive latency, or instances where a single-stream request outperformed the multi-stream probe—for ten minutes (up to a maximum of 512 entries). Full URLs and authentication parameters are not stored. The status is re-evaluated if the IP/TLS configuration or object version changes; clearing the network path cache will also purge these stored entries.

Controlled, real-world TLS testing confirms that if a slow "Range" request is encountered, the system proceeds with the original 5 MiB response; upon a subsequent request, it skips the four-stream probe and issues a standard GET request instead, ensuring byte-for-byte consistency. Public network record `20261108-183410-ebed82` includes instances of timeouts and failures; in scenarios involving a healthy CDN, no consistent speed benefit was demonstrated, so one cannot selectively cite only the faster individual results. This feature is primarily intended to avoid redundant overhead in cases where no benefit is expected; it is enabled by default.

## Item 3: Bounded Segmented Pipelining

Instead of waiting for requests to complete in batches, this approach utilizes a continuous stream of in-flight requests combined with a bounded out-of-order buffer. It supports up to four concurrent in-flight requests, covering a maximum of eight segments (256 KiB each)—whether currently in transit or already completed. The output remains strictly ordered by byte offset, with ETag, range, and length verification performed accordingly. Controlled testing confirms that if the initial segment (the "head of the line") is delayed, subsequent ranges can still be processed; the final byte sequence remains identical.

Record `20261108-183206-6c0981`: All 12 full-scale release tests passed verification. However, the median performance ratio (compared to the baseline) was approximately 0.77× for "cold" (uncached) states and 1.06× for "warm" (cached) states, showing no consistent performance gain. While the implementation and experimental code are retained, `range_pipeline` remains disabled by default. It currently lacks the more sophisticated throughput scheduling and "slow-tail" segment task migration found in GitPort.

## Item 4: Race Between Single-Stream and Four-Way Probing for the Initial Chunk

This method maintains the original response stream while simultaneously initiating four-way probing requests. The approach that completes the first 1 MiB of data first "wins." If the single-stream approach wins, or if a segment error or a three-second timeout occurs, the system reverts to the original stream. Any bytes already verified, along with the body length and trailers, are preserved—avoiding the need to re-issue a full GET request. This offers better adaptability to fluctuating load speeds compared to always-on segmentation; however, a faster start does not guarantee faster completion for the entire file.

Record `20261108-184109-fe70ff`: Using the same build, with only the `adaptive_split` feature toggled, three sets of full-scale releases all passed official SHA256 verification. In two of these sets, the version with `adaptive_split` enabled was faster, with a median performance ratio of approximately 1.76×. **Aggregated median completion times were 12.57 seconds (disabled) versus 13.33 seconds (enabled)—showing no improvement.** Since individual performance ratios and overall median times are distinct statistical metrics, one cannot be used to obscure the other. This round of network fluctuation was significant; all individual test results have been retained. We do not claim, based on these findings, that there is a universal speedup for large files. This feature is enabled by default—based on the data received—but can be toggled off independently for comparison.

## Correctness and Recovery

21 Rust tests have passed. For the Windows static CRT target, all `clippy` checks passed (with warnings treated as errors). 11 platform-specific tests passed, covering failure counting, pairing constraints, file integrity verification, request header sanitization, API protection, individual experimental tasks, execution locks, and cross-process cancellation.

Version 0.4 has been validated for robust recovery: it handles graceful deactivation, automatic engine recovery following a forced termination, and restoration after configuration corruption—all followed by successful reactivation. It also supports the restoration of `hosts`/SSH byte configurations and Git settings, as well as the cleanup of NRPT/CA entries and recovery tasks. Management API configuration, concurrent write/save operations, and explicit proxy byte-relaying have all been verified. For full details, please refer to `reports/lifecycle-0.4.json`, `reports/control-0.4.json`, and `reports/bench-api-0.4.json`.

Current measurements do not account for full browser rendering (e.g., FCP), login states, private repositories, `git push`/LFS operations, long-term peak-load performance, or cross-regional effects. While individual `curl` tests utilize separate processes, the accelerator reuses internal connections for "warm" (active) states. Similarly, browsers can reuse connections and cache; therefore, the results for small requests on this platform cannot be directly equated to the speedup ratio for full browser-rendered pages.

## Final Build Regression

For build `20261108-185049-5f0266`, two rounds of "cold" and "warm" testing were conducted—comparing the frozen 0.3 version, the final 0.4 version, and a direct (unaccelerated) connection. The test suite covered Raw data, CSS, Release manifests, repository HTML, APIs, small repositories, medium-sized `curl`-based repository fetches, and 1 MiB range requests. Out of 96 total samples, 88 were successful, and 8 resulted in timeouts. For the Raw/CSS/Release, manifest, web page, API, and small-repository categories, the success rate was 72/72. For the "Range" category, the success rate was 10/12, with two timeouts occurring in the "frozen" (static/archived) version. For medium-sized repositories, the success rate was 6/12, with timeouts observed across all three transport routes. Failed attempts have not been removed from the records. Medium-sized repositories and sustained throughput remain the primary focus for future development; it should be clarified that the "cold start" performance gains seen in small repositories do not imply that cloning speeds have improved across the board for all repository types.

The next phase of development will be guided by platform data, focusing on: real-world transfer rate and completion-time forecasting; the reassignment of remaining tasks from "slow" segments; cross-restart route scoring (incorporating expiration policies); and the caching of redirect paths for avatars. These features have not yet been marked as complete in the 0.4 release.