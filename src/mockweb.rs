//! A deterministic synthetic web on localhost for tests and benchmarks: thousands of hosts
//! (`h{i}.mock`) served by one axum server, with robots.txt quirks, slow responses and rate
//! limits, and a log of every request so politeness can be checked from the server's side.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::fmt::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use url::Url;
use xxhash_rust::xxh3::xxh3_64_with_seed;

#[derive(Clone, Debug)]
pub struct MockConfig {
    pub hosts: u32,
    pub pages_per_host: u32,
    pub links_per_page: u32,
    pub page_words: usize,
    /// Response delay range in ms, inclusive. Each (host, path) always gets the same delay.
    pub latency_ms: (u64, u64),
    /// Share of links that point to another host, in percent.
    pub cross_host_pct: u32,
}

impl Default for MockConfig {
    /// 1,000 hosts × 500 pages of about 30 KB with 20 links each, answering in 20–200 ms.
    fn default() -> Self {
        Self {
            hosts: 1000,
            pages_per_host: 500,
            links_per_page: 20,
            page_words: 4000,
            latency_ms: (20, 200),
            cross_host_pct: 30,
        }
    }
}

/// How a host behaves, chosen by `host % 10`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Behavior {
    /// 0: robots.txt disallows `/private`, and the host's pages link there.
    DisallowPrivate,
    /// 1: robots.txt sets `Crawl-delay: 2`.
    CrawlDelay2,
    /// 2: robots.txt answers 503, so no page may be fetched.
    RobotsDown,
    /// 3: every 3rd page request gets `429` with `Retry-After: 2`.
    RateLimited,
    /// 4–9: no robots.txt (404), everything allowed.
    Normal,
}

pub fn behavior(host: u32) -> Behavior {
    match host % 10 {
        0 => Behavior::DisallowPrivate,
        1 => Behavior::CrawlDelay2,
        2 => Behavior::RobotsDown,
        3 => Behavior::RateLimited,
        _ => Behavior::Normal,
    }
}

pub fn host_name(i: u32) -> String {
    format!("h{i}.mock")
}

/// Inverse of `host_name`, for exactly the spelling it produces (`h01.mock` is not host 1).
pub fn host_index(name: &str) -> Option<u32> {
    let i = name
        .strip_prefix('h')?
        .strip_suffix(".mock")?
        .parse()
        .ok()?;
    (host_name(i) == name).then_some(i)
}

/// Status and body of the host's robots.txt.
pub fn robots_txt(host: u32) -> (u16, String) {
    let (status, body) = match behavior(host) {
        Behavior::DisallowPrivate => (200, "User-agent: *\nDisallow: /private\n"),
        Behavior::CrawlDelay2 => (200, "User-agent: *\nCrawl-delay: 2\n"),
        Behavior::RobotsDown => (503, "service unavailable\n"),
        Behavior::RateLimited | Behavior::Normal => (404, "not found\n"),
    };
    (status, body.to_owned())
}

/// 4,096 pseudo-words, so page text is built by copying instead of formatting each word.
static WORDS: LazyLock<Vec<String>> =
    LazyLock::new(|| (0..4096).map(|i| format!("w{i}")).collect());

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`; 0 when `n` is 0.
    fn below(&mut self, n: u32) -> u32 {
        (self.next() % u64::from(n.max(1))) as u32
    }

    fn word(&mut self) -> &'static str {
        &WORDS[self.below(4096) as usize]
    }
}

/// Page `page` of `host`, identical on every call. Every 10th page (`page % 10 == 9`) repeats
/// the page before it except for the title, so near-duplicate detection has something to find.
pub fn page_html(cfg: &MockConfig, port: u16, host: u32, page: u32) -> String {
    let src = if page % 10 == 9 { page - 1 } else { page };
    let mut rng = SplitMix64(xxh3_64_with_seed(&src.to_le_bytes(), u64::from(host)));
    let name = host_name(host);
    let mut html = String::with_capacity(cfg.page_words * 6 + cfg.links_per_page as usize * 64);
    let _ = write!(
        html,
        "<!doctype html><html><head><title>{name} page {page}</title></head>\n\
         <body><h1>{name}</h1>\n<p>"
    );
    // Links are spread through the text, one every `gap` words; any left over go at the end.
    let gap = (cfg.page_words / cfg.links_per_page.max(1) as usize).max(1);
    let mut links = 0;
    for i in 0..cfg.page_words {
        html.push_str(rng.word());
        html.push(' ');
        if i % gap == gap - 1 && links < cfg.links_per_page {
            push_link(&mut html, cfg, port, host, &mut rng);
            links += 1;
        }
        if i % 100 == 99 {
            html.push_str("</p>\n<p>");
        }
    }
    for _ in links..cfg.links_per_page {
        push_link(&mut html, cfg, port, host, &mut rng);
    }
    html.push_str("</p>\n</body></html>\n");
    html
}

fn push_link(html: &mut String, cfg: &MockConfig, port: u16, host: u32, rng: &mut SplitMix64) {
    let n = rng.below(cfg.pages_per_host);
    let href = if rng.below(100) < cfg.cross_host_pct {
        format!("http://{}:{port}/p/{n}", host_name(rng.below(cfg.hosts)))
    } else if behavior(host) == Behavior::DisallowPrivate && rng.below(4) == 0 {
        format!("/private/{n}")
    } else {
        format!("/p/{n}")
    };
    let (a, b) = (rng.word(), rng.word());
    let _ = write!(html, "<a href=\"{href}\">{a} {b}</a> ");
}

/// Upper bound on a polite crawl's speed in pages/s: each host whose robots.txt is up allows
/// one request per `max(min_delay, Crawl-delay)`. Benchmarks report throughput against it.
pub fn politeness_ceiling(cfg: &MockConfig, min_delay: Duration) -> f64 {
    (0..cfg.hosts)
        .map(|h| match behavior(h) {
            Behavior::RobotsDown => 0.0,
            Behavior::CrawlDelay2 => 1.0 / min_delay.max(Duration::from_secs(2)).as_secs_f64(),
            _ => 1.0 / min_delay.as_secs_f64(),
        })
        .sum()
}

/// One request as the server saw it. `at` is when it arrived, before any latency.
#[derive(Clone, Debug)]
pub struct Hit {
    pub host: String,
    pub path: String,
    pub at: Instant,
    pub status: u16,
}

struct Shared {
    cfg: MockConfig,
    port: u16,
    /// Page requests per host, for the rate limit.
    page_requests: Vec<AtomicU32>,
    // ponytail: grows by ~100 B per request with no cap; fine for tests and benches, cap it
    // before running the server for hours.
    hits: Mutex<Vec<Hit>>,
    big: Bytes,
}

pub struct MockWeb {
    pub addr: SocketAddr,
    shared: Arc<Shared>,
}

/// Serve on `127.0.0.1:port` (0 picks a free port) until the tokio runtime shuts down.
pub async fn start(cfg: MockConfig, port: u16) -> std::io::Result<MockWeb> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let addr = listener.local_addr()?;
    let shared = Arc::new(Shared {
        page_requests: (0..cfg.hosts).map(|_| AtomicU32::new(0)).collect(),
        hits: Mutex::default(),
        big: Bytes::from(format!(
            "<html><body><p>{}</p></body></html>",
            "big ".repeat(3 << 18) // 3 MiB, past the fetcher's 2 MiB cap
        )),
        port: addr.port(),
        cfg,
    });
    let app = Router::new()
        .fallback(handle)
        .with_state(Arc::clone(&shared));
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok(MockWeb { addr, shared })
}

impl MockWeb {
    /// DNS overrides that send every mock host name to this server.
    pub fn resolve_overrides(&self) -> Vec<(String, SocketAddr)> {
        (0..self.shared.cfg.hosts)
            .map(|i| (host_name(i), self.addr))
            .collect()
    }

    /// Root URLs of the first `n` hosts.
    pub fn seeds(&self, n: u32) -> Vec<Url> {
        let port = self.addr.port();
        (0..n.min(self.shared.cfg.hosts))
            .map(|i| Url::parse(&format!("http://{}:{port}/", host_name(i))).expect("mock URL"))
            .collect()
    }

    /// Every request so far, in logging order (sort by `at` for timing).
    pub fn hits(&self) -> Vec<Hit> {
        self.shared.hits.lock().unwrap().clone()
    }
}

async fn handle(State(web): State<Arc<Shared>>, req: Request) -> Response {
    let at = Instant::now();
    let authority = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(h, _)| h)
        .to_ascii_lowercase();
    let path = req.uri().path();
    let index = host_index(&host).filter(|&i| i < web.cfg.hosts);
    let resp = match index {
        Some(i) => respond(&web, i, path),
        None => (StatusCode::NOT_FOUND, "unknown host\n").into_response(),
    };
    let delay = index.map_or(Duration::ZERO, |i| latency(&web.cfg, i, path));
    let status = resp.status().as_u16();
    let path = path.to_owned();
    web.hits.lock().unwrap().push(Hit {
        host,
        path,
        at,
        status,
    });
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    resp
}

fn respond(web: &Shared, host: u32, path: &str) -> Response {
    let html = |body: Bytes| ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body);
    match path {
        "/robots.txt" => {
            let (status, body) = robots_txt(host);
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, [(header::CONTENT_TYPE, "text/plain")], body).into_response()
        }
        "/big" => html(web.big.clone()).into_response(),
        "/redirect" => {
            (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, "/p/0")]).into_response()
        }
        "/file.pdf" => (
            [(header::CONTENT_TYPE, "application/pdf")],
            "%PDF-1.4\n%mock\n",
        )
            .into_response(),
        _ => match page_number(path).filter(|&n| n < web.cfg.pages_per_host) {
            None => (StatusCode::NOT_FOUND, "not found\n").into_response(),
            Some(_) if rate_limited(web, host) => {
                let retry = [(header::RETRY_AFTER, "2")];
                (StatusCode::TOO_MANY_REQUESTS, retry, "slow down\n").into_response()
            }
            Some(n) => html(page_html(&web.cfg, web.port, host, n).into()).into_response(),
        },
    }
}

/// `RateLimited` hosts refuse every 3rd page request (atomic, so exact under concurrency).
fn rate_limited(web: &Shared, host: u32) -> bool {
    behavior(host) == Behavior::RateLimited
        && web.page_requests[host as usize].fetch_add(1, Ordering::Relaxed) % 3 == 2
}

/// `/` is page 0; `/p/{n}` and `/private/{n}` are page `n`.
fn page_number(path: &str) -> Option<u32> {
    if path == "/" {
        return Some(0);
    }
    path.strip_prefix("/p/")
        .or_else(|| path.strip_prefix("/private/"))?
        .parse()
        .ok()
}

/// How long `host` takes to answer `path`: fixed, so tests can work out the least politeness
/// gap after each request.
pub fn latency(cfg: &MockConfig, host: u32, path: &str) -> Duration {
    let (lo, hi) = cfg.latency_ms;
    // Saturating, so the widest range (0..=u64::MAX) can't overflow; lo + the rest stays ≤ hi.
    let span = hi.saturating_sub(lo).saturating_add(1);
    Duration::from_millis(lo + xxh3_64_with_seed(path.as_bytes(), u64::from(host)) % span)
}
