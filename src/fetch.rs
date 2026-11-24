//! HTTP fetching. Two reqwest clients share one config: the page client never follows
//! redirects (the crawler queues the target as a new URL, so politeness and robots.txt apply
//! to it), and the robots.txt client follows up to 5, as RFC 9309 asks. Bodies are decoded
//! (gzip, br, zstd), capped at `max_body` bytes after decoding, and errors are sorted into
//! the few kinds the crawler acts on.

use std::{
    error::Error,
    fmt,
    net::SocketAddr,
    time::{Duration, Instant},
};

use reqwest::{
    Client, StatusCode,
    header::{self, HeaderMap, HeaderName, HeaderValue},
    redirect::Policy,
};
use url::Url;

#[derive(Clone, Debug)]
pub struct FetchConfig {
    pub user_agent: String,
    pub connect_timeout: Duration,
    /// Whole request: connect, headers and body.
    pub timeout: Duration,
    pub max_body: usize,
    /// DNS overrides, e.g. every mock host → the mock server. The URL's port always wins.
    pub resolve: Vec<(String, SocketAddr)>,
}

#[derive(Debug)]
pub struct FetchResponse {
    pub status: u16,
    /// Status line and headers for the WARC record, describing the body as stored: always
    /// `HTTP/1.1`, no `Transfer-Encoding`, and `Content-Length` = `body.len()`.
    pub http_head: Vec<u8>,
    pub is_html: bool,
    pub content_type: Option<String>,
    pub location: Option<String>,
    pub retry_after: Option<Duration>,
    /// Decoded body, at most `max_body` bytes. Left empty for a page that isn't HTML.
    pub body: Vec<u8>,
    pub truncated: bool,
    /// From sending the request until the body was read.
    pub elapsed: Duration,
}

/// DNS and TLS failures count as `Connect`: reqwest reports them all as connect errors.
#[derive(Debug)]
pub enum FetchError {
    Timeout,
    Connect(String),
    Other(String),
}

impl FetchError {
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Timeout | Self::Connect(_))
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Connect(_) => "connect",
            Self::Other(_) => "other",
        }
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("timeout"),
            Self::Connect(msg) | Self::Other(msg) => write!(f, "{}: {msg}", self.kind()),
        }
    }
}

impl Error for FetchError {}

impl From<reqwest::Error> for FetchError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            return Self::Timeout;
        }
        // reqwest's own message is generic ("error sending request"); the causes say why.
        let e = e.without_url();
        let mut msg = e.to_string();
        let mut cause = e.source();
        while let Some(c) = cause {
            msg.push_str(": ");
            msg.push_str(&c.to_string());
            cause = c.source();
        }
        if e.is_connect() {
            Self::Connect(msg)
        } else {
            Self::Other(msg)
        }
    }
}

pub struct Fetcher {
    page: Client,
    robots: Client,
    max_body: usize,
}

impl Fetcher {
    pub fn new(cfg: &FetchConfig) -> Result<Self, reqwest::Error> {
        let build = |redirects: Policy, accept: &'static str| {
            let mut b = Client::builder()
                .user_agent(&cfg.user_agent)
                .default_headers(HeaderMap::from_iter([(
                    header::ACCEPT,
                    HeaderValue::from_static(accept),
                )]))
                .connect_timeout(cfg.connect_timeout)
                .timeout(cfg.timeout)
                .redirect(redirects)
                .pool_max_idle_per_host(1)
                .pool_idle_timeout(Duration::from_secs(30));
            for (name, addr) in &cfg.resolve {
                b = b.resolve(name, *addr);
            }
            b.build()
        };
        Ok(Self {
            page: build(
                Policy::none(),
                "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8",
            )?,
            robots: build(Policy::limited(5), "text/plain,*/*;q=0.8")?,
            max_body: cfg.max_body,
        })
    }

    /// One page. The body is read only if the response is HTML.
    pub async fn fetch_page(&self, url: &Url) -> Result<FetchResponse, FetchError> {
        self.fetch(&self.page, url, true).await
    }

    /// robots.txt, read whatever its content type.
    pub async fn fetch_robots(&self, url: &Url) -> Result<FetchResponse, FetchError> {
        self.fetch(&self.robots, url, false).await
    }

    async fn fetch(
        &self,
        client: &Client,
        url: &Url,
        html_only: bool,
    ) -> Result<FetchResponse, FetchError> {
        let start = Instant::now();
        let mut resp = client.get(url.clone()).send().await?;
        // Lossy, not `to_str`: some servers send raw UTF-8 in Location, as browsers accept.
        let text = |name| {
            resp.headers()
                .get(name)
                .map(|v: &HeaderValue| String::from_utf8_lossy(v.as_bytes()).into_owned())
        };
        let content_type = text(header::CONTENT_TYPE);
        let location = text(header::LOCATION);
        let retry_after = text(header::RETRY_AFTER).and_then(|v| parse_retry_after(&v));
        let is_html = content_type.as_deref().is_some_and(is_html_type);

        let mut body = Vec::new();
        let mut truncated = false;
        if is_html || !html_only {
            // A size hint only, capped, so a hostile Content-Length can't allocate much.
            body.reserve(resp.content_length().unwrap_or(0).min(self.max_body as u64) as usize);
            while let Some(chunk) = resp.chunk().await? {
                let room = self.max_body - body.len();
                if chunk.len() > room {
                    body.extend_from_slice(&chunk[..room]);
                    truncated = true;
                    break;
                }
                body.extend_from_slice(&chunk);
            }
        }
        Ok(FetchResponse {
            status: resp.status().as_u16(),
            http_head: http_head(resp.status(), resp.headers(), body.len()),
            is_html,
            content_type,
            location,
            retry_after,
            body,
            truncated,
            elapsed: start.elapsed(),
        })
    }
}

fn is_html_type(content_type: &str) -> bool {
    let essence = content_type.split(';').next().unwrap_or_default().trim();
    essence.eq_ignore_ascii_case("text/html")
        || essence.eq_ignore_ascii_case("application/xhtml+xml")
}

/// reqwest already removes `Content-Encoding` when it decodes the body, so one still present
/// is true of the stored bytes and is kept. hyper always removes chunking, and the body may be
/// truncated, so `Transfer-Encoding` goes and `Content-Length` is rewritten.
fn http_head(status: StatusCode, headers: &HeaderMap, body_len: usize) -> Vec<u8> {
    let reason = status.canonical_reason().unwrap_or("");
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", status.as_u16()).into_bytes();
    let kept =
        |name: &HeaderName| name != header::TRANSFER_ENCODING && name != header::CONTENT_LENGTH;
    for (name, value) in headers.iter().filter(|(name, _)| kept(name)) {
        head.extend_from_slice(name.as_str().as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(format!("content-length: {body_len}\r\n\r\n").as_bytes());
    head
}

/// `Retry-After` as delta-seconds (RFC 9110 §10.2.3). Digits only; a value too big for u64
/// saturates (the frontier caps it at 24 h).
// ponytail: the HTTP-date form gives None, so the frontier's own backoff applies; parse it if
// real servers turn out to rely on it.
pub fn parse_retry_after(v: &str) -> Option<Duration> {
    let v = v.trim();
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(Duration::from_secs(v.parse().unwrap_or(u64::MAX)))
}
