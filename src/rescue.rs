//! Keep a download moving when its connection turns out to be slow.
//!
//! Speed here is a property of the TCP connection, not of the address: connections
//! opened at the same moment to the same IP can differ by 40x. So a download starts
//! as the ordinary single GET and costs nothing extra while that GET is fast. Only
//! once it has measurably fallen behind does it continue from the current offset
//! over several new connections, dropping any that are slow and letting fast ones
//! take over unfinished work. Output stays in byte order, and every Range reply must
//! carry the original strong ETag and the exact span, so bytes from different
//! connections are known to belong to one entity.
use super::*;
use crate::{forward::HttpBody, pool::Lease, routing::SLOW_FLOW_BYTES_PER_SECOND};
use bytes::{Bytes, BytesMut};
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::{body::Frame, HeaderMap, Request, StatusCode};
use std::sync::atomic::AtomicBool;

/// How often a read wakes up to re-check its speed, the client and cancellation.
const TICK: Duration = Duration::from_millis(250);
/// Time spent waiting on upstream before the single GET is judged. Time spent
/// waiting on a slow client does not count: that is not upstream's fault.
const GATE: Duration = Duration::from_secs(1);
/// Too little left to be worth new connections.
const MIN_REMAINING: u64 = 1024 * 1024;
const SPAN: u64 = 1024 * 1024;
const FLOWS: usize = 4;
/// Spans a flow may work on ahead of the client. Bounds buffered memory.
const WINDOW_SPANS: usize = 8;
/// A second flow may take over a span somebody is still working on.
const MAX_OWNERS: u8 = 2;
const CONNECT_LIMIT: Duration = Duration::from_millis(2500);
const FIRST_BYTE_LIMIT: Duration = Duration::from_millis(2500);
/// A flow must have run this long before its speed is judged.
const FLOW_GRACE: Duration = Duration::from_secs(1);
/// Connections that may be dropped for speed, plus one for each span delivered.
/// When every connection is slow, keep what there is instead of churning.
const DROP_BUDGET: u64 = 24;
const MAX_FAILURES: u32 = 6;

pub struct Download {
    pub state: Arc<State>,
    pub host: String,
    pub uri: hyper::Uri,
    pub headers: HeaderMap,
    pub etag: String,
    pub total: u64,
    pub route: Route,
}
#[derive(Default)]
pub struct Stats {
    triggered: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
    flows: AtomicU64,
    dropped: AtomicU64,
}
impl Stats {
    pub fn snapshot(&self) -> serde_json::Value {
        let get = |value: &AtomicU64| value.load(Ordering::Relaxed);
        serde_json::json!({"triggered":get(&self.triggered),"completed":get(&self.completed),"failed":get(&self.failed),"flows":get(&self.flows),"dropped":get(&self.dropped)})
    }
}
struct Span {
    end: u64,
    /// Everything before this offset has been received.
    pos: u64,
    owners: u8,
    /// Received but not yet handed to the client.
    buffer: BytesMut,
}
struct Table {
    base: u64,
    spans: Vec<Span>,
    head: usize,
}
enum Claim {
    Span(usize, u64, u64),
    Wait,
    Done,
}
impl Table {
    fn new(base: u64, total: u64) -> Self {
        let mut spans = Vec::new();
        let mut start = base;
        while start < total {
            let end = (start + SPAN).min(total);
            spans.push(Span {
                end,
                pos: start,
                owners: 0,
                buffer: BytesMut::new(),
            });
            start = end;
        }
        Self {
            base,
            spans,
            head: 0,
        }
    }
    fn window_end(&self) -> usize {
        (self.head + WINDOW_SPANS).min(self.spans.len())
    }
    fn claim(&mut self) -> Claim {
        let (head, end) = (self.head, self.window_end());
        if self.spans[head..].iter().all(|span| span.pos >= span.end) {
            return Claim::Done;
        }
        let open = |span: &Span| span.pos < span.end;
        let index = (head..end)
            .find(|i| open(&self.spans[*i]) && self.spans[*i].owners == 0)
            // Nothing untouched: help whichever unfinished span the client is
            // nearest to, since that is the one holding up its output.
            .or_else(|| {
                (head..end).find(|i| open(&self.spans[*i]) && self.spans[*i].owners < MAX_OWNERS)
            });
        match index {
            Some(index) => {
                let span = &mut self.spans[index];
                span.owners += 1;
                Claim::Span(index, span.pos, span.end)
            }
            None => Claim::Wait,
        }
    }
    fn release(&mut self, index: usize) {
        self.spans[index].owners -= 1;
    }
    /// Store bytes that begin at `at`. Flows may overlap; each byte is kept once.
    fn write(&mut self, mut at: u64, mut data: &[u8]) {
        while !data.is_empty() {
            let index = ((at - self.base) / SPAN) as usize;
            let Some(span) = self.spans.get_mut(index) else {
                return;
            };
            let take = data.len().min((span.end - at) as usize);
            let (piece, rest) = data.split_at(take);
            let piece_end = at + take as u64;
            if at <= span.pos && piece_end > span.pos {
                span.buffer
                    .extend_from_slice(&piece[(span.pos - at) as usize..]);
                span.pos = piece_end;
            }
            at = piece_end;
            data = rest;
        }
    }
    /// Next bytes for the client in order, and whether the whole file is delivered.
    fn take(&mut self) -> (Option<Bytes>, bool) {
        while let Some(span) = self.spans.get_mut(self.head) {
            if !span.buffer.is_empty() {
                let bytes = span.buffer.split().freeze();
                span.buffer = BytesMut::new();
                return (Some(bytes), false);
            }
            if span.pos < span.end {
                return (None, false);
            }
            self.head += 1;
        }
        (None, true)
    }
    fn span_done(&self, index: usize) -> bool {
        let span = &self.spans[index];
        span.pos >= span.end
    }
    fn within_window(&self, at: u64) -> bool {
        (((at - self.base) / SPAN) as usize) < self.head + WINDOW_SPANS
    }
}
struct Ctx {
    download: Download,
    routes: Vec<Route>,
    table: std::sync::Mutex<Table>,
    wake: tokio::sync::Notify,
    /// A Range reply has been verified, so the original GET is no longer needed.
    validated: AtomicBool,
    /// The server does not honour Range for this entity. Never splice from it.
    refused: AtomicBool,
    dropped: AtomicU64,
}
impl Ctx {
    fn may_drop(&self) -> bool {
        let delivered = self.table.lock().unwrap().head as u64;
        self.dropped.load(Ordering::Relaxed) < DROP_BUDGET + delivered
    }
    fn note_drop(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        self.download
            .state
            .rescue
            .dropped
            .fetch_add(1, Ordering::Relaxed);
    }
    fn write(&self, at: u64, data: &[u8]) {
        self.table.lock().unwrap().write(at, data);
        self.wake.notify_one();
    }
}

pub fn start(download: Download, body: HttpBody, lease: Lease) -> HttpBody {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>>>(4);
    tokio::spawn(single(download, body, lease, tx));
    StreamBody::new(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
    .boxed_unsync()
}

/// The healthy path: forward the original response and watch its speed.
async fn single(
    download: Download,
    mut body: HttpBody,
    lease: Lease,
    tx: tokio::sync::mpsc::Sender<Result<Frame<Bytes>>>,
) {
    let started = Instant::now();
    let (mut sent, mut window_bytes) = (0u64, 0u64);
    let (mut window_wait, mut total_wait) = (Duration::ZERO, Duration::ZERO);
    let state = download.state.clone();
    let learn = state.optimization().adaptive_routing;
    loop {
        if tx.is_closed() {
            if routing::slow_flow(sent, total_wait) {
                lease.retire();
            }
            return;
        }
        let before = Instant::now();
        let next = timeout(TICK, body.frame()).await;
        window_wait += before.elapsed();
        total_wait += before.elapsed();
        match next {
            Err(_) => {}
            Ok(None) => {
                if learn {
                    state.performance.record(
                        &download.host,
                        &download.route,
                        sent,
                        started.elapsed(),
                        true,
                    );
                }
                return;
            }
            Ok(Some(Err(error))) => {
                // The original connection died. With validators in hand the rest
                // of the file can still be fetched instead of failing the client.
                lease.retire();
                if learn {
                    state.performance.record(
                        &download.host,
                        &download.route,
                        sent,
                        started.elapsed(),
                        false,
                    );
                    state
                        .health
                        .fail(&download.host, &download.route, health::Failure::Network);
                }
                match state.split_slots.clone().try_acquire_owned() {
                    Ok(permit) if sent < download.total => {
                        rescue(download, None, sent, tx, permit).await;
                    }
                    _ => {
                        let _ = tx.send(Err(error)).await;
                    }
                }
                return;
            }
            Ok(Some(Ok(frame))) => {
                let len = frame.data_ref().map_or(0, |data| data.len() as u64);
                if tx.send(Ok(frame)).await.is_err() {
                    if routing::slow_flow(sent, total_wait) {
                        lease.retire();
                    }
                    return;
                }
                sent += len;
                window_bytes += len;
            }
        }
        if window_wait >= GATE {
            let rate = window_bytes as f64 / window_wait.as_secs_f64();
            window_bytes = 0;
            window_wait = Duration::ZERO;
            if rate < SLOW_FLOW_BYTES_PER_SECOND && download.total - sent >= MIN_REMAINING {
                if let Ok(permit) = state.split_slots.clone().try_acquire_owned() {
                    rescue(download, Some((body, lease)), sent, tx, permit).await;
                    return;
                }
            }
        }
    }
}

/// Addresses to spread new connections over, beginning with the one already in use.
async fn flow_routes(download: &Download) -> Vec<Route> {
    let mut routes = vec![download.route.clone()];
    let state = &download.state;
    if state.config.ip_overrides.contains_key(&download.host) {
        return routes;
    }
    let (ipv4, no_sni) = match &download.route {
        Route::Direct(address) => (address.is_ipv4(), false),
        Route::DirectNoSni(address) => (address.is_ipv4(), true),
        Route::Proxy => return routes,
    };
    if let Ok(Ok(ips)) = timeout(
        Duration::from_millis(500),
        state.resolver.resolve(&download.host),
    )
    .await
    {
        for ip in ips.into_iter().filter(|ip| ip.is_ipv4() == ipv4) {
            let address = SocketAddr::new(ip, 443);
            let route = if no_sni {
                Route::DirectNoSni(address)
            } else {
                Route::Direct(address)
            };
            if !routes.contains(&route) {
                routes.push(route);
            }
        }
    }
    routes
}

async fn rescue(
    download: Download,
    original: Option<(HttpBody, Lease)>,
    base: u64,
    tx: tokio::sync::mpsc::Sender<Result<Frame<Bytes>>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
    let state = download.state.clone();
    state.rescue.triggered.fetch_add(1, Ordering::Relaxed);
    state.health.event(&download.host, "flow-rescue");
    let routes = flow_routes(&download).await;
    let table = Table::new(base, download.total);
    let ctx = Arc::new(Ctx {
        download,
        routes,
        table: std::sync::Mutex::new(table),
        wake: tokio::sync::Notify::new(),
        validated: AtomicBool::new(false),
        refused: AtomicBool::new(false),
        dropped: AtomicU64::new(0),
    });
    // Dropping the set aborts every flow, including when the client goes away.
    let mut jobs = JoinSet::new();
    for index in 0..FLOWS {
        jobs.spawn(worker(ctx.clone(), index));
    }
    if let Some((body, lease)) = original {
        jobs.spawn(reader(ctx.clone(), body, lease, base));
    }
    loop {
        // Read this first: flows finishing after this point leave data for take().
        let exhausted = jobs.is_empty();
        let (data, done) = ctx.table.lock().unwrap().take();
        if let Some(data) = data {
            if tx.send(Ok(Frame::data(data))).await.is_err() {
                return;
            }
            continue;
        }
        if done {
            state.rescue.completed.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if exhausted {
            // Ending short lets the client's own resume take over.
            state.rescue.failed.fetch_add(1, Ordering::Relaxed);
            let _ = tx
                .send(Err(err(
                    "no usable connection for the rest of the download",
                )))
                .await;
            return;
        }
        tokio::select! {
            _ = ctx.wake.notified() => {}
            _ = jobs.join_next() => {}
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            _ = tx.closed() => return,
        }
    }
}

/// Keep the original GET going until a Range reply proves it can be replaced, so a
/// server that refuses Range still delivers the file over the connection it has.
async fn reader(ctx: Arc<Ctx>, mut body: HttpBody, lease: Lease, mut at: u64) {
    let total = ctx.download.total;
    while at < total {
        if ctx.validated.load(Ordering::Relaxed) {
            break;
        }
        if !ctx.table.lock().unwrap().within_window(at) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            continue;
        }
        match timeout(TICK, body.frame()).await {
            Err(_) => {}
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    ctx.write(at, data);
                    at += data.len() as u64;
                }
            }
            Ok(_) => break,
        }
    }
    lease.retire();
}

enum Outcome {
    /// The span is finished and the connection is good for another.
    Finished,
    /// Someone else finished the span first.
    Overtaken,
    /// The connection was too slow to keep.
    Slow,
    Failed,
    /// The server is not honouring Range for this entity.
    Refused,
}

async fn worker(ctx: Arc<Ctx>, index: usize) {
    let mut flow = None;
    let mut next_route = index;
    let mut failures = 0;
    while !ctx.refused.load(Ordering::Relaxed) {
        let claim = ctx.table.lock().unwrap().claim();
        let (span, start, end) = match claim {
            Claim::Done => return,
            Claim::Wait => {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            Claim::Span(span, start, end) => (span, start, end),
        };
        let outcome = fetch(&ctx, &mut flow, &mut next_route, span, start, end).await;
        ctx.table.lock().unwrap().release(span);
        match outcome {
            Outcome::Finished => failures = 0,
            Outcome::Overtaken => {
                failures = 0;
                // Abandoning a body ends an HTTP/1.1 connection but only one
                // stream of an HTTP/2 connection.
                if flow.as_ref().is_some_and(|f: &Lease| f.protocol() != "h2") {
                    flow = None;
                }
            }
            Outcome::Slow => {
                failures = 0;
                flow = None;
                ctx.note_drop();
            }
            Outcome::Failed => {
                flow = None;
                failures += 1;
                if failures >= MAX_FAILURES {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100 * failures as u64)).await;
            }
            Outcome::Refused => {
                ctx.refused.store(true, Ordering::Relaxed);
                return;
            }
        }
    }
}

fn range_request(download: &Download, start: u64, end: u64) -> Result<Request<HttpBody>> {
    let mut request = Request::new(
        Full::new(Bytes::new())
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *request.uri_mut() = download.uri.clone();
    *request.headers_mut() = download.headers.clone();
    let headers = request.headers_mut();
    for key in [
        "if-none-match",
        "if-modified-since",
        "content-length",
        "transfer-encoding",
        "connection",
    ] {
        headers.remove(key);
    }
    headers.insert("host", download.host.parse()?);
    headers.insert("accept-encoding", "identity".parse()?);
    headers.insert("range", format!("bytes={start}-{}", end - 1).parse()?);
    headers.insert("if-range", download.etag.parse()?);
    Ok(request)
}

async fn fetch(
    ctx: &Ctx,
    flow: &mut Option<Lease>,
    next_route: &mut usize,
    span: usize,
    start: u64,
    end: u64,
) -> Outcome {
    let download = &ctx.download;
    let state = &download.state;
    if flow.as_ref().is_some_and(Lease::is_closed) {
        *flow = None;
    }
    if flow.is_none() {
        let route = &ctx.routes[*next_route % ctx.routes.len()];
        *next_route += 1;
        match timeout(CONNECT_LIMIT, state.pool.flow(state, &download.host, route)).await {
            Ok(Ok(lease)) => *flow = Some(lease),
            _ => return Outcome::Failed,
        }
        state.rescue.flows.fetch_add(1, Ordering::Relaxed);
    }
    let lease = flow.as_ref().unwrap();
    let Ok(request) = range_request(download, start, end) else {
        return Outcome::Failed;
    };
    let reply = if ctx.may_drop() {
        match timeout(FIRST_BYTE_LIMIT, lease.request(&download.host, request)).await {
            Ok(reply) => reply,
            Err(_) => return Outcome::Slow,
        }
    } else {
        lease.request(&download.host, request).await
    };
    let Ok(response) = reply else {
        return Outcome::Failed;
    };
    let status = response.status();
    if status != StatusCode::PARTIAL_CONTENT {
        // 200 means If-Range failed: the entity changed or Range is unsupported.
        // Anything else may be temporary, such as a CDN error on this connection.
        return if status == StatusCode::OK {
            Outcome::Refused
        } else {
            Outcome::Failed
        };
    }
    let headers = response.headers();
    let exact = headers
        .get("content-range")
        .and_then(|value| value.to_str().ok())
        .and_then(segmented::interval)
        == Some((start, end - 1, download.total));
    let same_entity =
        headers.get("etag").and_then(|value| value.to_str().ok()) == Some(download.etag.as_str());
    let encoded = headers
        .get("content-encoding")
        .is_some_and(|value| value != "identity");
    if !exact || !same_entity || encoded {
        return Outcome::Refused;
    }
    ctx.validated.store(true, Ordering::Relaxed);
    let mut body = response.into_body();
    let began = Instant::now();
    let mut at = start;
    loop {
        if at < end && ctx.table.lock().unwrap().span_done(span) {
            return Outcome::Overtaken;
        }
        match timeout(TICK, body.frame()).await {
            Err(_) => {}
            Ok(None) => {
                return if at == end {
                    Outcome::Finished
                } else {
                    Outcome::Failed
                };
            }
            Ok(Some(Err(_))) => return Outcome::Failed,
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    if data.len() as u64 > end - at {
                        return Outcome::Refused;
                    }
                    ctx.write(at, data);
                    at += data.len() as u64;
                }
            }
        }
        let elapsed = began.elapsed();
        if at < end
            && elapsed >= FLOW_GRACE
            && ((at - start) as f64 / elapsed.as_secs_f64()) < SLOW_FLOW_BYTES_PER_SECOND
            && ctx.may_drop()
        {
            return Outcome::Slow;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn drain(table: &mut Table) -> Vec<u8> {
        let mut out = Vec::new();
        while let (Some(bytes), _) = table.take() {
            out.extend_from_slice(&bytes);
        }
        out
    }
    #[test]
    fn overlapping_flows_keep_each_byte_once_and_in_order() {
        let total = 3 * SPAN + 10;
        let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        let base = 100;
        let mut table = Table::new(base, total);
        let at = |offset: u64| &data[offset as usize..];
        // A later span arrives first and must not be emitted ahead of the head.
        table.write(base + SPAN, &at(base + SPAN)[..SPAN as usize]);
        assert_eq!(table.take(), (None, false));
        // Two flows race over the head span, overlapping and out of step.
        table.write(base, &at(base)[..1000]);
        table.write(base + 500, &at(base + 500)[..1000]);
        table.write(base, &at(base)[..200]);
        let first = drain(&mut table);
        assert_eq!(first, at(base)[..1500]);
        // A write that would leave a gap is ignored, never stored out of place.
        table.write(base + 2000, &at(base + 2000)[..10]);
        assert_eq!(table.take(), (None, false));
        // One write may span a boundary, as the original GET's frames do.
        table.write(base + 1500, &at(base + 1500)[..(SPAN - 1500) as usize]);
        table.write(base + 2 * SPAN, at(base + 2 * SPAN));
        let rest = drain(&mut table);
        assert_eq!(rest, at(base + 1500));
        assert_eq!(table.take(), (None, true));
    }
    #[test]
    fn claims_prefer_untouched_spans_then_help_the_client_facing_one() {
        let mut table = Table::new(0, 3 * SPAN);
        let span = |claim| match claim {
            Claim::Span(index, ..) => index,
            _ => panic!("expected a span"),
        };
        assert_eq!(span(table.claim()), 0);
        assert_eq!(span(table.claim()), 1);
        assert_eq!(span(table.claim()), 2);
        assert_eq!(span(table.claim()), 0, "duplicate the head span first");
        assert_eq!(span(table.claim()), 1);
        assert_eq!(span(table.claim()), 2);
        assert!(matches!(table.claim(), Claim::Wait));
        table.release(1);
        assert_eq!(span(table.claim()), 1);
    }
    #[test]
    fn claims_stay_inside_the_window_until_the_client_catches_up() {
        let mut table = Table::new(0, 20 * SPAN);
        let data = vec![0u8; SPAN as usize];
        for _ in 0..WINDOW_SPANS * MAX_OWNERS as usize {
            assert!(matches!(table.claim(), Claim::Span(index, ..) if index < WINDOW_SPANS));
        }
        assert!(matches!(table.claim(), Claim::Wait));
        assert!(table.within_window(0));
        assert!(!table.within_window(WINDOW_SPANS as u64 * SPAN));
        table.write(0, &data);
        drain(&mut table);
        table.take();
        assert!(matches!(table.claim(), Claim::Span(WINDOW_SPANS, ..)));
    }
}
