//! The crawl pipeline. One scheduler task owns the frontier and the tally (no locks) and keeps
//! up to `concurrency` fetch tasks running. 2xx HTML goes through a small bounded channel to
//! the parse threads, one per core; when they fall behind, fetch tasks wait with their slot
//! held, so memory stays bounded. Every fetch becomes one store message, and every parsed page
//! sends exactly one links message back to the scheduler.

use crate::{
    dedup::{body_hash, simhash},
    fetch::{FetchConfig, FetchError, FetchResponse, Fetcher},
    frontier::{Frontier, FrontierConfig, HostId, Job, Next, Outcome, UrlEntry},
    metrics::{Progress, Report, Tally},
    parse::parse,
    robots::RobotsRules,
    store::{self, LinkOut, PageRecord, Store, StoreMsg, WarcPayload},
    urlnorm::{host_key, normalize},
};
use std::{
    fs::OpenOptions,
    future::Future,
    io::Write,
    net::SocketAddr,
    num::NonZero,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    pin::pin,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
    time::{MissedTickBehavior, sleep_until},
};
use url::Url;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// How often `CrawlConfig::progress` gets a snapshot.
pub const PROGRESS_EVERY: Duration = Duration::from_millis(250);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRIES: u8 = 2;
const MAX_REDIRECT_HOPS: u8 = 5;
const MAX_HOST_FAILURES: u32 = 5;
const MAX_ROBOTS_ATTEMPTS: u32 = 3;
const MAX_BACKOFF: Duration = Duration::from_secs(600);
/// Store and links channels: room for bursts while bounding memory.
const CHANNEL: usize = 1024;
/// Upper limit for every configured duration, which keeps `Instant` math from overflowing.
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug)]
pub struct CrawlConfig {
    pub seeds: Vec<Url>,
    pub out_dir: PathBuf,
    /// Page fetches to start, retries and redirects included.
    pub budget: u64,
    /// Fetches in flight at once, robots.txt included.
    pub concurrency: usize,
    pub per_host_cap: u32,
    pub min_delay: Duration,
    /// k in the politeness formula; 0 turns the adaptive delay off.
    pub delay_factor: f64,
    pub user_agent: String,
    /// The token matched against robots.txt `User-agent` lines.
    pub robots_agent: String,
    /// DNS overrides, e.g. every mock host → the mock server.
    pub resolve: Vec<(String, SocketAddr)>,
    pub max_body: usize,
    pub robots_retry: Duration,
    /// Gets a snapshot every `PROGRESS_EVERY`. `run` drops it when it returns, which closes
    /// the channel.
    pub progress: Option<watch::Sender<Progress>>,
    /// After the shutdown signal, how long fetches in flight get to finish.
    pub grace: Duration,
    /// Start a new WARC file once the current one reaches this many (compressed) bytes.
    pub warc_rotate_at: u64,
}

impl CrawlConfig {
    pub fn new(seeds: Vec<Url>, out_dir: PathBuf) -> Self {
        Self {
            seeds,
            out_dir,
            budget: 100_000,
            concurrency: 512,
            per_host_cap: 500,
            min_delay: Duration::from_secs(1),
            delay_factor: 5.0,
            user_agent: "McCrawl/0.1".into(),
            robots_agent: "mccrawl".into(),
            resolve: Vec::new(),
            max_body: 2 << 20,
            robots_retry: Duration::from_secs(60),
            progress: None,
            grace: Duration::from_secs(10),
            warc_rotate_at: 1 << 30,
        }
    }
}

/// Shared by every fetch task.
struct Ctx {
    fetcher: Fetcher,
    robots_agent: String,
    parse_tx: mpsc::Sender<ParseJob>,
    store_tx: mpsc::Sender<StoreMsg>,
}

struct ParseJob {
    entry: UrlEntry,
    resp: FetchResponse,
    fetched_at: u64,
}

/// What the scheduler needs from a page response; the body has gone to the store or the parser.
struct Meta {
    status: u16,
    elapsed: Duration,
    retry_after: Option<Duration>,
    location: Option<String>,
    bytes: u64,
    is_html: bool,
}

enum Done {
    Robots {
        host: HostId,
        rules: RobotsRules,
        elapsed: Duration,
    },
    Page {
        host: HostId,
        entry: UrlEntry,
        result: Result<Meta, FetchError>,
        /// Sent to the parse pool, which answers with one links message.
        parsing: bool,
    },
}

/// Crawl until the budget is spent, nothing is left to fetch, or `shutdown` completes (then
/// fetches in flight get `grace` to finish). Writes `pages.jsonl`, WARC files and `report.json`
/// into `out_dir`, which must not hold an earlier crawl. A store failure ends the crawl with
/// the store's error.
pub async fn run(cfg: CrawlConfig, shutdown: impl Future<Output = ()>) -> Result<Report, BoxError> {
    validate(&cfg)?;
    let started = Instant::now();
    let fetcher = Fetcher::new(&FetchConfig {
        user_agent: cfg.user_agent.clone(),
        connect_timeout: CONNECT_TIMEOUT,
        timeout: TIMEOUT,
        max_body: cfg.max_body,
        resolve: cfg.resolve.clone(),
    })?;
    let (store_tx, store_rx) = mpsc::channel(CHANNEL);
    let store_thread = store::spawn(Store::open(&cfg.out_dir, cfg.warc_rotate_at)?, store_rx);

    // Parse pool: one thread per core sharing one job queue. The queue is only as deep as the
    // pool; past that, fetch tasks wait (backpressure).
    let workers = thread::available_parallelism().map_or(1, NonZero::get);
    let (parse_tx, parse_rx) = mpsc::channel(workers);
    let (links_tx, mut links_rx) = mpsc::channel(CHANNEL);
    let parse_rx = Arc::new(Mutex::new(parse_rx));
    let pool = (0..workers)
        .map(|i| {
            let (jobs, store_tx, links_tx) = (parse_rx.clone(), store_tx.clone(), links_tx.clone());
            thread::Builder::new()
                .name(format!("parse-{i}"))
                .spawn(move || parse_worker(&jobs, &store_tx, &links_tx))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let ctx = Arc::new(Ctx {
        fetcher,
        robots_agent: cfg.robots_agent.clone(),
        parse_tx,
        store_tx,
    });

    let mut frontier = Frontier::new(FrontierConfig {
        min_delay: cfg.min_delay,
        delay_factor: cfg.delay_factor,
        per_host_cap: cfg.per_host_cap,
        max_backoff: MAX_BACKOFF,
        max_host_failures: MAX_HOST_FAILURES,
        robots_retry: cfg.robots_retry,
        max_robots_attempts: MAX_ROBOTS_ATTEMPTS,
    });
    let now = Instant::now();
    for seed in &cfg.seeds {
        frontier.push(UrlEntry::seed(seed.clone()), now);
    }

    let mut tally = Tally::default();
    let mut inflight = JoinSet::new();
    let mut dispatched = 0u64;
    // Pages sent to the parse pool whose links haven't come back. Links can arrive before
    // their fetch task is joined, so this dips below 0 for a moment.
    let mut pending_parse = 0isize;
    // The grace deadline, once the shutdown signal has fired.
    let mut stop_at: Option<Instant> = None;
    // Since when a slot has been free but no host allowed (the cost of politeness).
    let mut blocked_since: Option<Instant> = None;
    let mut fatal: Option<BoxError> = None;
    let mut progress = tokio::time::interval(PROGRESS_EVERY);
    progress.set_missed_tick_behavior(MissedTickBehavior::Delay);
    progress.reset(); // the first tick would fire at once
    let mut shutdown = pin!(shutdown);

    loop {
        // Fill the free slots with whatever the frontier allows now.
        let mut wake = None;
        while stop_at.is_none() && inflight.len() < cfg.concurrency && dispatched < cfg.budget {
            match frontier.next(Instant::now()) {
                Next::Job(job) => {
                    if let Job::Page { entry, .. } = &job {
                        dispatched += 1;
                        tally.retries += u64::from(entry.retries > 0);
                    }
                    inflight.spawn(fetch(ctx.clone(), job));
                }
                Next::WaitUntil(t) => {
                    wake = Some(t);
                    break;
                }
                Next::Idle | Next::Done => break,
            }
        }
        let now = Instant::now();
        if let Some(t) = blocked_since.take() {
            tally.idle_slot += now - t;
        }
        if wake.is_some() {
            blocked_since = Some(now);
        }
        if inflight.is_empty()
            && pending_parse == 0
            && (stop_at.is_some() || dispatched >= cfg.budget || frontier.is_done())
        {
            break;
        }

        let timer = stop_at.or(wake);
        tokio::select! {
            Some(done) = inflight.join_next() => match done {
                Ok(done) => on_done(done, &mut frontier, &mut tally, &mut pending_parse),
                // A fetch task panicked: a bug, not a per-URL error.
                Err(e) => {
                    fatal = Some(e.into());
                    break;
                }
            },
            Some(children) = links_rx.recv() => {
                pending_parse -= 1;
                let now = Instant::now();
                for child in children {
                    frontier.push(child, now);
                }
            }
            // The store thread stopped early, so it failed; its error is returned below.
            () = ctx.store_tx.closed() => break,
            () = sleep_until(timer.unwrap_or(now).into()), if timer.is_some() => {
                if stop_at.is_some() {
                    break; // grace is over: abandon what is still in flight
                }
            }
            () = &mut shutdown, if stop_at.is_none() => stop_at = Some(Instant::now() + cfg.grace),
            _ = progress.tick(), if cfg.progress.is_some() => {
                if let Some(tx) = &cfg.progress {
                    let stopping = stop_at.is_some();
                    let snap = tally.progress(started.elapsed(), inflight.len(), frontier.stats(), stopping);
                    tx.send_replace(snap);
                }
            }
        }
    }
    if let Some(t) = blocked_since {
        tally.idle_slot += t.elapsed();
    }
    // Fetching is over; say so while the parse pool and the store finish writing.
    if let Some(tx) = &cfg.progress {
        let stopping = stop_at.is_some();
        let snap = tally.progress(
            started.elapsed(),
            inflight.len(),
            frontier.stats(),
            stopping,
        );
        tx.send_replace(Progress {
            saving: true,
            ..snap
        });
    }

    // Teardown: cancel what is still fetching (only after the grace period or an error), stop
    // taking links, let the parse pool drain its queue into the store, then close the store.
    inflight.shutdown().await;
    drop(links_rx);
    drop(ctx);
    let stored = tokio::task::spawn_blocking(move || {
        for worker in pool {
            let _ = worker.join(); // a parse panic is caught per page
        }
        store_thread.join()
    })
    .await?;
    let summary = stored.map_err(|_| "the store thread panicked")??;
    if let Some(e) = fatal {
        return Err(e);
    }
    let report = tally.report(started.elapsed(), frontier.stats(), summary);
    write_report(&cfg, workers, &report)?;
    Ok(report)
}

/// Settings that would hang the crawl or panic deep inside it are refused up front.
fn validate(cfg: &CrawlConfig) -> Result<(), BoxError> {
    if cfg.concurrency == 0 {
        return Err("concurrency must be at least 1".into());
    }
    if !(0.0..=1000.0).contains(&cfg.delay_factor) {
        return Err("delay_factor must be between 0 and 1000".into());
    }
    let too_long = |d: Duration| d > DAY;
    if too_long(cfg.min_delay) || too_long(cfg.robots_retry) || too_long(cfg.grace) {
        return Err("min_delay, robots_retry and grace must be at most 24 h".into());
    }
    Ok(())
}

/// Seed URLs from a seeds file: one URL per line, normalized. Blank lines and `#` comments are
/// skipped, and a comment may follow the URL. Every other line gives a warning instead.
pub fn parse_seeds(text: &str) -> (Vec<Url>, Vec<String>) {
    let (mut seeds, mut warnings) = (Vec::new(), Vec::new());
    for (i, line) in text.trim_start_matches('\u{feff}').lines().enumerate() {
        let mut words = line.split_whitespace();
        let Some(first) = words.next().filter(|w| !w.starts_with('#')) else {
            continue;
        };
        let url = Url::parse(first).ok().and_then(|u| normalize(&u, first));
        match url {
            Some(url) if words.next().is_none_or(|w| w.starts_with('#')) => seeds.push(url),
            _ => {
                // A pasted binary or minified file mustn't flood the terminal.
                let line = line.trim();
                let shown: String = line.chars().take(100).collect();
                let more = if shown.len() < line.len() { "…" } else { "" };
                warnings.push(format!(
                    "line {}: skipped, not one http(s) URL: {shown}{more}",
                    i + 1
                ));
            }
        }
    }
    (seeds, warnings)
}

/// The User-Agent `McCrawl/0.1 (+contact)`, which tells site owners who runs the crawl. The
/// contact must be an http(s) URL or an email address, in plain ASCII.
pub fn user_agent(contact: &str) -> Result<String, BoxError> {
    // Parentheses or a backslash would end or escape the User-Agent comment early.
    let printable = contact
        .bytes()
        .all(|b| b.is_ascii_graphic() && !b"()\\".contains(&b));
    // An http(s) URL always has a host: the parser refuses one without.
    let url = Url::parse(contact).is_ok_and(|u| matches!(u.scheme(), "http" | "https"));
    let email = contact.split_once('@').is_some_and(|(user, domain)| {
        !user.is_empty()
            && !domain.contains('@')
            && domain.contains('.')
            && !domain.starts_with('.')
            && !domain.ends_with('.')
    });
    if contact.len() > 256 || !printable || !(url || email) {
        return Err(
            "the contact must be an http(s) URL or an email address in plain ASCII, \
             e.g. https://example.com/crawler or crawler@example.com"
                .into(),
        );
    }
    Ok(format!("McCrawl/0.1 (+{contact})"))
}

async fn fetch(ctx: Arc<Ctx>, job: Job) -> Done {
    let (host, entry) = match job {
        Job::Robots { host, url } => {
            let (rules, elapsed) = match ctx.fetcher.fetch_robots(&url).await {
                Ok(r) => (
                    RobotsRules::from_response(r.status, &r.body, &ctx.robots_agent),
                    r.elapsed,
                ),
                // Unreachable (RFC 9309): fetch nothing from the host until a retry succeeds.
                Err(_) => (RobotsRules::Unreachable, Duration::ZERO),
            };
            return Done::Robots {
                host,
                rules,
                elapsed,
            };
        }
        Job::Page { host, entry } => (host, entry),
    };
    let result = ctx.fetcher.fetch_page(&entry.url).await;
    let fetched_at = unix_ms();
    let mut parsing = false;
    // A failed send means the store or the pool is gone; the scheduler sees that and stops.
    let result = match result {
        Ok(mut resp) => {
            let meta = Meta {
                status: resp.status,
                elapsed: resp.elapsed,
                retry_after: resp.retry_after,
                location: resp.location.take(),
                bytes: resp.body.len() as u64,
                is_html: resp.is_html,
            };
            if resp.is_html && (200..300).contains(&resp.status) {
                let job = ParseJob {
                    entry: entry.clone(),
                    resp,
                    fetched_at,
                };
                // Backpressure: while the parse pool is behind, wait here, slot held.
                parsing = ctx.parse_tx.send(job).await.is_ok();
            } else {
                let record = PageRecord {
                    status: Some(resp.status),
                    content_type: resp.content_type,
                    bytes: meta.bytes,
                    truncated: resp.truncated,
                    ..record(&entry, fetched_at)
                };
                let _ = ctx.store_tx.send(row(record)).await;
            }
            Ok(meta)
        }
        Err(e) => {
            let record = PageRecord {
                error: Some(e.to_string()),
                ..record(&entry, fetched_at)
            };
            let _ = ctx.store_tx.send(row(record)).await;
            Err(e)
        }
    };
    Done::Page {
        host,
        entry,
        result,
        parsing,
    }
}

/// Politeness, retries and redirects for one finished fetch.
fn on_done(done: Done, frontier: &mut Frontier, tally: &mut Tally, pending_parse: &mut isize) {
    let now = Instant::now();
    let (host, entry, result, parsing) = match done {
        Done::Robots {
            host,
            rules,
            elapsed,
        } => {
            tally.robots_fetches += 1;
            frontier.complete_robots(host, rules, elapsed, now);
            return;
        }
        Done::Page {
            host,
            entry,
            result,
            parsing,
        } => (host, entry, result, parsing),
    };
    *pending_parse += isize::from(parsing);
    let (retry, redirect) = match result {
        Err(e) => {
            *tally.errors.entry(e.kind()).or_default() += 1;
            frontier.complete_page(host, Outcome::NetworkError, now);
            (e.retryable(), None)
        }
        Ok(m) => {
            tally.page(m.status, m.bytes, m.elapsed);
            let success = (200..300).contains(&m.status);
            tally.non_html += u64::from(success && !m.is_html);
            let server_error = (500..600).contains(&m.status);
            let elapsed = m.elapsed;
            let outcome = match m.status {
                429 | 503 => Outcome::RateLimited {
                    elapsed,
                    retry_after: m.retry_after,
                },
                _ if server_error => Outcome::ServerError { elapsed },
                _ => Outcome::Ok { elapsed },
            };
            frontier.complete_page(host, outcome, now);
            let redirect = (300..400).contains(&m.status).then_some(m.location);
            let target = redirect.flatten().and_then(|l| normalize(&entry.url, &l));
            (m.status == 429 || server_error, target)
        }
    };
    if retry && entry.retries < MAX_RETRIES {
        let retries = entry.retries + 1;
        frontier.requeue(UrlEntry { retries, ..entry }, now);
    } else if let Some(url) = redirect
        && entry.redirect_hops < MAX_REDIRECT_HOPS
    {
        // Same depth, priority and parent: the redirect stands in for the link that led to it.
        let hops = entry.redirect_hops + 1;
        let next = UrlEntry {
            url,
            retries: 0,
            redirect_hops: hops,
            ..entry
        };
        tally.redirects += u64::from(frontier.push(next, now));
    }
}

fn parse_worker(
    jobs: &Mutex<mpsc::Receiver<ParseJob>>,
    store_tx: &mpsc::Sender<StoreMsg>,
    links_tx: &mpsc::Sender<Vec<UrlEntry>>,
) {
    loop {
        // The lock is held only while waiting for the next job.
        let Some(job) = jobs.lock().unwrap().blocking_recv() else {
            return;
        };
        // Exactly one links message per job, even if parsing panics, or the scheduler would
        // wait for it forever.
        // ponytail: a panicking page loses its row (the panic hook still logs it); record an
        // error row instead if a parser panic is ever seen.
        let children = catch_unwind(AssertUnwindSafe(|| parse_page(job, store_tx)));
        let _ = links_tx.blocking_send(children.unwrap_or_default());
    }
}

/// Parses one page, sends its row and WARC payload to the store, and returns the links to follow.
fn parse_page(job: ParseJob, store_tx: &mpsc::Sender<StoreMsg>) -> Vec<UrlEntry> {
    let ParseJob {
        entry,
        resp,
        fetched_at,
    } = job;
    let page = parse(&entry.url, &resp.body);
    let children = if page.nofollow {
        Vec::new()
    } else {
        let parent: Arc<str> = Arc::from(entry.url.as_str());
        page.links
            .iter()
            .filter(|l| !l.nofollow)
            .map(|l| UrlEntry::child(l.url.clone(), &entry, parent.clone()))
            .collect()
    };
    let msg = StoreMsg {
        simhash: Some(simhash(&page.text)),
        body_hash: Some(body_hash(&resp.body)),
        record: PageRecord {
            status: Some(resp.status),
            content_type: resp.content_type,
            bytes: resp.body.len() as u64,
            truncated: resp.truncated,
            title: page.title,
            text: page.text,
            canonical: page.canonical.map(String::from),
            noindex: page.noindex,
            links: page
                .links
                .into_iter()
                .map(|l| LinkOut {
                    url: l.url.into(),
                    anchor: l.anchor,
                    nofollow: l.nofollow,
                })
                .collect(),
            ..record(&entry, fetched_at)
        },
        warc: Some(WarcPayload {
            http_head: resp.http_head,
            body: resp.body,
            truncated: resp.truncated,
        }),
    };
    let _ = store_tx.blocking_send(msg);
    children
}

/// The fields every row has, whatever happened to the fetch.
fn record(entry: &UrlEntry, fetched_at: u64) -> PageRecord {
    PageRecord {
        url: entry.url.to_string(),
        host: host_key(&entry.url).unwrap_or_default().to_owned(),
        depth: entry.depth,
        parent_url: entry.parent.as_deref().map(str::to_owned),
        priority: entry.priority,
        fetched_at,
        ..PageRecord::default()
    }
}

/// A row with nothing to archive or fingerprint: an error, a non-2xx or a non-HTML response.
fn row(record: PageRecord) -> StoreMsg {
    StoreMsg {
        record,
        simhash: None,
        body_hash: None,
        warc: None,
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `report.json`: the report plus the settings it ran with. Never overwrites a file.
fn write_report(cfg: &CrawlConfig, parse_workers: usize, report: &Report) -> Result<(), BoxError> {
    let mut json = serde_json::to_value(report)?;
    json["config"] = serde_json::json!({
        "seeds": cfg.seeds.len(),
        "budget": cfg.budget,
        "concurrency": cfg.concurrency,
        "per_host_cap": cfg.per_host_cap,
        "min_delay_secs": cfg.min_delay.as_secs_f64(),
        "delay_factor": cfg.delay_factor,
        "user_agent": cfg.user_agent,
        "robots_agent": cfg.robots_agent,
        "max_body": cfg.max_body,
        "robots_retry_secs": cfg.robots_retry.as_secs_f64(),
        "grace_secs": cfg.grace.as_secs_f64(),
        "warc_rotate_at": cfg.warc_rotate_at,
        "parse_workers": parse_workers,
    });
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(cfg.out_dir.join("report.json"))?;
    file.write_all(&serde_json::to_vec_pretty(&json)?)?;
    Ok(())
}
