//! Whole crawls: against the mock web, and against small hand-written sites that each test one
//! rule end to end (redirects, retries, robots.txt, nofollow, slots, shutdown, failures).

use axum::{
    Router,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri},
    response::IntoResponse,
};
use flate2::read::MultiGzDecoder;
use mccrawl::crawler::{BoxError, CrawlConfig, run};
use mccrawl::metrics::Report;
use mccrawl::mockweb::{self, MockConfig};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::pending;
use std::io::{self, Read};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use url::Url;

/// No test crawl should take this long; past it, the crawl is reported as hung.
const LIMIT: Duration = Duration::from_secs(30);

fn out_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mccrawl-crawler-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Test-sized settings: 5 ms between requests to a host, no adaptive delay, quick robots retries.
fn config(seeds: Vec<Url>, out: &Path, resolve: Vec<(String, SocketAddr)>) -> CrawlConfig {
    CrawlConfig {
        concurrency: 64,
        min_delay: Duration::from_millis(5),
        delay_factor: 0.0,
        user_agent: "McCrawl/0.1 (+test)".into(),
        resolve,
        robots_retry: Duration::from_millis(50),
        progress_every: None,
        ..CrawlConfig::new(seeds, out.to_path_buf())
    }
}

/// `run` with no shutdown signal, failing the test instead of hanging if the crawl never ends.
async fn crawl(cfg: CrawlConfig) -> Result<Report, BoxError> {
    tokio::time::timeout(LIMIT, run(cfg, pending()))
        .await
        .expect("the crawl did not finish")
}

fn rows(out: &Path) -> Vec<Value> {
    std::fs::read_to_string(out.join("pages.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Every WARC file in `out`, decompressed and joined in file order.
fn warc_text(out: &Path) -> String {
    let mut files: Vec<_> = std::fs::read_dir(out)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(".warc.gz"))
        .collect();
    files.sort();
    let mut bytes = Vec::new();
    for f in files {
        MultiGzDecoder::new(std::fs::File::open(f).unwrap())
            .read_to_end(&mut bytes)
            .unwrap();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn page_hits(web: &mockweb::MockWeb) -> u64 {
    web.hits()
        .iter()
        .filter(|h| h.path != "/robots.txt")
        .count() as u64
}

fn errors(r: &Report) -> u64 {
    r.errors.values().sum()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// One reply from a test site.
struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
    delay: Duration,
}

impl Reply {
    fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: String::new(),
            delay: Duration::ZERO,
        }
    }

    fn html(body: impl Into<String>) -> Self {
        Self::new(200)
            .header("content-type", "text/html")
            .body(body)
    }

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    fn body(self, body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            ..self
        }
    }

    fn delay(self, delay: Duration) -> Self {
        Self { delay, ..self }
    }
}

type Log = Arc<Mutex<Vec<(String, String, Instant)>>>;

/// A hand-written site on 127.0.0.1 answering for any host name: every request is logged as
/// (host, path, arrival time), and `max_active` is the most requests it was ever serving at once.
struct Site {
    port: u16,
    log: Log,
    max_active: Arc<AtomicUsize>,
}

impl Site {
    fn url(&self, host: &str, path: &str) -> Url {
        Url::parse(&format!("http://{host}:{}{path}", self.port)).unwrap()
    }

    fn resolve(&self, hosts: &[&str]) -> Vec<(String, SocketAddr)> {
        let addr = SocketAddr::from(([127, 0, 0, 1], self.port));
        hosts.iter().map(|h| (h.to_string(), addr)).collect()
    }

    fn requests(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    fn hits(&self, host: &str, path: &str) -> usize {
        let log = self.log.lock().unwrap();
        log.iter()
            .filter(|(h, p, _)| h == host && p == path)
            .count()
    }

    /// (path, arrival) of every request to `host`, in arrival order.
    fn times(&self, host: &str) -> Vec<(String, Instant)> {
        let log = self.log.lock().unwrap();
        let mut t: Vec<_> = log
            .iter()
            .filter(|(h, _, _)| h == host)
            .map(|(_, p, at)| (p.clone(), *at))
            .collect();
        t.sort_by_key(|(_, at)| *at);
        t
    }
}

/// `route(host, path, n)` answers each request: `host` is the Host header (with the port), and
/// `n` counts the earlier requests for the same host and path.
async fn serve(route: impl Fn(&str, &str, usize) -> Reply + Send + Sync + 'static) -> Site {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = Log::default();
    let max_active = Arc::new(AtomicUsize::new(0));
    let state = (
        Arc::new(route),
        log.clone(),
        Arc::new(AtomicUsize::new(0)),
        max_active.clone(),
    );
    let app = Router::new().fallback(move |headers: HeaderMap, uri: Uri| {
        let (route, log, active, max_active) = state.clone();
        async move {
            let host = headers
                .get("host")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let name = host.split(':').next().unwrap_or_default().to_owned();
            let path = uri.path().to_owned();
            let n = {
                let mut log = log.lock().unwrap();
                let n = log
                    .iter()
                    .filter(|(h, p, _)| *h == name && *p == path)
                    .count();
                log.push((name, path.clone(), Instant::now()));
                n
            };
            let reply = route(&host, &path, n);
            max_active.fetch_max(active.fetch_add(1, SeqCst) + 1, SeqCst);
            tokio::time::sleep(reply.delay).await;
            active.fetch_sub(1, SeqCst);
            let mut headers = HeaderMap::new();
            for (name, value) in reply.headers {
                headers.insert(
                    HeaderName::from_static(name),
                    HeaderValue::from_str(&value).unwrap(),
                );
            }
            (
                StatusCode::from_u16(reply.status).unwrap(),
                headers,
                reply.body,
            )
                .into_response()
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await });
    Site {
        port,
        log,
        max_active,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stops_at_the_budget_and_writes_every_output() {
    let mock = MockConfig {
        hosts: 50,
        page_words: 500,
        latency_ms: (1, 3),
        ..Default::default()
    };
    let web = mockweb::start(mock, 0).await.unwrap();
    let out = out_dir("budget");
    let cfg = CrawlConfig {
        budget: 300,
        ..config(web.seeds(50), &out, web.resolve_overrides())
    };
    let r = crawl(cfg).await.unwrap();

    // Exactly the budget of page fetches was started, and each ended as a response or an error.
    assert_eq!(page_hits(&web), 300);
    assert_eq!(r.pages + errors(&r), 300);
    // One row per fetch; every 2xx HTML page archived.
    let rows = rows(&out);
    assert_eq!((rows.len() as u64, r.store.records), (300, 300));
    let archived = rows
        .iter()
        .filter(|r| r["status"] == 200 && r["simhash"].is_string())
        .count() as u64;
    assert_eq!(r.store.warc_records, archived);
    assert!(archived > 200, "{archived}");
    let warc = warc_text(&out);
    assert_eq!(warc.matches("WARC-Type: response").count() as u64, archived);
    assert!(out.join("crawl-00000.warc.gz").exists());
    // report.json: the returned report plus the settings it ran with.
    let json: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("report.json")).unwrap()).unwrap();
    assert_eq!(json["pages"], r.pages);
    assert_eq!(json["store"]["records"], 300);
    assert_eq!(json["frontier"]["hosts"], r.frontier.hosts);
    let config = &json["config"];
    assert_eq!(
        (config["budget"].as_u64(), config["concurrency"].as_u64()),
        (Some(300), Some(64))
    );
    assert_eq!(config["seeds"], 50);
    assert_eq!(config["user_agent"], "McCrawl/0.1 (+test)");
    assert_eq!(config["min_delay_secs"], 0.005);
    std::fs::remove_dir_all(&out).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ends_by_itself_once_every_reachable_page_is_fetched() {
    let mock = MockConfig {
        hosts: 10,
        pages_per_host: 30,
        links_per_page: 10,
        page_words: 300,
        latency_ms: (0, 0),
        cross_host_pct: 0,
    };
    let web = mockweb::start(mock, 0).await.unwrap();
    let out = out_dir("exhaust");
    // h4..h9 behave normally: robots.txt 404, no Crawl-delay, no 429s.
    let seeds = web.seeds(10).split_off(4);
    let r = crawl(config(seeds, &out, web.resolve_overrides()))
        .await
        .unwrap();

    let rows = rows(&out);
    let fetched: HashSet<&str> = rows.iter().map(|r| r["url"].as_str().unwrap()).collect();
    assert_eq!(fetched.len(), rows.len(), "a URL was fetched twice");
    for row in &rows {
        assert_eq!(row["status"], 200, "{}", row["url"]);
        assert!(!row["title"].as_str().unwrap().is_empty());
        for link in row["links"].as_array().unwrap() {
            let url = link["url"].as_str().unwrap();
            assert!(fetched.contains(url), "{url} was linked but never fetched");
        }
    }
    assert_eq!(r.pages, rows.len() as u64);
    assert!(r.pages > 100, "{}", r.pages);
    assert_eq!((r.frontier.queued, r.frontier.hosts), (0, 6));
    assert_eq!(r.store.warc_records, r.pages);
    assert!(
        r.store.near_dups > 0,
        "the mock plants near-duplicate pages"
    );
    // Per host: robots.txt first and once, then each page once.
    let mut by_host: HashMap<String, Vec<mockweb::Hit>> = HashMap::new();
    for h in web.hits() {
        by_host.entry(h.host.clone()).or_default().push(h);
    }
    assert_eq!(by_host.len(), 6);
    for (host, mut hits) in by_host {
        hits.sort_by_key(|h| h.at);
        assert_eq!(hits[0].path, "/robots.txt", "{host}");
        let paths: HashSet<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(
            paths.len(),
            hits.len(),
            "{host}: a path was requested twice"
        );
    }
    std::fs::remove_dir_all(&out).ok();
}

const HOME: &str = r#"<title>Home</title><link rel="canonical" href="/">
<a href="/moved">moved</a> <a href="/loop/0">loop</a> <a href="/gone">gone</a>
<a href="/doc.pdf">pdf</a> <a href="/blocked/x">blocked</a> <a href="/nf" rel="nofollow">nf</a>
<a href="/meta-nf">meta</a> <a href="/big">big</a>"#;

const RETRY_HOME: &str = r#"<a href="/flaky">f</a> <a href="/busy">b</a>
<a href="/unavailable">u</a> <a href="/down">d</a> <a href="/corrupt">c</a>
<a href="http://retry.test:1/refused">r</a>"#;

/// Five hosts on one test site, plus one whose robots.txt can't be reached:
/// - `a.test`: robots.txt Disallow, a cross-host redirect, an endless redirect chain, an HTML 404
///   over the size cap, a PDF, both kinds of nofollow, a 200 over the cap;
/// - `b.test`: the redirect's target (a 203), then a retried redirect onto a URL that needs both
///   of its own retries;
/// - `retry.test`: 500 once, 429 and 503 with Retry-After once, 500 always, a corrupt gzip body
///   (not retried), connection refused;
/// - `dead.test`: every page 500, and `deadnet.test`: every page refused, so each is dropped
///   after 5 failures in a row;
/// - `norobots.test` (port 1): robots.txt unreachable, so nothing is fetched and it's dropped.
fn scenario(host: &str, path: &str, n: usize) -> Reply {
    let (name, port) = host.split_once(':').unwrap();
    match (name, path) {
        (_, "/robots.txt") => Reply::new(200).body("User-agent: *\nDisallow: /blocked\n"),
        ("a.test", "/") => Reply::html(HOME),
        ("a.test", "/gone") => Reply::new(404)
            .header("content-type", "text/html")
            .body(format!(r#"<a href="/from-404">x</a>{}"#, "x".repeat(5000))),
        ("a.test", "/moved") => {
            Reply::new(301).header("location", format!("http://b.test:{port}/landing"))
        }
        ("a.test", "/doc.pdf") => Reply::new(200)
            .header("content-type", "application/pdf")
            .body("%PDF-1.4"),
        ("a.test", "/meta-nf") => {
            Reply::html(r#"<meta name="robots" content="noindex, nofollow"><a href="/never">x</a>"#)
        }
        ("a.test", "/big") => Reply::html("x".repeat(10_000)),
        ("a.test", p) if p.starts_with("/loop/") => {
            let k: u32 = p["/loop/".len()..].parse().unwrap();
            Reply::new(302).header("location", format!("/loop/{}", k + 1))
        }
        ("b.test", "/landing") => Reply {
            status: 203,
            ..Reply::html(r#"<title>Landed</title><a href="/bounce">b</a>"#)
        },
        ("b.test", "/bounce") if n == 0 => Reply::new(500),
        ("b.test", "/bounce") => Reply::new(302).header("location", "/after"),
        ("b.test", "/after") if n < 2 => Reply::new(500),
        ("b.test", "/after") => Reply::html("after"),
        ("retry.test", "/") => Reply::html(RETRY_HOME),
        ("retry.test", "/corrupt") => Reply::html("not gzip").header("content-encoding", "gzip"),
        ("retry.test", "/flaky") if n == 0 => Reply::new(500),
        ("retry.test", "/busy") if n == 0 => Reply::new(429).header("retry-after", "1"),
        ("retry.test", "/unavailable") if n == 0 => Reply::new(503).header("retry-after", "1"),
        ("retry.test", "/flaky" | "/busy" | "/unavailable") => Reply::html("ok"),
        ("retry.test", "/down") => Reply::new(500),
        ("dead.test", "/") => Reply::html(
            (1..=8)
                .map(|i| format!(r#"<a href="/{i}">{i}</a> "#))
                .collect::<String>(),
        ),
        ("dead.test", _) => Reply::new(500),
        ("deadnet.test", "/") => Reply::html(
            (1..=6)
                .map(|i| format!(r#"<a href="http://deadnet.test:1/{i}">{i}</a> "#))
                .collect::<String>(),
        ),
        _ => Reply::new(404),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn follows_redirects_retries_failures_and_obeys_robots_and_nofollow() {
    let site = serve(scenario).await;
    let out = out_dir("scenario");
    let hosts = [
        "a.test",
        "b.test",
        "retry.test",
        "dead.test",
        "deadnet.test",
    ];
    let mut seeds = ["a.test", "retry.test", "dead.test", "deadnet.test"]
        .map(|h| site.url(h, "/"))
        .to_vec();
    seeds.push(Url::parse("http://norobots.test:1/").unwrap());
    let cfg = CrawlConfig {
        concurrency: 16,
        max_body: 4096,
        ..config(
            seeds,
            &out,
            site.resolve(&[&hosts[..], &["norobots.test"]].concat()),
        )
    };
    let start_ms = unix_ms();
    let r = crawl(cfg).await.unwrap();
    let end_ms = unix_ms();
    let hits = |host: &str, path: &str| site.hits(host, path);

    for host in hosts {
        assert_eq!(hits(host, "/robots.txt"), 1, "{host}");
        assert_eq!(
            site.times(host)[0].0,
            "/robots.txt",
            "{host}: robots.txt must come first"
        );
    }
    // Redirects: followed across hosts (through that host's robots.txt), at most 5 hops.
    assert_eq!(
        (hits("a.test", "/moved"), hits("b.test", "/landing")),
        (1, 1)
    );
    for k in 0..=5 {
        assert_eq!(hits("a.test", &format!("/loop/{k}")), 1, "/loop/{k}");
    }
    assert_eq!(hits("a.test", "/loop/6"), 0, "followed a 6th redirect");
    // A redirect target starts with fresh retries: /bounce used one, /after still gets two.
    assert_eq!(
        (hits("b.test", "/bounce"), hits("b.test", "/after")),
        (2, 3)
    );
    // Never fetched: disallowed by robots.txt, rel=nofollow, linked from a meta-nofollow page,
    // linked from an error page.
    for path in ["/blocked/x", "/nf", "/never", "/from-404"] {
        assert_eq!(hits("a.test", path), 0, "{path}");
    }
    // Fetched once: success, 404, non-HTML, an oversized body.
    for path in ["/", "/gone", "/doc.pdf", "/meta-nf", "/big"] {
        assert_eq!(hits("a.test", path), 1, "{path}");
    }
    // Retried until they succeed, or 3 tries in all; a broken body isn't worth retrying.
    for (path, tries) in [
        ("/flaky", 2),
        ("/busy", 2),
        ("/unavailable", 2),
        ("/down", 3),
        ("/corrupt", 1),
    ] {
        assert_eq!(hits("retry.test", path), tries, "{path}");
    }
    // Retry-After holds back the whole host, not just the URL.
    let times = site.times("retry.test");
    for path in ["/busy", "/unavailable"] {
        let i = times.iter().position(|(p, _)| p == path).unwrap();
        let gap = times[i + 1].1 - times[i].1;
        assert!(
            gap >= Duration::from_millis(950),
            "{path}: next request after {gap:?}"
        );
    }
    // Five failures in a row: dead.test is dropped with /6../8 and the retries unfetched.
    let dead: Vec<String> = site
        .times("dead.test")
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    assert_eq!(dead, ["/robots.txt", "/", "/1", "/2", "/3", "/4", "/5"]);

    let status = BTreeMap::from([
        (200, 11),
        (203, 1),
        (301, 1),
        (302, 7),
        (404, 1),
        (429, 1),
        (500, 12),
        (503, 1),
    ]);
    assert_eq!(r.status, status);
    assert_eq!(r.pages, 35);
    // connect: 3 tries of retry.test's refused URL, then deadnet.test's 5 before it's dropped.
    assert_eq!(r.errors, BTreeMap::from([("connect", 8), ("other", 1)]));
    // robots.txt: 5 hosts once, norobots.test 3 times.
    assert_eq!(
        (r.retries, r.redirects, r.non_html, r.robots_fetches),
        (10, 7, 1, 8)
    );
    assert_eq!(
        (
            r.frontier.hosts,
            r.frontier.dead_hosts,
            r.frontier.dropped_disallowed
        ),
        (6, 3, 1)
    );
    assert_eq!((r.store.records, r.store.warc_records), (44, 11));

    // pages.jsonl: what each row says.
    let rows = rows(&out);
    let row = |url: Url| {
        rows.iter()
            .find(|r| r["url"] == url.as_str())
            .unwrap_or_else(|| panic!("no row for {url}"))
    };
    let home_url = site.url("a.test", "/");
    let home = row(home_url.clone());
    assert_eq!(
        (home["title"].as_str(), home["depth"].as_u64()),
        (Some("Home"), Some(0))
    );
    assert_eq!(home["canonical"], home_url.as_str());
    assert_eq!(home["links"].as_array().unwrap().len(), 8);
    assert_eq!(home["links"][5]["url"], site.url("a.test", "/nf").as_str());
    assert_eq!(home["links"][5]["nofollow"], true);
    assert!(home["simhash"].is_string() && home["body_hash"].is_string());
    assert_eq!(home["content_type"], "text/html");
    let fetched_at = home["fetched_at"].as_u64().unwrap();
    assert!((start_ms..=end_ms).contains(&fetched_at), "{fetched_at}");
    // The redirect target keeps the depth, priority and parent of the link that led to it.
    let landing = row(site.url("b.test", "/landing"));
    assert_eq!(
        (landing["status"].as_u64(), landing["title"].as_str()),
        (Some(203), Some("Landed"))
    );
    assert_eq!(
        (landing["depth"].as_u64(), landing["priority"].as_i64()),
        (Some(1), Some(-1))
    );
    assert_eq!(
        (landing["host"].as_str(), landing["parent_url"].as_str()),
        (Some("b.test"), Some(home_url.as_str()))
    );
    assert_eq!(row(site.url("a.test", "/loop/5"))["status"], 302);
    // An HTML error page is recorded but not parsed or archived.
    let gone = row(site.url("a.test", "/gone"));
    assert_eq!(
        (gone["status"].as_u64(), gone["bytes"].as_u64()),
        (Some(404), Some(4096))
    );
    assert_eq!(
        (gone["truncated"].as_bool(), gone["content_type"].as_str()),
        (Some(true), Some("text/html"))
    );
    assert!(gone["simhash"].is_null() && gone["links"].as_array().unwrap().is_empty());
    let big = row(site.url("a.test", "/big"));
    assert_eq!(
        (big["truncated"].as_bool(), big["bytes"].as_u64()),
        (Some(true), Some(4096))
    );
    let pdf = row(site.url("a.test", "/doc.pdf"));
    assert_eq!(
        (pdf["content_type"].as_str(), pdf["bytes"].as_u64()),
        (Some("application/pdf"), Some(0))
    );
    assert!(pdf["simhash"].is_null());
    assert_eq!(row(site.url("a.test", "/meta-nf"))["noindex"], true);
    let refused: Vec<_> = rows
        .iter()
        .filter(|r| r["url"] == "http://retry.test:1/refused")
        .collect();
    assert_eq!(refused.len(), 3);
    for r in refused {
        assert!(r["status"].is_null(), "{r}");
        assert!(r["error"].as_str().unwrap().starts_with("connect"), "{r}");
    }
    let corrupt = row(site.url("retry.test", "/corrupt"));
    assert!(corrupt["status"].is_null() && corrupt["error"].as_str().unwrap().starts_with("other"));
    assert!(!rows.iter().any(|r| r["host"] == "norobots.test"));
    // The WARC holds the 2xx HTML responses only, bodies included; the capped one is marked.
    let warc = warc_text(&out);
    assert_eq!(warc.matches("WARC-Type: response").count(), 11);
    assert_eq!(warc.matches("WARC-Truncated: length").count(), 1);
    let landing_uri = format!("WARC-Target-URI: {}\r\n", site.url("b.test", "/landing"));
    assert!(warc.contains(&landing_uri));
    assert!(warc.contains("<title>Landed</title>"));
    std::fs::remove_dir_all(&out).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn never_runs_more_fetches_at_once_than_the_concurrency() {
    let site = serve(|_, path, _| {
        let reply = if path == "/robots.txt" {
            Reply::new(404)
        } else {
            Reply::html("leaf")
        };
        reply.delay(Duration::from_millis(30))
    })
    .await;
    let names: Vec<String> = (0..20).map(|i| format!("h{i}.test")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let out = out_dir("slots");
    let seeds = names.iter().map(|h| site.url(h, "/")).collect();
    let cfg = CrawlConfig {
        concurrency: 3,
        ..config(seeds, &out, site.resolve(&names))
    };
    let r = crawl(cfg).await.unwrap();
    assert_eq!((r.pages, r.robots_fetches), (20, 20));
    assert_eq!(site.max_active.load(SeqCst), 3);
    // The slots were the limit, not politeness: there was almost always a host allowed.
    assert!(
        r.idle_slot_secs < r.wall_secs / 2.0,
        "{} of {}",
        r.idle_slot_secs,
        r.wall_secs
    );
    std::fs::remove_dir_all(&out).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waits_between_requests_to_a_host_and_counts_the_wait() {
    let site = serve(|_, path, _| match path {
        "/robots.txt" => Reply::new(404),
        "/" => Reply::html(
            r#"<a href="/1">1</a> <a href="/2">2</a> <a href="/3">3</a> <a href="/4">4</a>"#,
        ),
        _ => Reply::html("leaf"),
    })
    .await;
    let out = out_dir("wait");
    let cfg = CrawlConfig {
        min_delay: Duration::from_millis(150),
        progress_every: Some(Duration::from_millis(200)),
        ..config(
            vec![site.url("a.test", "/")],
            &out,
            site.resolve(&["a.test"]),
        )
    };
    let r = crawl(cfg).await.unwrap();
    assert_eq!(r.pages, 5);
    let times = site.times("a.test");
    assert_eq!(times.len(), 6);
    for w in times.windows(2) {
        let gap = w[1].1 - w[0].1;
        assert!(
            gap >= Duration::from_millis(145),
            "{} after {gap:?}",
            w[1].0
        );
    }
    // Five 150 ms waits with a free slot and nothing else allowed: that's the politeness cost.
    assert!(
        r.idle_slot_secs >= 0.7 && r.idle_slot_secs <= r.wall_secs,
        "{} of {}",
        r.idle_slot_secs,
        r.wall_secs
    );
    std::fs::remove_dir_all(&out).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_lets_fetches_in_flight_finish_then_stops() {
    let mock = MockConfig {
        hosts: 50,
        page_words: 500,
        ..Default::default()
    };
    let web = mockweb::start(mock, 0).await.unwrap();
    let out = out_dir("stop");
    let cfg = CrawlConfig {
        budget: 1_000_000,
        ..config(web.seeds(50), &out, web.resolve_overrides())
    };
    let t = Instant::now();
    let stop = tokio::time::sleep(Duration::from_millis(500));
    let r = tokio::time::timeout(LIMIT, run(cfg, stop))
        .await
        .unwrap()
        .unwrap();
    // The mock answers within 200 ms, so the grace period (10 s) is never used up.
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert!(r.pages > 0);
    // Every request the server saw was finished and recorded.
    assert_eq!(page_hits(&web), r.pages + errors(&r));
    assert_eq!(rows(&out).len() as u64, r.store.records);
    assert!(out.join("report.json").exists());
    // And nothing is sent after `run` returns.
    let sent = web.hits().len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(web.hits().len(), sent, "requests sent after run returned");
    std::fs::remove_dir_all(&out).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grace_period_abandons_fetches_that_never_finish() {
    let site = serve(|_, _, _| Reply::new(200).delay(Duration::from_secs(3600))).await;
    let out = out_dir("grace");
    let cfg = CrawlConfig {
        grace: Duration::from_millis(300),
        ..config(
            vec![site.url("a.test", "/")],
            &out,
            site.resolve(&["a.test"]),
        )
    };
    let t = Instant::now();
    let stop = tokio::time::sleep(Duration::from_millis(200));
    let r = tokio::time::timeout(LIMIT, run(cfg, stop))
        .await
        .unwrap()
        .unwrap();
    let took = t.elapsed();
    assert!(
        took >= Duration::from_millis(500) && took < Duration::from_secs(3),
        "{took:?}"
    );
    assert_eq!(site.hits("a.test", "/robots.txt"), 1);
    assert_eq!((r.pages, r.robots_fetches, r.store.records), (0, 0, 0));
    assert!(out.join("report.json").exists());
    std::fs::remove_dir_all(&out).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_store_failure_stops_the_crawl_with_its_error() {
    let mock = MockConfig {
        hosts: 50,
        page_words: 500,
        latency_ms: (0, 0),
        ..Default::default()
    };
    let web = mockweb::start(mock, 0).await.unwrap();
    let out = out_dir("storefail");
    // The first rotation lands on a leftover file, which the store refuses to overwrite.
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("crawl-00001.warc.gz"), "keep").unwrap();
    let cfg = CrawlConfig {
        budget: 1_000_000,
        warc_rotate_at: 1,
        ..config(web.seeds(50), &out, web.resolve_overrides())
    };
    let err = crawl(cfg).await.unwrap_err();
    let io = err
        .downcast_ref::<io::Error>()
        .expect("the store's io::Error");
    assert_eq!(io.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read(out.join("crawl-00001.warc.gz")).unwrap(),
        b"keep"
    );
    assert!(!out.join("report.json").exists());
    std::fs::remove_dir_all(&out).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_bad_settings_before_touching_disk_or_network() {
    let site = serve(|_, _, _| Reply::html("x")).await;
    let out = out_dir("invalid");
    let base = || {
        config(
            vec![site.url("a.test", "/")],
            &out,
            site.resolve(&["a.test"]),
        )
    };
    let day = Duration::from_secs(24 * 60 * 60);
    let past_day = day + Duration::from_millis(1);
    let bad = [
        (
            "concurrency 0",
            CrawlConfig {
                concurrency: 0,
                ..base()
            },
        ),
        (
            "k NaN",
            CrawlConfig {
                delay_factor: f64::NAN,
                ..base()
            },
        ),
        (
            "k negative",
            CrawlConfig {
                delay_factor: -0.1,
                ..base()
            },
        ),
        (
            "k too big",
            CrawlConfig {
                delay_factor: 1000.1,
                ..base()
            },
        ),
        (
            "k infinite",
            CrawlConfig {
                delay_factor: f64::INFINITY,
                ..base()
            },
        ),
        (
            "min_delay",
            CrawlConfig {
                min_delay: past_day,
                ..base()
            },
        ),
        (
            "robots_retry",
            CrawlConfig {
                robots_retry: Duration::MAX,
                ..base()
            },
        ),
        (
            "grace",
            CrawlConfig {
                grace: past_day,
                ..base()
            },
        ),
        (
            "progress 0",
            CrawlConfig {
                progress_every: Some(Duration::ZERO),
                ..base()
            },
        ),
        (
            "progress",
            CrawlConfig {
                progress_every: Some(past_day),
                ..base()
            },
        ),
        (
            "user agent",
            CrawlConfig {
                user_agent: "bad\nagent".into(),
                ..base()
            },
        ),
    ];
    for (what, cfg) in bad {
        assert!(crawl(cfg).await.is_err(), "{what} accepted");
        assert!(!out.exists(), "{what}: created the output directory");
    }
    assert_eq!(site.requests(), 0);
    // The limits themselves are fine. Budget 0 fetches nothing, not even robots.txt.
    let edge = CrawlConfig {
        budget: 0,
        delay_factor: 1000.0,
        min_delay: day,
        robots_retry: day,
        grace: day,
        progress_every: Some(day),
        ..base()
    };
    let r = crawl(edge).await.unwrap();
    assert_eq!((r.pages, r.robots_fetches, r.store.records), (0, 0, 0));
    assert_eq!(site.requests(), 0);
    std::fs::remove_dir_all(&out).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_crawl_finishes_and_its_output_is_never_overwritten() {
    let site = serve(|_, _, _| Reply::html("x")).await;
    let out = out_dir("empty");
    let r = crawl(config(Vec::new(), &out, Vec::new())).await.unwrap();
    assert_eq!((r.pages, r.robots_fetches, r.store.records), (0, 0, 0));
    let report = std::fs::read(out.join("report.json")).unwrap();
    // The same directory again: refused before anything is fetched, earlier output untouched.
    let again = config(
        vec![site.url("a.test", "/")],
        &out,
        site.resolve(&["a.test"]),
    );
    let err = crawl(again).await.unwrap_err();
    let io = err.downcast_ref::<io::Error>().expect("an io::Error");
    assert_eq!(io.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(out.join("report.json")).unwrap(), report);
    assert_eq!(site.requests(), 0);
    std::fs::remove_dir_all(&out).ok();
    // A leftover report.json alone: the crawl runs, but its report won't replace the old one.
    let out = out_dir("leftover");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("report.json"), "keep").unwrap();
    let err = crawl(config(Vec::new(), &out, Vec::new()))
        .await
        .unwrap_err();
    let io = err.downcast_ref::<io::Error>().expect("an io::Error");
    assert_eq!(io.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(out.join("report.json")).unwrap(), b"keep");
    std::fs::remove_dir_all(&out).ok();
}
