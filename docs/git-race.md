# Git clone racing (`git_race`, experimental, off by default)

## Why

Flow rescue keeps slow downloads moving by continuing them over new connections
with Range requests. A Git clone cannot use that: the pack arrives as the reply to a
`git-upload-pack` POST, with no ETag and no Range support, so once a slow
connection is chosen the whole clone is stuck with it. Connection speed is a
property of the individual TCP connection and can only be learned by trying, so
the remaining option is to try more than one at the start.

## What it does

For a POST to `https://github.com/<repo>/git-upload-pack` that asks for objects
(`want` lines, or a compressed body), declares its size and is at most 1 MiB:

1. The request body is buffered and sent over two new connections to two
   different GitHub addresses: the usual choice and the best alternative.
2. The first reply to deliver 1 MiB, or all of itself if smaller, is kept. The
   other connection is closed. git receives exactly one upstream reply, never a mix.
3. A refused or failed connection loses; refusals by the wrong GitHub edge cool
   down that address. If both fail, the request is sent once the ordinary way. If
   both give a real HTTP error (for example 401), the client sees that error.

Ref listings (protocol v2 `ls-refs`), pushes, and anything over 1 MiB or chunked
take the ordinary path. `/status` reports `git_race` counters: races started, how
often the usual choice or the alternative was kept, and how often both failed.

Cost: GitHub runs each fetch twice, and up to 1 MiB is downloaded twice.

## Measurements (2026-11-09, one network, cold clones, same binary off vs on)

`git-medium` is a full clone of `curl/curl` at a fixed tag.

| Run | Racing off (s) | Racing on (s) |
|---|---|---|
| `20261109-184210-302b8c` | 3.1, 15.0, 17.1, 13.9 | 5.6, 4.1, 3.8, 6.4 |
| `20261109-184700-2c9beb` | 4.6, 23.1, 4.5, 147.6, 67.6, 3.5 | 3.6, 38.7, 125.1, 3.7, 66.6, 3.9 |
| All 10, median | 14.5 | 4.9 |
| All 10, faster in pair | 4 | 6 |

The first run looked like a 3x gain. The second did not: both settings had clones
that took over a minute. Racing only judges the first 1 MiB, and a connection that
slows down later in the pack is not replaced. Ten pairs on a noisy network do not
show a reliable difference.

The small clone (`octocat/Hello-World`) became slower: median 1.54 → 2.29 s when
every request raced, and 1.59 → 1.94 s after ref listings were excluded. Two new
TLS connections cost more than the pooled one they replace.

Peak private memory of the engine was 5.5 MB off and 7.7 MB on.

## Status

Kept as an off-by-default experiment. It may help on networks where connection
speed varies a lot at the start of a transfer; it does not help with stalls in
the middle of one, and it slows small clones.
