//! anthroxy — a transparent rate-limiting + retry reverse proxy in front of api.anthropic.com.
//!
//! Point Claude Code at it with `ANTHROPIC_BASE_URL=http://127.0.0.1:8787`. It forwards every
//! request VERBATIM (the subscription OAuth bearer + anthropic-beta/version headers, body opaque —
//! never decoded), paces request STARTS to dodge Anthropic's burst/acceleration limit, and absorbs
//! 429/5xx with backoff so the agent never sees the throttle. Single in-memory instance.
//!
//! Tunables (env): ANTHROXY_BIND (default 127.0.0.1), ANTHROXY_PORT (8787),
//! ANTHROXY_MAX_CONCURRENCY (3), ANTHROXY_MIN_INTERVAL_MS (350), ANTHROXY_MAX_RETRIES (8),
//! ANTHROXY_BASE_BACKOFF_MS (500).

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
use tokio::sync::{Mutex, Semaphore};
use tokio::time::{sleep, Instant};

struct Cfg {
    upstream: String,
    max_concurrency: usize,
    min_interval: Duration,
    max_retries: u32,
    base_backoff: Duration,
}

type ProxyBody = BoxBody<Bytes, std::io::Error>;

#[tokio::main]
async fn main() {
    let bind = env::var("ANTHROXY_BIND").unwrap_or_else(|_| "127.0.0.1".into());
    let port: u16 = env_parse("ANTHROXY_PORT", 8787u16);
    let cfg = Arc::new(Cfg {
        upstream: env::var("ANTHROXY_UPSTREAM").unwrap_or_else(|_| "https://api.anthropic.com".into()),
        max_concurrency: env_parse("ANTHROXY_MAX_CONCURRENCY", 3usize),
        min_interval: Duration::from_millis(env_parse("ANTHROXY_MIN_INTERVAL_MS", 350u64)),
        max_retries: env_parse("ANTHROXY_MAX_RETRIES", 8u32),
        base_backoff: Duration::from_millis(env_parse("ANTHROXY_BASE_BACKOFF_MS", 500u64)),
    });

    let sem = Arc::new(Semaphore::new(cfg.max_concurrency));
    let last_start = Arc::new(Mutex::new(Instant::now())); // first request may wait up to min_interval
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none()) // pure passthrough — never follow redirects
        .build()
        .expect("reqwest client");

    let addr: SocketAddr = format!("{bind}:{port}").parse().expect("bind address");
    let listener = TcpListener::bind(addr).await.expect("bind");
    eprintln!(
        "anthroxy: http://{addr} -> {}  (concurrency={}, min_interval={:?}, max_retries={})",
        cfg.upstream, cfg.max_concurrency, cfg.min_interval, cfg.max_retries
    );

    loop {
        let (tcp, _) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => continue,
        };
        let io = TokioIo::new(tcp);
        let (client, sem, last_start, cfg) =
            (client.clone(), sem.clone(), last_start.clone(), cfg.clone());
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                handle(req, client.clone(), sem.clone(), last_start.clone(), cfg.clone())
            });
            let _ = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await;
        });
    }
}

async fn handle(
    req: Request<Incoming>,
    client: reqwest::Client,
    sem: Arc<Semaphore>,
    last_start: Arc<Mutex<Instant>>,
    cfg: Arc<Cfg>,
) -> Result<Response<ProxyBody>, Infallible> {
    let _permit = sem.acquire().await.expect("semaphore closed"); // concurrency cap

    // pace request STARTS — the acceleration limit is rate-of-onset, not steady volume
    {
        let mut last = last_start.lock().await;
        let elapsed = Instant::now().saturating_duration_since(*last);
        if elapsed < cfg.min_interval {
            sleep(cfg.min_interval - elapsed).await;
        }
        *last = Instant::now();
    }

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
                if attempt < cfg.max_retries {
                    backoff(&cfg, attempt, None).await; // connect error → treat like 5xx
                    attempt += 1;
                    continue;
                }
                return Ok(msg(502, "anthroxy: upstream unreachable"));
            }
        };

        eprintln!("anthroxy: {} {} -> {} (attempt {})", parts.method, path, upstream.status().as_u16(), attempt);

        // the core dispatch: a literal match on the upstream status code
        match upstream.status().as_u16() {
            200 => return Ok(stream_back(upstream)),          // success → stream straight through
            429 => {
                if attempt < cfg.max_retries {
                    backoff(&cfg, attempt, retry_after(&upstream)).await; // honor Retry-After
                    attempt += 1;
                    continue;
                }
                return Ok(stream_back(upstream)); // retries spent → surface the throttle
            }
            500..=599 => {
                if attempt < cfg.max_retries {
                    backoff(&cfg, attempt, None).await; // same backoff, no Retry-After
                    attempt += 1;
                    continue;
                }
                return Ok(stream_back(upstream));
            }
            _ => return Ok(stream_back(upstream)), // other 2xx/3xx + real 4xx → pass through as-is
        }
    }
}

/// Wrap the upstream response as a streaming hyper response — body flows through unbuffered (SSE).
fn stream_back(upstream: reqwest::Response) -> Response<ProxyBody> {
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
    let stream = upstream
        .bytes_stream()
        .map_ok(Frame::data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
    builder.body(StreamBody::new(stream).boxed()).unwrap()
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
    let exp = cfg.base_backoff.as_millis() as u64 * 2u64.pow(attempt.min(6));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as u64;
    let jitter = nanos % (exp / 2 + 1); // cheap jitter, no rand dep
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
