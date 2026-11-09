//! Direct-only bounded Range scheduling. Every byte interval and validator is checked.
use super::*;
use crate::forward::HttpBody;
use bytes::Bytes;
use futures_util::{stream::FuturesUnordered, StreamExt};
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::Body;
use hyper::{body::Frame, HeaderMap, Request, Response, StatusCode};
use std::collections::BTreeMap;
use std::collections::VecDeque;

// Put inspected frames back without losing bytes, trailers or Content-Length.
struct PrefixBody {
    prefix: VecDeque<Result<Frame<Bytes>>>,
    bytes: u64,
    inner: HttpBody,
}
impl Body for PrefixBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Bytes>>>> {
        if let Some(frame) = self.prefix.pop_front() {
            if let Ok(frame) = &frame {
                if let Some(data) = frame.data_ref() {
                    self.bytes -= data.len() as u64;
                }
            }
            std::task::Poll::Ready(Some(frame))
        } else {
            std::pin::Pin::new(&mut self.inner).poll_frame(cx)
        }
    }
    fn is_end_stream(&self) -> bool {
        self.prefix.is_empty() && self.inner.is_end_stream()
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        let source = self.inner.size_hint();
        let mut hint = hyper::body::SizeHint::new();
        hint.set_lower(source.lower().saturating_add(self.bytes));
        if let Some(upper) = source
            .upper()
            .and_then(|upper| upper.checked_add(self.bytes))
        {
            hint.set_upper(upper);
        }
        hint
    }
}
fn restore(body: &mut HttpBody, prefix: VecDeque<Result<Frame<Bytes>>>, bytes: u64) {
    if prefix.is_empty() {
        return;
    }
    let empty = Full::new(Bytes::new())
        .map_err(|never| match never {})
        .boxed_unsync();
    let inner = std::mem::replace(body, empty);
    *body = PrefixBody {
        prefix,
        bytes,
        inner,
    }
    .boxed_unsync();
}

struct RangeTask<'a> {
    state: &'a Arc<State>,
    host: &'a str,
    uri: &'a hyper::Uri,
    headers: &'a HeaderMap,
    etag: &'a str,
    total: u64,
}
async fn pipeline(
    task: RangeTask<'_>,
    mut offset: u64,
    tx: &tokio::sync::mpsc::Sender<Result<Frame<Bytes>>>,
) -> Result<()> {
    let mut next = offset;
    let mut pending = FuturesUnordered::new();
    let mut ready = BTreeMap::<u64, Bytes>::new();
    while offset < task.total {
        // Four live requests; eight chunks including reordering (2 MiB maximum).
        while pending.len() < 4 && pending.len() + ready.len() < 8 && next < task.total {
            let start = next;
            let end = (start + 256 * 1024).min(task.total) - 1;
            next = end + 1;
            let task = &task;
            pending.push(async move {
                (
                    start,
                    piece(
                        task.state,
                        task.host,
                        task.uri,
                        task.headers,
                        task.etag,
                        (start, end, task.total),
                    )
                    .await,
                )
            });
        }
        let item = tokio::select! { _=tx.closed()=>return Ok(()), item=pending.next()=>item };
        let (start, result) = item.ok_or_else(|| err("range pipeline gap"))?;
        ready.insert(start, result?);
        while let Some(bytes) = ready.remove(&offset) {
            offset += bytes.len() as u64;
            if tx.send(Ok(Frame::data(bytes))).await.is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}
pub fn interval(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.strip_prefix("bytes ")?;
    let (span, total) = value.split_once('/')?;
    let (start, end) = span.split_once('-')?;
    let (start, end, total) = (start.parse().ok()?, end.parse().ok()?, total.parse().ok()?);
    if start > end || end >= total {
        return None;
    }
    Some((start, end, total))
}
pub fn eligible(host: &str, headers: &HeaderMap, original: &HeaderMap) -> Option<(u64, String)> {
    if original.contains_key("range")
        || original.contains_key("if-none-match")
        || original.contains_key("if-modified-since")
    {
        return None;
    }
    if headers
        .get("content-encoding")
        .is_some_and(|value| value != "identity")
    {
        return None;
    }
    let total = headers
        .get("content-length")?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()?;
    if total < 4 * 1024 * 1024 {
        return None;
    }
    let media = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|s| s.starts_with("image/") || s.starts_with("video/"));
    if host != "release-assets.githubusercontent.com"
        && host != "objects.githubusercontent.com"
        && !media
    {
        return None;
    }
    let etag = headers.get("etag")?.to_str().ok()?;
    if etag.starts_with("W/") || !etag.starts_with('"') || !etag.ends_with('"') {
        return None;
    }
    Some((total, etag.to_owned()))
}
async fn piece(
    state: &Arc<State>,
    host: &str,
    uri: &hyper::Uri,
    headers: &HeaderMap,
    etag: &str,
    span: (u64, u64, u64),
) -> Result<Bytes> {
    let (start, end, total) = span;
    let mut last = String::new();
    for _ in 0..2 {
        let mut req = Request::new(
            Full::new(Bytes::new())
                .map_err(|never| match never {})
                .boxed_unsync(),
        );
        *req.uri_mut() = uri.clone();
        *req.headers_mut() = headers.clone();
        for key in [
            "if-none-match",
            "if-modified-since",
            "content-length",
            "transfer-encoding",
            "connection",
        ] {
            req.headers_mut().remove(key);
        }
        req.headers_mut().insert("host", host.parse()?);
        req.headers_mut()
            .insert("accept-encoding", "identity".parse()?);
        req.headers_mut()
            .insert("range", format!("bytes={start}-{end}").parse()?);
        req.headers_mut().insert("if-range", etag.parse()?);
        req.extensions_mut().insert(pool::IndependentRange);
        let work = async {
            let exchange = state
                .pool
                .send(state, host, req, None, Some(Duration::from_secs(8)))
                .await?;
            let response = exchange.response;
            let lease = exchange.lease;
            let route = lease.route().clone();
            if matches!(route, Route::Proxy) {
                return Err(err("split trial requires direct transport"));
            }
            if response.status() != StatusCode::PARTIAL_CONTENT
                || response
                    .headers()
                    .get("content-range")
                    .and_then(|h| h.to_str().ok())
                    .and_then(interval)
                    != Some((start, end, total))
                || response.headers().get("etag").and_then(|h| h.to_str().ok()) != Some(etag)
                || response
                    .headers()
                    .get("content-encoding")
                    .is_some_and(|v| v != "identity")
            {
                return Err(err("range response or entity validator mismatch"));
            }
            let bytes = timeout(
                Duration::from_secs(15),
                Limited::new(response.into_body(), (end - start + 1) as usize).collect(),
            )
            .await??
            .to_bytes();
            if bytes.len() as u64 != end - start + 1 {
                return Err(err("truncated Range response"));
            }
            state
                .counters
                .downloaded
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            state.health.success(host, &route, 500.);
            drop(lease);
            Ok(bytes)
        }
        .await;
        match work {
            Ok(bytes) => return Ok(bytes),
            Err(e) => {
                last = e.to_string();
                state
                    .routes
                    .lock()
                    .await
                    .remove(&Traffic::classify(host, uri.path()).key(host));
            }
        }
    }
    Err(err(&last))
}
pub async fn start(
    state: &Arc<State>,
    host: &str,
    uri: &hyper::Uri,
    original: &HeaderMap,
    headers: &HeaderMap,
    route: &Route,
    original_body: &mut HttpBody,
) -> Option<Response<crate::forward::HttpBody>> {
    let (total, etag) = eligible(host, headers, original)?;
    let memo = download::Memo::key(host, uri, &etag, total, route);
    if state.optimization().range_memo && state.download_memo.blocked(memo) {
        return None;
    }
    let permit = state.split_slots.clone().try_acquire_owned().ok()?;
    let chunk = 256 * 1024u64;
    let batch = |base: u64| {
        let mut spans = Vec::new();
        let mut pos = base;
        for _ in 0..4 {
            if pos >= total {
                break;
            }
            let end = (pos + chunk).min(total) - 1;
            spans.push((pos, end));
            pos = end + 1;
        }
        spans
    };
    let first = batch(0);
    let mut initial = Vec::new();
    let trial = futures_util::future::join_all(
        first
            .iter()
            .map(|(a, b)| piece(state, host, uri, original, &etag, (*a, *b, total))),
    );
    let mut prefetched = 0;
    let outcome = if state.optimization().adaptive_split {
        let mut prefix = VecDeque::new();
        let winner = {
            let single = async {
                while prefetched < 1024 * 1024 {
                    match original_body.frame().await {
                        Some(Ok(frame)) => {
                            if let Some(bytes) = frame.data_ref() {
                                prefetched += bytes.len() as u64;
                            }
                            prefix.push_back(Ok(frame));
                        }
                        Some(Err(error)) => {
                            prefix.push_back(Err(error));
                            break;
                        }
                        None => break,
                    }
                }
            };
            tokio::pin!(single);
            tokio::select! {biased;_= &mut single=>None,result=timeout(Duration::from_secs(3),trial)=>Some(result)}
        };
        restore(original_body, prefix, prefetched);
        winner
    } else {
        Some(timeout(Duration::from_secs(3), trial).await)
    };
    let Some(outcome) = outcome else {
        if state.optimization().range_memo {
            state.download_memo.reject(memo);
        }
        state.health.event(host, "single-stream-won-trial");
        return None;
    };
    let results = match outcome {
        Ok(results) => results,
        Err(_) => {
            if state.optimization().range_memo {
                state.download_memo.reject(memo);
            }
            state.health.event(host, "split-trial-slow-single-stream");
            return None;
        }
    };
    for result in results {
        match result {
            Ok(bytes) => initial.push(bytes),
            Err(_) => {
                if state.optimization().range_memo {
                    state.download_memo.reject(memo);
                }
                return None;
            }
        }
    }
    state
        .counters
        .downloaded
        .fetch_add(prefetched, Ordering::Relaxed);
    let mut response_headers = headers.clone();
    response_headers.remove("transfer-encoding");
    response_headers.remove("connection");
    response_headers.insert("x-github-turbo-route", "direct-split".parse().unwrap());
    let state = state.clone();
    let host = host.to_owned();
    let uri = uri.clone();
    let original = original.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>>>(2);
    tokio::spawn(async move {
        let _permit = permit;
        state.health.event(&host, "direct-split");
        let mut offset = 0;
        for bytes in initial {
            offset += bytes.len() as u64;
            if tx.send(Ok(Frame::data(bytes))).await.is_err() {
                return;
            }
        }
        if state.optimization().range_pipeline {
            let task = RangeTask {
                state: &state,
                host: &host,
                uri: &uri,
                headers: &original,
                etag: &etag,
                total,
            };
            if let Err(error) = pipeline(task, offset, &tx).await {
                let _ = tx.send(Err(error)).await;
            }
            return;
        }
        while offset < total {
            let mut spans = Vec::new();
            let mut next = offset;
            for _ in 0..4 {
                if next >= total {
                    break;
                }
                let end = (next + chunk).min(total) - 1;
                spans.push((next, end));
                next = end + 1;
            }
            let requests = futures_util::future::join_all(
                spans
                    .iter()
                    .map(|(a, b)| piece(&state, &host, &uri, &original, &etag, (*a, *b, total))),
            );
            let results = tokio::select! {_ = tx.closed()=>return,results=requests=>results};
            for result in results {
                match result {
                    Ok(bytes) => {
                        offset += bytes.len() as u64;
                        if tx.send(Ok(Frame::data(bytes))).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }
        }
    });
    let body = StreamBody::new(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
    .boxed_unsync();
    let mut response = Response::new(body);
    *response.headers_mut() = response_headers;
    Some(response)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_ranges_and_strong_validator() {
        assert_eq!(interval("bytes 2-4/10"), Some((2, 4, 10)));
        for text in ["bytes 4-2/10", "bytes 2-10/10", "bytes 2-4/*", "2-4/10"] {
            assert!(interval(text).is_none());
        }
        let mut h = HeaderMap::new();
        h.insert("content-length", "8388608".parse().unwrap());
        h.insert("etag", "\"a\"".parse().unwrap());
        assert!(eligible(
            "release-assets.githubusercontent.com",
            &h,
            &HeaderMap::new()
        )
        .is_some());
        h.insert("etag", "W/\"a\"".parse().unwrap());
        assert!(eligible(
            "release-assets.githubusercontent.com",
            &h,
            &HeaderMap::new()
        )
        .is_none());
    }
}
