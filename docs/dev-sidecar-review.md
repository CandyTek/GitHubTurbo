# What GitHubTurbo Took From dev-sidecar

A review of [dev-sidecar](https://github.com/docmirror/dev-sidecar) and its engine (`ds-core` at `8882b03`), made on 2026-11-09. It is MPL-2.0 licensed, so ideas were reimplemented and no code was copied. Each idea was measured on the test network before being adopted.

## Adopted

**Connection warm-up (`prewarm`, on by default).** dev-sidecar tests its main hosts at startup and keeps testing them, stopping after an hour without use. GitHubTurbo goes further and holds a verified connection: the engine connects to five hosts at startup and sends no requests over those connections. Any of them used in the last hour keeps a connection ready. Re-route cancels this and forgets the usage. A failed attempt leaves no route cooldown, so starting before the machine is online cannot make the first real request fail. It only applies to the transparent front end.

**IPv6 detection.** dev-sidecar filters IPv6 when no interface has it. GitHubTurbo asks the routing table, which also catches an adapter with no way out, and rechecks every 60 seconds. Without a route it skips AAAA queries and IPv6 candidates, which previously used probe slots and delayed ordered IPv4 candidates by 150 ms each. The test machine has IPv6 disabled, so this path ran in every measurement here. Its separate effect was not isolated.

**A second reachable DoH provider.** Of the 13 endpoints tried here, only AliDNS and DNSPod answered, so DNS depended on one company. DNSPod (`doh.pub`, certificate validated) answered 24 of 24 JSON requests in about 50 ms. It returned the same addresses as AliDNS, so this adds redundancy rather than better routes, and hostnames now also go to Tencent.

## Bug found while measuring

`20.205.243.167`, `20.205.243.168` and `20.27.177.114` complete TLS with a valid `github.com` certificate and then refuse every `github.com` request with a bare 400 or 403. Routes are chosen on TLS alone, so these could win the race. The refusal also counted as a success and was cached as the route, and the longer route caching in `flow-rescue.md` made that last longer.

The three seed entries are removed. As hosts sources could supply similar addresses, the forwarder also treats a 400 or 403 from `github.com` or `api.github.com` without `X-GitHub-Request-Id` like a 421: the route cools down and a replayable request tries another address. Genuine errors sampled from those hosts all carried that header, including a 400 from the cache layer and a rate-limit 403.

With `github.com` limited to one refusing and one working address:

| Build | Successes |
| --- | --- |
| Previous commit `1af8241` | 12/30 |
| This build | 30/30 |

If a refusing address is the only candidate, for example through `ip_overrides`, the client gets the 400 once and then a 502 while it cools down.

## Measurements

Same binary (`cab6c807…`), only `prewarm` differs, alternating order, no saved routes. Time to first byte.

First request, 4 seconds after launch, 10 rounds:

| Case | Off | On |
| --- | --- | --- |
| Web page | 0.654 s | 0.123 s |
| CSS | 0.285 s | 0.074 s |
| Raw | 0.265 s | 0.078 s |

Returning after 340 idle seconds, past the 300-second connection lifetime, 6 pairs:

| Case | Off | On |
| --- | --- | --- |
| Web page | 1.057 s | 0.575 s |
| Raw | 0.597 s | 0.079 s |
| CSS | 0.015 s | 0.015 s (served from the local cache either way) |

Against `1af8241` over 10 rounds with the benchmark's cold and warm phases (`20261109-084733-2cffae`), raw, CSS and web had no failures on either side and paired median speedups of 0.99–1.28, with neither build consistently ahead. This phase does not exercise warm-up, because `/api/reset` cancels it. `git-clone` succeeded 16/20 on the previous build and 20/20 on this one. All four failures were HTTP 403 and recurred across phases of the same round, as a cached refusing route such as `20.205.243.167` would. The same 403 explains the 0.5.0 `git-clone` failures in `flow-rescue.md`, though the route used was not logged, so this link is inferred.

## Not adopted

| Idea | Why not |
| --- | --- |
| Decoy SNI (`sni: baidu.com`) | No throughput difference: real, two decoys and no SNI all ran at about 0.05 MB/s on the same IPs at the same moment, for release and Git pack downloads (10 rounds, one flow per arm). All arms connected, so nothing here blocks by SNI, and the existing no-SNI fallback covers networks that do. It also needs hostname verification handed off from SChannel. All flows were slow during this test, so it says little about a network that throttles by SNI. |
| Local 304 and one-year cache overrides | GitHub's hashed assets already send `immutable`, so browsers do not revalidate them. Avatars send `max-age=300`, and overriding that would serve stale images. |
| Public mirrors and `xget` | Would send credentials and repository traffic through third parties, with no integrity guarantee. |
| `http.sslVerify=false` and `unVerifySsl` | GitHubTurbo always verifies the original hostname. |
| Answering `collector.github.com` locally | Changes behaviour for no measured page-load gain. |
| ECH, Cloudflare routing, NAT64, P2P | Not applicable to GitHub's endpoints or this project's scope. |
| More seed IPs | Its `github.com` list is already covered. |

## Limitations

- **One network, one period.** Results may differ elsewhere.
- **`api.github.com` was not measured reliably.** These tests used up the anonymous limit of 60 requests an hour, and the later samples were rate-limit 403s on both sides. In the earlier ten-round warm-up run, before the limit was hit, the API showed no gain (0.46 s off, 0.52 s on), probably because GitHub's own response time dominates.
- **Downloads and Git are not helped by warm-up.** They use separate connections by design. `release-fgit` is highly dependent on network speed: it reached 3/3 in the earlier run, but 5/10 on this build and 4/10 on the previous commit during a slower period (`20261109-080646-35c302`). `git-medium` still does not finish.
- **Warm-up has a small background cost.** Up to five TLS connections at startup, HTTP/2 keep-alive pings, and reconnects while a host stays in use. Memory was not re-measured.
- **The guard covers two hosts.** Other hosts could have similar refusing addresses, but static hosts do not send the request ID, so this rule cannot be used for them.
