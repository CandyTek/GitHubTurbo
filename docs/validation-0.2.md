# GitHubTurbo 0.2 Verification Log

Date: 2026-11-08. Windows x64, with default `upstream=null`. This version is intended as a system-integrated release ready for immediate trial; cross-machine validation for a public stable release has not yet been conducted.

## Default Integration and Networking

`reports/smoke-0.2.json` contains a comprehensive check, including sampled regional DNS resolution. The scripts were executed after the system session reached an "enabled" state. No forward proxy was specified, full certificate validation was maintained, and it was verified that the certificate chain for every final HTTPS response includes the GitHubTurbo local CA—ensuring that direct connections bypassing the software are not incorrectly counted as successful.

| Scenario | Status | Total Request Time (This Run) | Size |
| --- | --- | ---: | ---: |
| GitHub Homepage | 200 | 27.44 s | 525,303 bytes |
| Repository Page | 200 | 15.06 s | 182,014 bytes |
| GitHub API | 200 | 2.94 s | 5,566 bytes |
| Raw README | 200 | 2.55 s | 13 bytes |
| Source Code ZIP | 200 | 4.10 s | 351 bytes |
| CLI Release Checksum List | 200 (with one redirect) | 5.31 s | 1,971 bytes |
| Page CSS | 200 | 2.64 s | 87,305 bytes |

These are functional samples based on a limited number of requests; they do not serve as bandwidth benchmarks, nor do they imply that all network operations will be faster. Factors such as cold-start DNS resolution, handshake contention, and failover mechanisms for regional network paths can increase latency. The 27-second load time for the homepage, in particular, leaves significant room for optimization. During early build stages, 500 and 504 errors occurred on the homepage and API endpoints—documented in files such as `reports/smoke-0.2-before-buffering.json` and `reports/smoke-0.2-single-region.json`. These failures were not misclassified as successes.

Diagnostics revealed that the local machine could not reach two overseas DoH (DNS-over-HTTPS) bootstrap addresses, and the default available resolver returned only addresses within the same region. To address this, the system now uses EDNS subnet sampling via Alibaba’s DoH service to obtain candidates from various regions, followed by actual TLS verification and a fallback mechanism for request routing. No GitHub IP addresses are hardcoded into the main program. The EDNS subnet parameter utilizes the network prefix of a public resolver as a reference point for DNS resolution; it does not transmit the user’s actual internal network segment. Note that this method does not guarantee that a returned IP address belongs to a specific, fixed geographic region. [Alibaba DNS API Parameter Documentation](https://help.aliyun.com/en/dns/httpdns-doh-json-api)

In this iteration, the 3proxy 1.0.3 x64 Release (1,881,325 bytes) was successfully downloaded in its entirety. Its SHA256 hash is `f831b550c532cd6c8daa11ec8cdd6bcfd62a37701a5bd8367a4bfa06a0660251`, which matches the digest provided by the official release API. This file was retrieved during an intermediate build stage and is preserved as `reports/v02-3proxy.zip`.

## Git and SSH

- Operations including `git ls-remote`, shallow cloning, and subsequent `fetch` commands were successful, even without an explicit proxy configured for Git.
- During the final static CRT (C Runtime) build, a full `fetch` was performed again, with the raw download returning an HTTP 200 status. The core logic remains identical to the build mentioned in the table above; the entire suite of time-consuming comparative tests was not re-run after switching the CRT linking method.
- An SSH connection established via `ProxyCommand` to the official SSH 443 endpoint returned the identification string `SSH-2.0-9bd49d50`. No authentication for test accounts, SSH cloning for private repositories, or modification of `known_hosts` is performed.
- There is no automated replay of `git push` or POST requests; testing with public repositories does not guarantee compatibility with private repositories, LFS, or complex login workflows.

## Automation and Recovery

8 Rust tests passed, covering: domain/address restrictions, configuration limits, cache capacity, DNS (A/AAAA/HTTPS/refusal responses), early-arrival tunnel bytes, bidirectional half-closure, as well as 512 KiB binary uploads/downloads using actual local TLS, `Range 206` requests, `421` status codes for `Host` mismatches, rejection of invalid certificate names, and the non-replay of `POST 503` responses.

`cargo clippy --all-targets --locked -- -D warnings` passed; all unit tests were also executed against the final Windows x64 static-CRT target. Black-box integration tests confirmed compatibility with explicit proxy modes, upstream rejection scenarios, and binary tunnel transmission.

`scripts/lifecycle.ps1` performed three sets of validations on a live Windows environment, ensuring the program was restored to its original state upon completion:

1. Graceful deactivation: Restored `hosts` and SSH files to their original byte-for-byte state (assuming no external modifications), reverted Git domain configurations, and removed tasks related to NRPT, CA trust, and login recovery.
2. Forced engine termination: The native daemon triggered the recovery process, which passed the same set of checks.
3. Intentional corruption of the default `config.json`: The recovery command (equivalent to `Recover.cmd`) successfully restored the configuration, after which the pre-test configuration file was reinstated.

Results are available in `reports/lifecycle-0.2.json`. Note that an actual system reboot to verify the login recovery task has not been performed; only the creation and cleanup of the task were validated. Therefore, this cannot be considered a verified "reboot test" success.

## Resources and Distribution

The final static-CRT executable is 3,271,680 bytes. `dumpbin` analysis shows that import dependencies are limited to Windows system DLLs, with no remaining dependency on `VCRUNTIME140.dll`. (Note: This has not yet been tested on a "clean" machine—one without development tools installed.) Measurements taken after completing the static build integration and executing actual requests: the engine’s working set is 11.46 MiB (with 3.88 MiB of private memory), and the native daemon’s working set is 6.60 MiB (with 1.58 MiB of private memory), totaling approximately 18.1 MiB for the working set. These are sampled values, not peak concurrency figures or results from leak testing.

The portable package is generated based on an explicit file manifest, containing only the executable, default configuration, start/stop/recovery scripts, download scripts, a README, a list of dependencies, and licenses. Local state, private keys, `hosts` file backups, session data, and development logs are excluded; the contents of the ZIP archive have been verified against the manifest. The final file size and SHA256 checksum are as specified in the corresponding `.sha256` files within the `packages` directory and in `reports/artifact-0.2.json`.

## Pending Validation

The following scenarios have not yet been fully validated: full browser-based login and complete resource waterfall loading; private repository operations (including `push` and LFS); usage with non-OpenSSH clients; independent certificate stores; explicit remote proxies; applications with built-in DNS-over-HTTPS (DoH) or hardcoded IP addresses; multi-user and service account configurations; long-term persistent operation; recovery after restarts; peak concurrency; and cross-region throughput. As this release lacks code signing, an installer, and an auto-update mechanism, it is not designated as a public stable release.