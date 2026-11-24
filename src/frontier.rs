//! The crawl frontier: per-host URL queues behind a two-heap politeness scheduler.
//!
//! - `waiting`: min-heap of hosts by `next_allowed`, i.e. *when* a host may be contacted.
//! - `ready`: max-heap of hosts allowed now, by their best URL's priority, i.e. *which* host is worth most.
//!
//! Each host is in at most one of {waiting, ready, in flight}, so at most one request per host is
//! ever outstanding. The struct is pure: time is passed in as `now`, never read inside, which makes
//! every timing rule unit-testable without sleeping.

use crate::dedup::SeenSet;
use crate::robots::{RobotsRules, robots_url};
use crate::urlnorm::host_key;
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

/// robots.txt rules are refetched after this long.
const ROBOTS_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// No server-requested wait (Retry-After) is honored beyond this; also keeps `Instant` math from overflowing.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
/// Backoff multiplier ceiling (2^10).
const MAX_BACKOFF_FACTOR: u32 = 1024;

pub type HostId = u32;

#[derive(Clone, Debug)]
pub struct UrlEntry {
    pub url: Url,
    pub depth: u16,
    /// Higher is fetched first. Sub-project 1 uses `-depth` (breadth-first).
    pub priority: i32,
    pub parent: Option<Arc<str>>,
    pub retries: u8,
    pub redirect_hops: u8,
}

impl UrlEntry {
    pub fn seed(url: Url) -> Self {
        Self {
            url,
            depth: 0,
            priority: 0,
            parent: None,
            retries: 0,
            redirect_hops: 0,
        }
    }

    pub fn child(url: Url, parent: &UrlEntry, parent_url: Arc<str>) -> Self {
        let depth = parent.depth.saturating_add(1);
        Self {
            depth,
            priority: -i32::from(depth),
            parent: Some(parent_url),
            ..Self::seed(url)
        }
    }
}

#[derive(Debug)]
pub enum Job {
    Robots { host: HostId, url: Url },
    Page { host: HostId, entry: UrlEntry },
}

#[derive(Debug)]
pub enum Next {
    Job(Job),
    /// Nothing allowed yet; the earliest host becomes allowed at this time.
    WaitUntil(Instant),
    /// Nothing queued, but requests are in flight (their links may add more).
    Idle,
    /// Nothing queued and nothing in flight.
    Done,
}

#[derive(Debug, Clone, Copy)]
pub enum Outcome {
    Ok {
        elapsed: Duration,
    },
    /// 429 / 503: the server asked us to slow down.
    RateLimited {
        elapsed: Duration,
        retry_after: Option<Duration>,
    },
    ServerError {
        elapsed: Duration,
    },
    NetworkError,
}

#[derive(Debug, Clone)]
pub struct FrontierConfig {
    pub min_delay: Duration,
    /// k in `delay = max(crawl_delay, min_delay, k × response_time) × backoff`.
    pub delay_factor: f64,
    pub per_host_cap: u32,
    pub max_backoff: Duration,
    pub max_host_failures: u32,
    pub robots_retry: Duration,
    pub max_robots_attempts: u32,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FrontierStats {
    pub hosts: usize,
    pub dead_hosts: usize,
    pub queued: usize,
    pub dropped_disallowed: u64,
    pub dropped_cap: u64,
    pub dropped_dead: u64,
}

/// A queued URL, ordered by (priority, then FIFO among equals).
struct Queued {
    priority: i32,
    seq: u64,
    entry: UrlEntry,
}

impl Ord for Queued {
    fn cmp(&self, other: &Self) -> Ordering {
        // Max-heap: higher priority first; for equal priority the lower seq (older) wins.
        self.priority
            .cmp(&other.priority)
            .then(other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Queued {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Queued {}

struct Host {
    name: String,
    // ponytail: robots.txt comes from the first URL's origin; hosts mixing http/https or ports share it.
    robots_url: Url,
    queue: BinaryHeap<Queued>,
    /// `None` until robots.txt has been fetched successfully (or after it went unreachable).
    robots: Option<(RobotsRules, Instant)>,
    robots_attempts: u32,
    next_allowed: Instant,
    backoff: u32,
    in_flight: bool,
    /// In the waiting or ready heap.
    scheduled: bool,
    failures: u32,
    fetched: u32,
    dead: bool,
}

impl Host {
    fn new(name: String, robots_url: Url, now: Instant) -> Self {
        Self {
            name,
            robots_url,
            queue: BinaryHeap::new(),
            robots: None,
            robots_attempts: 0,
            next_allowed: now,
            backoff: 1,
            in_flight: false,
            scheduled: false,
            failures: 0,
            fetched: 0,
            dead: false,
        }
    }

    /// Mark dead and drop the queue; returns how many URLs were dropped.
    fn kill(&mut self) -> u64 {
        self.dead = true;
        let dropped = self.queue.len() as u64;
        self.queue.clear();
        dropped
    }
}

/// The politeness formula's base: `max(crawl_delay, min_delay, k × response_time)`.
fn base_delay(cfg: &FrontierConfig, crawl_delay: Duration, elapsed: Duration) -> Duration {
    crawl_delay
        .max(cfg.min_delay)
        .max(elapsed.mul_f64(cfg.delay_factor))
}

pub struct Frontier {
    cfg: FrontierConfig,
    hosts: Vec<Host>,
    ids: HashMap<String, HostId>,
    waiting: BinaryHeap<Reverse<(Instant, HostId)>>,
    // ponytail: priority is a snapshot taken when the host is promoted; a better URL pushed while the
    // host sits in `ready` doesn't reorder it. Fine for -depth; revisit if SP3 scores swing a lot.
    ready: BinaryHeap<(i32, Reverse<u64>, HostId)>,
    seen: SeenSet,
    seq: u64,
    in_flight: usize,
    /// Only the `dropped_*` counters live here; the rest is computed in `stats()`.
    stats: FrontierStats,
}

// ponytail: one scheduler task owns this, so one core is the ceiling; SP2 shards by hash(host).
impl Frontier {
    pub fn new(cfg: FrontierConfig) -> Self {
        Self {
            cfg,
            hosts: Vec::new(),
            ids: HashMap::new(),
            waiting: BinaryHeap::new(),
            ready: BinaryHeap::new(),
            seen: SeenSet::default(),
            seq: 0,
            in_flight: 0,
            stats: FrontierStats::default(),
        }
    }

    /// Queue a newly discovered URL. `false` if it was seen before or has no host.
    pub fn push(&mut self, entry: UrlEntry, now: Instant) -> bool {
        if host_key(&entry.url).is_none() || !self.seen.insert(&entry.url) {
            return false;
        }
        self.enqueue(entry, now);
        true
    }

    /// Queue a retry of an already-seen URL (bypasses the seen-set).
    pub fn requeue(&mut self, entry: UrlEntry, now: Instant) {
        self.enqueue(entry, now);
    }

    pub fn next(&mut self, now: Instant) -> Next {
        // 1. Promote every host whose wait is over.
        while let Some(&Reverse((t, id))) = self.waiting.peek() {
            if t > now {
                break;
            }
            self.waiting.pop();
            let best = self.hosts[id as usize]
                .queue
                .peek()
                .map_or(i32::MIN, |q| q.priority);
            self.seq += 1;
            self.ready.push((best, Reverse(self.seq), id));
        }
        // 2. Serve the most valuable ready host that yields a job.
        while let Some((_, _, id)) = self.ready.pop() {
            if let Some(job) = self.dispatch(id, now) {
                self.in_flight += 1;
                return Next::Job(job);
            }
        }
        // 3. Nothing allowed right now.
        match self.waiting.peek() {
            Some(&Reverse((t, _))) => Next::WaitUntil(t),
            None if self.in_flight > 0 => Next::Idle,
            None => Next::Done,
        }
    }

    pub fn is_done(&self) -> bool {
        self.in_flight == 0 && self.waiting.is_empty() && self.ready.is_empty()
    }

    /// Politeness after a page fetch:
    /// `delay = max(base, min(base × backoff, max_backoff))`, then at least `Retry-After`,
    /// where `base = max(crawl_delay, min_delay, k × response_time)`. The cap only limits the
    /// backoff's extra wait; the site's own Crawl-delay is always honored in full.
    pub fn complete_page(&mut self, host: HostId, outcome: Outcome, now: Instant) {
        self.in_flight -= 1;
        let h = &mut self.hosts[host as usize];
        h.in_flight = false;
        let (elapsed, retry_after) = match outcome {
            Outcome::Ok { elapsed } => {
                h.failures = 0;
                h.backoff = (h.backoff / 2).max(1);
                (elapsed, None)
            }
            Outcome::RateLimited {
                elapsed,
                retry_after,
            } => {
                h.backoff = (h.backoff * 2).min(MAX_BACKOFF_FACTOR);
                (elapsed, retry_after)
            }
            Outcome::ServerError { elapsed } => {
                h.failures += 1;
                (elapsed, None)
            }
            Outcome::NetworkError => {
                h.failures += 1;
                (Duration::ZERO, None)
            }
        };
        if h.failures >= self.cfg.max_host_failures {
            self.stats.dropped_dead += h.kill();
            return;
        }
        let crawl_delay = h.robots.as_ref().and_then(|(r, _)| r.crawl_delay());
        let base = base_delay(&self.cfg, crawl_delay.unwrap_or_default(), elapsed);
        let delay = base.max((base * h.backoff).min(self.cfg.max_backoff));
        let retry_after = retry_after.unwrap_or_default().min(MAX_RETRY_AFTER);
        h.next_allowed = now + delay.max(retry_after);
        self.schedule(host);
    }

    pub fn complete_robots(
        &mut self,
        host: HostId,
        rules: RobotsRules,
        elapsed: Duration,
        now: Instant,
    ) {
        self.in_flight -= 1;
        let h = &mut self.hosts[host as usize];
        h.in_flight = false;
        if matches!(rules, RobotsRules::Unreachable) {
            // RFC 9309: fetch nothing while robots.txt is unreachable; retry with backoff, then give up.
            h.robots = None;
            h.robots_attempts += 1;
            if h.robots_attempts >= self.cfg.max_robots_attempts {
                self.stats.dropped_dead += h.kill();
                return;
            }
            h.next_allowed = now + self.cfg.robots_retry * 2u32.pow(h.robots_attempts - 1);
        } else {
            h.robots_attempts = 0;
            let crawl_delay = rules.crawl_delay().unwrap_or_default();
            h.robots = Some((rules, now));
            h.next_allowed = now + base_delay(&self.cfg, crawl_delay, elapsed);
        }
        self.schedule(host);
    }

    pub fn host_name(&self, host: HostId) -> &str {
        &self.hosts[host as usize].name
    }

    /// O(hosts); called for progress lines every few seconds, not per URL.
    pub fn stats(&self) -> FrontierStats {
        FrontierStats {
            hosts: self.hosts.len(),
            dead_hosts: self.hosts.iter().filter(|h| h.dead).count(),
            queued: self.hosts.iter().map(|h| h.queue.len()).sum(),
            ..self.stats.clone()
        }
    }

    fn enqueue(&mut self, entry: UrlEntry, now: Instant) {
        let Some(name) = host_key(&entry.url) else {
            return;
        };
        let id = match self.ids.get(name) {
            Some(&id) => id,
            None => {
                let id = self.hosts.len() as HostId;
                let host = Host::new(name.to_owned(), robots_url(&entry.url), now);
                self.hosts.push(host);
                self.ids.insert(name.to_owned(), id);
                id
            }
        };
        let h = &mut self.hosts[id as usize];
        if h.dead {
            self.stats.dropped_dead += 1;
            return;
        }
        self.seq += 1;
        h.queue.push(Queued {
            priority: entry.priority,
            seq: self.seq,
            entry,
        });
        self.schedule(id);
    }

    /// The host's next job: robots.txt if its rules are missing or stale, else its best allowed URL.
    /// `None` (host stays unscheduled) if robots or the page cap dropped everything it had.
    fn dispatch(&mut self, id: HostId, now: Instant) -> Option<Job> {
        let h = &mut self.hosts[id as usize];
        h.scheduled = false;
        if h.dead {
            return None;
        }
        let rules = match &h.robots {
            Some((rules, fetched_at)) if now.duration_since(*fetched_at) < ROBOTS_TTL => rules,
            _ => {
                h.in_flight = true;
                let url = h.robots_url.clone();
                return Some(Job::Robots { host: id, url });
            }
        };
        while let Some(q) = h.queue.pop() {
            if h.fetched >= self.cfg.per_host_cap {
                self.stats.dropped_cap += 1 + h.queue.len() as u64;
                h.queue.clear();
                return None;
            }
            if !rules.allowed(&q.entry.url) {
                self.stats.dropped_disallowed += 1;
                continue;
            }
            h.in_flight = true;
            h.fetched += 1;
            return Some(Job::Page {
                host: id,
                entry: q.entry,
            });
        }
        None
    }

    /// Put the host in the waiting heap, unless it's dead, busy, already scheduled or has nothing queued.
    fn schedule(&mut self, id: HostId) {
        let h = &mut self.hosts[id as usize];
        if !h.dead && !h.in_flight && !h.scheduled && !h.queue.is_empty() {
            h.scheduled = true;
            self.waiting.push(Reverse((h.next_allowed, id)));
        }
    }
}
