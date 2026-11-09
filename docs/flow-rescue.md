# Slow-Connection Rescue, DNS and Route Caching

These changes follow a diagnosis made on 2026-11-08: on the test network, download speed is decided per TCP connection rather than per IP address.

## Diagnosis

Direct `curl` to `release-assets.githubusercontent.com`, with no accelerator, 8-second limit:

- Connections opened at the same moment to the same IP differed widely: one ran at 2.94 MB/s while three others ran at 0.06–0.08 MB/s.
- Median speed per IP was similar across the four addresses (0.08–0.13 MB/s), while individual connections ranged from 0.04 to 5.44 MB/s.
- The 0.5.0 run `20261108-212112-f4a38c` shows `185.199.109.133` at both 0.11 and 4.40 MB/s.

This is one network over a short period. The cause of the variation (path hashing, per-flow policing or something else) was not identified. The IP-level conclusion in `ip-selection-fgit.md`, based on two samples per address, may reflect the same per-connection variation.

## Changes

**Slow-connection rescue (`flow_rescue`, on by default).** Applies to GETs eligible for splitting: at least 4 MiB, strong ETag, not compressed, no client `Range`, and a release, object or media resource. The download starts as the ordinary single GET and sends no extra requests while it is healthy. Time spent waiting on a slow client does not count against upstream.

If upstream delivers under 512 KiB/s over a one-second window, or the connection fails, the download continues from the current offset over four new connections:

- Work is split into 1 MiB spans, with at most eight ahead of the client, which bounds buffering.
- A connection under 512 KiB/s after one second, or with no response headers after 2.5 seconds, is dropped and replaced. The budget is 24 drops plus one per span delivered, so a network where every connection is slow does not churn indefinitely.
- An idle connection helps with the unfinished span nearest the client. Each byte is kept once and output stays in order.
- Every Range reply must carry the original ETag and the exact span. The original GET keeps running until one reply has been verified, so a server that refuses Range still delivers the file over it.

Rescue connections are dedicated and never join the shared pools. `segmented_downloads` takes precedence when enabled. At most two downloads are rescued at once.

**Connection handling.** Concurrent downloads get separate connections while the pool has room. A download connection that averaged under 512 KiB/s over at least two seconds is not reused, including after a client cancels. A cancellation still teaches nothing about the IP.

**DNS.** DoH connect timeout is 800 ms (was 2 s) and query timeout 1.5 s (was 3 s). A lookup slot is released on the first answer instead of when the slowest provider finishes. A failing provider is skipped for 60 seconds, unless all are failing. Answers stay fresh for 5–60 minutes (was 5 seconds to 5 minutes) and can be served for 6 more hours while refreshing in the background. "Re-route" clears this cache.

**Routes and web connections.** The default `cache_seconds` is 600 (was 120). An idle web route up to an hour past that answers at once and refreshes in the background, unless it is in cooldown. Web connections stay open for 300 seconds of inactivity, with HTTP/2 keep-alive pings; download connections still close after 90.

**Fixed crash risk.** Restoring a saved route could panic when its age exceeded machine uptime, which aborts the release build shortly after a reboot.

## Verification

44 Rust tests pass on five consecutive runs, clippy is clean with warnings as errors, and the 12 platform tests pass. The tests cover byte-exact rescue, unlucky replacements, an upstream failing mid-file, a server refusing Range, a slow client and a healthy download not triggering rescue, and the option being disabled.

Public network, frozen 0.5.0 (`d025544d…`) against this build (`304e8120…`), 30-second limit, alternating order. Values are successes and the median of successful samples; failures are not excluded from the success counts.

Run `20261108-234528-468f33` (three rounds, cold and warm):

| Case | 0.5.0 | This build |
| --- | --- | --- |
| `release-fgit` (7.5 MB), cold | 0/3 | 3/3, 10.78 s |
| `release-fgit` (7.5 MB), warm | 0/3 | 3/3, 7.57 s |
| `git-clone`, cold | 2/3, 1.84 s | 3/3, 1.75 s |
| `git-clone`, warm | 2/3, 1.23 s | 3/3, 1.09 s |
| `git-medium`, cold and warm | 0/6 | 0/6 |

Run `20261108-235646-8c77aa` (ten rounds, cold and warm, all 120 samples successful):

| Case | 0.5.0 | This build |
| --- | --- | --- |
| Web page, cold | 0.901 s | 0.714 s |
| Web page, warm | 0.459 s | 0.355 s |
| Raw, cold | 0.253 s | 0.300 s |
| Raw, warm | 0.073 s | 0.074 s |
| CSS, cold | 0.310 s | 0.370 s |
| CSS, warm | 0.015 s | 0.015 s |

In the three-round run the web page was slower on this build (warm 0.20 s against 0.54 s), but that did not repeat over ten rounds; web timing is noisy. `/api/reset` now also clears DNS, so this build's cold samples include a DoH lookup that 0.5.0's do not, which probably accounts for the 50–60 ms on cold Raw and CSS. That explanation was not tested separately.

Five of the six `release-fgit` samples on this build triggered a rescue, opening 11–21 connections each; the sixth (warm, round 1) finished in 8.43 s on its original connection. Peak private memory rose from 5.4 MiB to 15.8 MiB.

## Limitations

- **Healthy networks are not covered by these public runs.** The network was slow throughout, so "no overhead when healthy" rests on the local tests.
- **Git is unchanged.** Pack downloads cannot use Range, and `git-medium` still times out at this network speed. Racing `git-upload-pack` would break the rule against replaying Git POST requests, and has not been done.
- **Signed URLs can expire during a rescue.** New Range requests reuse the URL the client was given. If it expires mid-rescue they are refused and the download ends short, leaving the client to resume. The original single GET would not be affected once started. This has not been observed or tested.
- **A connection that is merely under the threshold can be replaced even if nothing faster exists.** The drop budget limits the churn, and the 512 KiB/s threshold is fixed. On a line slower than this, an eligible download will open extra connections.
- **Idle-return gains are unmeasured.** The benchmark does not idle for minutes between requests, so longer caching and connection lifetimes are untested for their intended case.
