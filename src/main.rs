//! anthroxy — a transparent retry reverse proxy in front of api.anthropic.com.
//!
//! Point Claude Code at it with `ANTHROPIC_BASE_URL=http://127.0.0.1:8787`. It forwards every
//! request VERBATIM (the subscription OAuth bearer + anthropic-beta/version headers, body opaque —
//! never decoded). Its one job: when Anthropic returns 429 (burst/acceleration throttle) or 5xx,
//! retry — honoring Retry-After, exponential backoff in between — for up to a wall-clock window,
//! so the agent never errors out on a transient throttle. Claude Code does NOT ride out a 429 on
//! its own; the agent halts and needs a human to restart it. This makes the throttle invisible.
//!
//! It does NOT anticipate the limit — no concurrency cap, no start-pacing. Measurement showed
//! request-concurrency is not the constraint (100+ parallel agents run fine), so anything proactive
//! was overreach. React to the 429; don't try to predict it.
//!
//! Tunables (env): ANTHROXY_BIND (default 127.0.0.1), ANTHROXY_PORT (8787),
//! ANTHROXY_MAX_RETRY_WINDOW_S (600 — wall-clock budget to keep retrying 429/5xx/connect errors),
//! ANTHROXY_BASE_BACKOFF_MS (500), ANTHROXY_DEBUG (off — when on, scans RESPONSE frames for error
//! markers and logs only the matching frame; never logs request bodies or normal content, so it's
//! safe with huge context windows. Catches rate-limit/overload errors that arrive inside a 200 SSE
//! stream rather than as an HTTP status).

use std::{convert::Infallible, env, net::SocketAddr, sync::Arc, time::Duration,
          time::{SystemTime, UNIX_EPOCH}};

use bytes::Bytes;
use futures_util::TryStreamExt;
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use tokio::net::TcpListener;
use tokio::time::{sleep, Instant};

struct Cfg {
    upstream: String,
    retry_window: Duration,
    base_backoff: Duration,
    debug: bool,
}

type ProxyBody = BoxBody<Bytes, std::io::Error>;

#[tokio::main]
async fn main() {
    let bind = env::var("ANTHROXY_BIND").unwrap_or_else(|_| "127.0.0.1".into());
    let port: u16 = env_parse("ANTHROXY_PORT", 8787u16);
    let cfg = Arc::new(Cfg {
        upstream: env::var("ANTHROXY_UPSTREAM").unwrap_or_else(|_| "https://api.anthropic.com".into()),
        retry_window: Duration::from_secs(env_parse("ANTHROXY_MAX_RETRY_WINDOW_S", 600u64)),
        base_backoff: Duration::from_millis(env_parse("ANTHROXY_BASE_BACKOFF_MS", 500u64)),
        debug: env::var("ANTHROXY_DEBUG").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false),
    });

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none()) // pure passthrough — never follow redirects
        .build()
        .expect("reqwest client");

    let addr: SocketAddr = format!("{bind}:{port}").parse().expect("bind address");
    let listener = TcpListener::bind(addr).await.expect("bind");
    eprintln!(
        "anthroxy: http://{addr} -> {}  (retry_window={:?}, base_backoff={:?}, debug={})",
        cfg.upstream, cfg.retry_window, cfg.base_backoff, cfg.debug
    );

    loop {
        let (tcp, _) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => continue,
        };
        let io = TokioIo::new(tcp);
        let (client, cfg) = (client.clone(), cfg.clone());
        tokio::spawn(async move {
            let svc = service_fn(move |req| handle(req, client.clone(), cfg.clone()));
            let _ = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await;
        });
    }
}

async fn handle(
    req: Request<Incoming>,
    client: reqwest::Client,
    cfg: Arc<Cfg>,
) -> Result<Response<ProxyBody>, Infallible> {
    let (parts, body) = req.into_parts();
    let body_bytes = match body.collect().await {
        Ok(c) => c.to_bytes(), // buffer the REQUEST body so a retry can re-send it
        Err(_) => return Ok(msg(400, "anthroxy: could not read request body")),
    };

    let path = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let url = format!("{}{}", cfg.upstream, path);

    // reqwest and hyper share the `http` crate's Method/HeaderMap, so forward VERBATIM
    // (drop only hop-by-hop). No body decode/re-encode — that's where re-serializers mangle betas.
    let mut headers = parts.headers.clone();
    headers.remove(hyper::header::HOST);
    headers.remove(hyper::header::CONNECTION);
    headers.remove(hyper::header::CONTENT_LENGTH);

    // Keep retrying 429 / 5xx / connect-errors until this wall-clock deadline, so a transient
    // throttle or blip never surfaces to the agent as a hard error (which would halt it). A
    // persistent failure (real quota, a long outage) costs us the window, then passes through.
    let deadline = Instant::now() + cfg.retry_window;
    let mut attempt: u32 = 0;
    loop {
        let sent = client
            .request(parts.method.clone(), &url)
            .headers(headers.clone())
            .body(body_bytes.clone())
            .send()
            .await;

        let upstream = match sent {
            Ok(r) => r,
            Err(_) => {
                if Instant::now() < deadline {
                    backoff(&cfg, attempt, None).await; // connect error → treat like 5xx
                    attempt += 1;
                    continue;
                }
                return Ok(msg(502, "anthroxy: upstream unreachable (retry window exhausted)"));
            }
        };

        eprintln!("anthroxy: {} {} -> {} (attempt {})", parts.method, path, upstream.status().as_u16(), attempt);

        // the core dispatch: a literal match on the upstream status code
        match upstream.status().as_u16() {
            429 => {
                if Instant::now() < deadline {
                    backoff(&cfg, attempt, retry_after(&upstream)).await; // honor Retry-After
                    attempt += 1;
                    continue;
                }
                return Ok(stream_back(upstream, cfg.debug, path)); // window exhausted → surface the throttle
            }
            500..=599 => {
                if Instant::now() < deadline {
                    backoff(&cfg, attempt, None).await; // same backoff, no Retry-After
                    attempt += 1;
                    continue;
                }
                return Ok(stream_back(upstream, cfg.debug, path));
            }
            _ => return Ok(stream_back(upstream, cfg.debug, path)), // 200/2xx/3xx + real 4xx → pass through as-is
        }
    }
}

/// Wrap the upstream response as a streaming hyper response — body flows through unbuffered (SSE).
/// When `debug`, each frame is scanned for error markers and only matching frames are logged.
fn stream_back(upstream: reqwest::Response, debug: bool, label: &str) -> Response<ProxyBody> {
    let mut builder = Response::builder().status(upstream.status());
    for (k, v) in upstream.headers().iter() {
        if k == hyper::header::CONNECTION
            || k == hyper::header::TRANSFER_ENCODING
            || k == hyper::header::CONTENT_LENGTH
        {
            continue;
        }
        builder = builder.header(k.clone(), v.clone());
    }
    let label = label.to_string();
    let stream = upstream
        .bytes_stream()
        .inspect_ok(move |chunk| {
            if debug {
                scan_for_error(&label, chunk);
            }
        })
        .map_ok(Frame::data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
    builder.body(StreamBody::new(stream).boxed()).unwrap()
}

/// Debug-only: scan ONE response frame for Anthropic error markers and log just that frame (capped).
/// Never touches request bodies or normal content deltas, so logs stay tiny even when requests carry
/// huge context windows. Catches rate-limit/overload errors delivered inside a 200 SSE stream as well
/// as non-200 error bodies. (A marker split exactly across two frames may be missed — fine for detection.)
fn scan_for_error(label: &str, chunk: &Bytes) {
    const MARKERS: [&[u8]; 4] = [
        b"\"type\":\"error\"",
        b"overloaded_error",
        b"rate_limit_error",
        b"event: error",
    ];
    if MARKERS.iter().any(|m| contains(chunk, m)) {
        let end = chunk.len().min(2048);
        eprintln!(
            "anthroxy[debug]: error frame on {} -> {}",
            label,
            String::from_utf8_lossy(&chunk[..end]).replace('\n', " ").trim()
        );
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    needle.len() <= hay.len() && hay.windows(needle.len()).any(|w| w == needle)
}

fn retry_after(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get(hyper::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

async fn backoff(cfg: &Cfg, attempt: u32, retry_after_s: Option<u64>) {
    let exp = cfg.base_backoff.as_millis() as u64 * 2u64.pow(attempt.min(6)); // cap growth at base*64
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as u64;
    let jitter = nanos % (exp / 2 + 1); // cheap jitter, no rand dep — decorrelates fleet retries
    let ra_ms = retry_after_s.map(|s| s.saturating_mul(1000)).unwrap_or(0);
    // Retry-After dominates when present; otherwise exponential. Never tight-loop (degraded-token risk).
    sleep(Duration::from_millis(ra_ms.max(exp) + jitter)).await;
}

fn msg(status: u16, text: &str) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(text.to_owned())).map_err(|e| match e {}).boxed())
        .unwrap()
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}
