//! Race a Git fetch over two connections and keep whichever delivers first.
//!
//! Flow rescue cannot help a clone: `git-upload-pack` is a POST whose reply has no
//! ETag and no Range support, so a slow connection cannot be continued elsewhere.
//! What can still be done is choosing well at the start, and connection speed is
//! only known by trying. So the small request body is sent over two new
//! connections to different addresses. The first reply to deliver `DECIDE` bytes,
//! or all of itself, is kept and the other connection is closed. The client always
//! receives one complete upstream reply, never a mix of two.
//!
//! The cost is that GitHub runs the request twice and up to `DECIDE` bytes are
//! downloaded twice, which is why this is off by default.
use super::*;
use crate::{forward::HttpBody, pool::Lease};
use bytes::{Bytes, BytesMut};
use http_body_util::{BodyExt, Full};
use hyper::{
    body::{Body, Frame, Incoming},
    HeaderMap, Method, Request, Response, StatusCode,
};

/// Larger request bodies are not buffered; git streams those in chunks anyway.
pub const MAX_REQUEST: u64 = 1024 * 1024;
const RACERS: usize = 2;
/// Bytes a reply must deliver to win. Enough to be past the server's progress
/// messages and into the pack itself, small enough to cap the duplicate download.
const DECIDE: usize = 1024 * 1024;
const CONNECT_LIMIT: Duration = Duration::from_secs(4);
/// GitHub may count objects for a while before sending headers.
const HEADER_LIMIT: Duration = Duration::from_secs(60);
/// A reply that sends nothing for this long has stalled.
const STALL_LIMIT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Stats {
    started: AtomicU64,
    kept_first: AtomicU64,
    kept_other: AtomicU64,
    failed: AtomicU64,
}
impl Stats {
    pub fn snapshot(&self) -> serde_json::Value {
        let get = |value: &AtomicU64| value.load(Ordering::Relaxed);
        serde_json::json!({"started":get(&self.started),"kept_first":get(&self.kept_first),"kept_other":get(&self.kept_other),"failed":get(&self.failed)})
    }
}

pub fn eligible(
    state: &State,
    host: &str,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
) -> bool {
    state.optimization().git_race
        && host == "github.com"
        && *method == Method::POST
        && path.ends_with("/git-upload-pack")
        && state.config.mode_for(host) != Mode::Proxy
        && headers
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|length| length <= MAX_REQUEST)
}

pub enum Outcome {
    /// Fewer than two usable addresses; send the request the ordinary way.
    NotStarted,
    Kept {
        response: Response<HttpBody>,
        lease: Lease,
    },
    /// No reply succeeded, but one gave a real answer the client should see.
    Refused {
        response: Response<Incoming>,
        lease: Lease,
    },
    Failed,
}

enum Finish {
    Ready {
        response: Response<HttpBody>,
        lease: Lease,
    },
    Refused {
        response: Response<Incoming>,
        lease: Lease,
    },
    Failed,
}

fn address(route: &Route) -> Option<SocketAddr> {
    match route {
        Route::Direct(a) | Route::DirectNoSni(a) => Some(*a),
        Route::Proxy => None,
    }
}

async fn routes(state: &Arc<State>, host: &str) -> Vec<Route> {
    let Ok(primary) = state.select_for(host, Traffic::Bulk).await else {
        return Vec::new();
    };
    let Some(first) = address(&primary) else {
        return Vec::new();
    };
    let mut used = vec![first];
    let mut routes = vec![primary.clone()];
    for route in state
        .alternatives(host, Some(&primary), Traffic::Bulk)
        .await
    {
        if routes.len() == RACERS {
            break;
        }
        // Two connections to one address say little about each other.
        if let Some(a) = address(&route).filter(|a| !used.contains(a)) {
            used.push(a);
            routes.push(route);
        }
    }
    routes
}

/// Only a request for objects can bring a pack worth racing for. A ref listing
/// (protocol v2 `ls-refs`) is small, and two new connections only slow it down.
/// A compressed body cannot be read here; git compresses only larger ones.
pub fn wants_objects(headers: &HeaderMap, body: &[u8]) -> bool {
    headers.contains_key(hyper::header::CONTENT_ENCODING) || body.windows(5).any(|w| w == b"want ")
}

pub async fn upload_pack(
    state: &Arc<State>,
    host: &str,
    uri: &hyper::Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> Outcome {
    if !wants_objects(headers, &body) {
        return Outcome::NotStarted;
    }
    let routes = routes(state, host).await;
    if routes.len() < 2 {
        return Outcome::NotStarted;
    }
    contest(state, host, routes, uri, headers, body).await
}

/// Send the request over each route at once. `routes[0]` is the usual choice.
pub async fn contest(
    state: &Arc<State>,
    host: &str,
    routes: Vec<Route>,
    uri: &hyper::Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> Outcome {
    state.race.started.fetch_add(1, Ordering::Relaxed);
    let mut set = tokio::task::JoinSet::new();
    for (index, route) in routes.into_iter().enumerate() {
        let (state, host, uri, headers, body) = (
            state.clone(),
            host.to_owned(),
            uri.clone(),
            headers.clone(),
            body.clone(),
        );
        set.spawn(async move { (index, run(state, host, route, uri, headers, body).await) });
    }
    let mut refused = None;
    while let Some(joined) = set.join_next().await {
        let Ok((index, finish)) = joined else {
            continue;
        };
        match finish {
            Finish::Ready { response, lease } => {
                let counter = if index == 0 {
                    &state.race.kept_first
                } else {
                    &state.race.kept_other
                };
                counter.fetch_add(1, Ordering::Relaxed);
                state.health.event(host, "git-race-kept");
                // Dropping the set aborts the others, which closes their connections.
                return Outcome::Kept { response, lease };
            }
            Finish::Refused { response, lease } => {
                refused.get_or_insert((response, lease));
            }
            Finish::Failed => {}
        }
    }
    state.race.failed.fetch_add(1, Ordering::Relaxed);
    match refused {
        Some((response, lease)) => Outcome::Refused { response, lease },
        None => Outcome::Failed,
    }
}

async fn run(
    state: Arc<State>,
    host: String,
    route: Route,
    uri: hyper::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Finish {
    let lease = match timeout(CONNECT_LIMIT, state.pool.flow(&state, &host, &route)).await {
        Ok(Ok(lease)) => lease,
        _ => {
            state.health.fail(&host, &route, health::Failure::Network);
            return Finish::Failed;
        }
    };
    let mut request = Request::new(
        Full::new(body)
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *request.method_mut() = Method::POST;
    *request.uri_mut() = uri;
    *request.headers_mut() = headers;
    *request.version_mut() = hyper::Version::HTTP_11;
    let response = match timeout(HEADER_LIMIT, lease.request(&host, request)).await {
        Ok(Ok(response)) => response,
        _ => {
            state.health.fail(&host, &route, health::Failure::Network);
            return Finish::Failed;
        }
    };
    let status = response.status();
    if status == StatusCode::MISDIRECTED_REQUEST
        || routing::wrong_edge(&host, status.as_u16(), response.headers())
    {
        state.health.fail(&host, &route, health::Failure::Rejected);
        return Finish::Failed;
    }
    if status != StatusCode::OK {
        if status == StatusCode::TOO_MANY_REQUESTS {
            state.health.fail(&host, &route, health::Failure::Throttled);
        } else if status.is_server_error() {
            state.health.fail(&host, &route, health::Failure::Server);
        }
        return Finish::Refused { response, lease };
    }
    let (parts, mut rest) = response.into_parts();
    let mut prefix = BytesMut::new();
    let finished = loop {
        match timeout(STALL_LIMIT, rest.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    prefix.extend_from_slice(&data);
                    if prefix.len() >= DECIDE {
                        break false;
                    }
                }
            }
            Ok(None) => break true,
            _ => {
                state.health.fail(&host, &route, health::Failure::Network);
                return Finish::Failed;
            }
        }
    };
    let body = Prefixed {
        prefix: Some(prefix.freeze()).filter(|b| !b.is_empty()),
        rest: (!finished).then_some(rest),
    };
    Finish::Ready {
        response: Response::from_parts(parts, body.boxed_unsync()),
        lease,
    }
}

/// What the winner already delivered, followed by the rest of its reply.
struct Prefixed {
    prefix: Option<Bytes>,
    rest: Option<Incoming>,
}
impl Body for Prefixed {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<std::result::Result<Frame<Bytes>, Self::Error>>> {
        if let Some(prefix) = self.prefix.take() {
            return std::task::Poll::Ready(Some(Ok(Frame::data(prefix))));
        }
        match self.rest.as_mut() {
            Some(rest) => std::pin::Pin::new(rest).poll_frame(cx).map_err(Into::into),
            None => std::task::Poll::Ready(None),
        }
    }
    fn is_end_stream(&self) -> bool {
        self.prefix.is_none() && self.rest.as_ref().is_none_or(|r| r.is_end_stream())
    }
}
