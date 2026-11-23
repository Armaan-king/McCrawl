use mccrawl::frontier::{
    Frontier, FrontierConfig, FrontierStats, HostId, Job, Next, Outcome, UrlEntry,
};
use mccrawl::robots::RobotsRules;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

const DAY: u64 = 24 * 60 * 60;

fn s(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn cfg() -> FrontierConfig {
    FrontierConfig {
        min_delay: s(1),
        delay_factor: 0.0,
        per_host_cap: 100,
        max_backoff: s(600),
        max_host_failures: 5,
        robots_retry: s(60),
        max_robots_attempts: 3,
    }
}

fn e(u: &str, depth: u16) -> UrlEntry {
    UrlEntry {
        depth,
        priority: -(depth as i32),
        ..UrlEntry::seed(Url::parse(u).unwrap())
    }
}

fn job(f: &mut Frontier, now: Instant) -> Job {
    match f.next(now) {
        Next::Job(j) => j,
        other => panic!("expected job, got {other:?}"),
    }
}

fn page_url(j: Job) -> String {
    match j {
        Job::Page { entry, .. } => entry.url.to_string(),
        j => panic!("{j:?}"),
    }
}

/// Take the next robots job, complete it with AllowAll at time `t`, return its host id.
fn allow(f: &mut Frontier, t: Instant) -> HostId {
    let Job::Robots { host, .. } = job(f, t) else {
        panic!("expected robots job")
    };
    f.complete_robots(host, RobotsRules::AllowAll, ms(0), t);
    host
}

/// Advance `t` by 1 s, take the host's next page job and complete it with `outcome`.
fn fetch_once(f: &mut Frontier, h: HostId, t: &mut Instant, outcome: Outcome) {
    *t += s(1);
    job(f, *t);
    f.complete_page(h, outcome, *t);
}

#[test]
fn robots_first_then_page_after_delay() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    assert!(f.push(e("https://a.com/1", 0), t0));
    let Job::Robots { host, url } = job(&mut f, t0) else {
        panic!()
    };
    assert_eq!(url.as_str(), "https://a.com/robots.txt");
    assert!(matches!(f.next(t0), Next::Idle));
    f.complete_robots(host, RobotsRules::AllowAll, ms(10), t0);
    assert!(matches!(f.next(t0), Next::WaitUntil(t) if t == t0 + s(1)));
    assert_eq!(page_url(job(&mut f, t0 + s(1))), "https://a.com/1");
}

#[test]
fn one_in_flight_per_host_and_delay_between_requests() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    f.push(e("https://a.com/1", 0), t0);
    f.push(e("https://a.com/2", 0), t0);
    let h = allow(&mut f, t0);
    let t1 = t0 + s(1);
    job(&mut f, t1);
    assert!(matches!(f.next(t1), Next::Idle));
    f.complete_page(h, Outcome::Ok { elapsed: ms(10) }, t1);
    assert!(matches!(f.next(t1 + ms(999)), Next::WaitUntil(t) if t == t1 + s(1)));
    assert_eq!(page_url(job(&mut f, t1 + s(1))), "https://a.com/2");
}

#[test]
fn ready_hosts_are_served_best_priority_first() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    f.push(e("https://a.com/deep", 3), t0);
    f.push(e("https://b.com/root", 0), t0);
    allow(&mut f, t0); // b.com (better priority) gets its robots fetched first
    allow(&mut f, t0);
    assert_eq!(page_url(job(&mut f, t0 + s(1))), "https://b.com/root");
    assert_eq!(page_url(job(&mut f, t0 + s(1))), "https://a.com/deep");
}

#[test]
fn dedups_but_requeue_bypasses() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    assert!(f.push(e("https://a.com/1", 0), t0));
    assert!(!f.push(e("https://a.com/1", 0), t0));
    f.requeue(e("https://a.com/1", 0), t0);
    assert_eq!(f.stats().queued, 2);
}

#[test]
fn adaptive_delay_follows_response_time() {
    let t0 = Instant::now();
    let mut f = Frontier::new(FrontierConfig {
        delay_factor: 5.0,
        ..cfg()
    });
    f.push(e("https://a.com/1", 0), t0);
    f.push(e("https://a.com/2", 0), t0);
    let h = allow(&mut f, t0);
    job(&mut f, t0 + s(1));
    f.complete_page(h, Outcome::Ok { elapsed: ms(400) }, t0 + s(2));
    // 5 × 400 ms = 2 s, which beats the 1 s minimum
    assert!(matches!(f.next(t0 + s(2)), Next::WaitUntil(t) if t == t0 + s(4)));
}

#[test]
fn rate_limit_backoff_doubles_halves_and_honors_retry_after() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    for i in 0..5 {
        f.push(e(&format!("https://a.com/{i}"), 0), t0);
    }
    let h = allow(&mut f, t0);
    let mut t = t0 + s(1);
    job(&mut f, t);
    let limited = |retry_after| Outcome::RateLimited {
        elapsed: ms(0),
        retry_after,
    };
    f.complete_page(h, limited(Some(s(30))), t);
    assert!(matches!(f.next(t), Next::WaitUntil(x) if x == t + s(30))); // backoff 2 → 2 s; Retry-After wins
    t += s(30);
    job(&mut f, t);
    f.complete_page(h, limited(None), t);
    assert!(matches!(f.next(t), Next::WaitUntil(x) if x == t + s(4))); // backoff 4
    t += s(4);
    job(&mut f, t);
    f.complete_page(h, Outcome::Ok { elapsed: ms(0) }, t);
    assert!(matches!(f.next(t), Next::WaitUntil(x) if x == t + s(2))); // backoff halves to 2
}

#[test]
fn site_crawl_delay_is_never_cut_by_backoff_cap() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg()); // max_backoff = 600 s
    f.push(e("https://a.com/1", 0), t0);
    f.push(e("https://a.com/2", 0), t0);
    let Job::Robots { host, .. } = job(&mut f, t0) else {
        panic!()
    };
    let txt = b"User-agent: *\nCrawl-delay: 3600\n";
    let rules = RobotsRules::from_response(200, txt, "mccrawl");
    f.complete_robots(host, rules, ms(0), t0);
    let t = t0 + s(3600);
    job(&mut f, t);
    f.complete_page(host, Outcome::Ok { elapsed: ms(0) }, t);
    assert!(matches!(f.next(t), Next::WaitUntil(x) if x == t + s(3600)));
}

#[test]
fn empty_frontier_is_done() {
    let mut f = Frontier::new(cfg());
    assert!(f.is_done());
    assert!(matches!(f.next(Instant::now()), Next::Done));
    f.push(e("https://a.com/1", 0), Instant::now());
    assert!(!f.is_done());
}

// ---------- robots.txt, page caps and dead hosts ----------

#[test]
fn crawl_delay_from_robots() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    f.push(e("https://a.com/1", 0), t0);
    let Job::Robots { host, .. } = job(&mut f, t0) else {
        panic!()
    };
    let txt = b"User-agent: *\nCrawl-delay: 3\n";
    f.complete_robots(
        host,
        RobotsRules::from_response(200, txt, "mccrawl"),
        ms(0),
        t0,
    );
    assert!(matches!(f.next(t0), Next::WaitUntil(t) if t == t0 + s(3)));
}

#[test]
fn disallowed_urls_are_dropped() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    f.push(e("https://a.com/private/x", 0), t0);
    f.push(e("https://a.com/ok", 1), t0);
    let Job::Robots { host, .. } = job(&mut f, t0) else {
        panic!()
    };
    let txt = b"User-agent: *\nDisallow: /private\n";
    f.complete_robots(
        host,
        RobotsRules::from_response(200, txt, "mccrawl"),
        ms(0),
        t0,
    );
    assert_eq!(page_url(job(&mut f, t0 + s(1))), "https://a.com/ok");
    assert_eq!(f.stats().dropped_disallowed, 1);
}

#[test]
fn per_host_cap_drops_the_rest() {
    let t0 = Instant::now();
    let mut f = Frontier::new(FrontierConfig {
        per_host_cap: 2,
        ..cfg()
    });
    for i in 0..3 {
        f.push(e(&format!("https://a.com/{i}"), 0), t0);
    }
    let h = allow(&mut f, t0);
    let mut t = t0;
    for _ in 0..2 {
        fetch_once(&mut f, h, &mut t, Outcome::Ok { elapsed: ms(0) });
    }
    assert!(matches!(f.next(t + s(1)), Next::Done));
    assert_eq!(f.stats().dropped_cap, 1);
}

#[test]
fn host_dies_after_consecutive_failures() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    for i in 0..10 {
        f.push(e(&format!("https://a.com/{i}"), 0), t0);
    }
    let h = allow(&mut f, t0);
    let mut t = t0;
    for _ in 0..5 {
        fetch_once(&mut f, h, &mut t, Outcome::NetworkError);
    }
    assert!(matches!(f.next(t + s(1)), Next::Done));
    assert_eq!(f.stats().dead_hosts, 1);
    assert_eq!(f.stats().dropped_dead, 5);
}

#[test]
fn robots_unreachable_retries_with_backoff_then_gives_up() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    f.push(e("https://a.com/1", 0), t0);
    let mut t = t0;
    for wait in [60, 120] {
        let Job::Robots { host, .. } = job(&mut f, t) else {
            panic!("expected robots")
        };
        f.complete_robots(host, RobotsRules::Unreachable, ms(0), t);
        assert!(matches!(f.next(t), Next::WaitUntil(x) if x == t + s(wait)));
        t += s(wait);
    }
    let Job::Robots { host, .. } = job(&mut f, t) else {
        panic!()
    };
    f.complete_robots(host, RobotsRules::Unreachable, ms(0), t);
    assert!(matches!(f.next(t), Next::Done)); // no Page job was ever issued
    assert_eq!(f.stats().dropped_dead, 1);
}

// ---------- edge cases ----------

#[test]
fn url_pushed_while_host_is_downloading_waits_its_turn() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    f.push(e("https://a.com/1", 0), t0);
    let h = allow(&mut f, t0);
    let t1 = t0 + s(1);
    job(&mut f, t1); // a.com/1 in flight
    f.push(e("https://a.com/2", 0), t1);
    assert!(matches!(f.next(t1), Next::Idle)); // never a second request to a.com
    f.complete_page(h, Outcome::Ok { elapsed: ms(0) }, t1);
    assert!(matches!(f.next(t1), Next::WaitUntil(t) if t == t1 + s(1)));
    assert_eq!(page_url(job(&mut f, t1 + s(1))), "https://a.com/2");
}

#[test]
fn within_a_host_best_priority_first_then_oldest_first() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    f.push(e("https://a.com/deep", 2), t0);
    f.push(e("https://a.com/a", 0), t0);
    f.push(e("https://a.com/b", 0), t0);
    let h = allow(&mut f, t0);
    let mut t = t0;
    for want in ["https://a.com/a", "https://a.com/b", "https://a.com/deep"] {
        t += s(1);
        assert_eq!(page_url(job(&mut f, t)), want);
        f.complete_page(h, Outcome::Ok { elapsed: ms(0) }, t);
    }
}

#[test]
fn one_slow_host_does_not_hold_up_others() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    for u in [
        "https://a.com/1",
        "https://a.com/2",
        "https://b.com/1",
        "https://b.com/2",
    ] {
        f.push(e(u, 0), t0);
    }
    allow(&mut f, t0);
    allow(&mut f, t0);
    let t = t0 + s(1);
    let jobs = [job(&mut f, t), job(&mut f, t)];
    for j in jobs {
        let Job::Page { host, .. } = j else { panic!() };
        let outcome = if f.host_name(host) == "a.com" {
            Outcome::RateLimited {
                elapsed: ms(0),
                retry_after: Some(s(3600)),
            }
        } else {
            Outcome::Ok { elapsed: ms(0) }
        };
        f.complete_page(host, outcome, t);
    }
    assert_eq!(page_url(job(&mut f, t + s(1))), "https://b.com/2");
    assert!(matches!(f.next(t + s(2)), Next::WaitUntil(x) if x == t + s(3600)));
}

#[test]
fn requeued_retry_is_fetched_again() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    f.push(e("https://a.com/1", 0), t0);
    let h = allow(&mut f, t0);
    let Job::Page { entry, .. } = job(&mut f, t0 + s(1)) else {
        panic!()
    };
    f.complete_page(h, Outcome::NetworkError, t0 + s(1));
    let retry = UrlEntry {
        retries: entry.retries + 1,
        ..entry
    };
    f.requeue(retry, t0 + s(1));
    let Job::Page { entry, .. } = job(&mut f, t0 + s(2)) else {
        panic!()
    };
    assert_eq!((entry.url.as_str(), entry.retries), ("https://a.com/1", 1));
}

#[test]
fn server_errors_count_toward_death_but_a_success_resets_the_count() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    for i in 0..20 {
        f.push(e(&format!("https://a.com/{i}"), 0), t0);
    }
    let h = allow(&mut f, t0);
    let mut t = t0;
    let err = Outcome::ServerError { elapsed: ms(0) };
    for _ in 0..4 {
        fetch_once(&mut f, h, &mut t, err);
    }
    fetch_once(&mut f, h, &mut t, Outcome::Ok { elapsed: ms(0) });
    for _ in 0..4 {
        fetch_once(&mut f, h, &mut t, err);
    }
    assert_eq!(f.stats().dead_hosts, 0); // never 5 in a row
    fetch_once(&mut f, h, &mut t, err);
    assert_eq!(f.stats().dead_hosts, 1);
    assert_eq!(f.stats().dropped_dead, 10);
    assert!(matches!(f.next(t + s(1)), Next::Done));
}

#[test]
fn urls_for_a_dead_host_are_dropped() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    for i in 0..5 {
        f.push(e(&format!("https://a.com/{i}"), 0), t0);
    }
    let h = allow(&mut f, t0);
    let mut t = t0;
    for _ in 0..5 {
        fetch_once(&mut f, h, &mut t, Outcome::NetworkError);
    }
    assert!(f.push(e("https://a.com/new", 0), t)); // a new URL, so accepted...
    f.requeue(e("https://a.com/0", 0), t); // ...but both it and a retry are dropped
    let st = f.stats();
    assert_eq!((st.dead_hosts, st.dropped_dead, st.queued), (1, 2, 0));
    assert!(matches!(f.next(t + s(1)), Next::Done));
}

#[test]
fn robots_unreachable_then_recovers_keeps_queued_urls() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    f.push(e("https://a.com/1", 0), t0);
    let Job::Robots { host, .. } = job(&mut f, t0) else {
        panic!()
    };
    f.complete_robots(host, RobotsRules::Unreachable, ms(0), t0);
    assert!(matches!(f.next(t0 + s(59)), Next::WaitUntil(_))); // nothing before the 60 s retry
    allow(&mut f, t0 + s(60));
    assert_eq!(page_url(job(&mut f, t0 + s(61))), "https://a.com/1");
}

#[test]
fn robots_refetched_after_24h_and_new_rules_apply() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    f.push(e("https://a.com/1", 0), t0);
    f.push(e("https://a.com/2", 0), t0);
    let h = allow(&mut f, t0);
    let mut t = t0;
    fetch_once(&mut f, h, &mut t, Outcome::Ok { elapsed: ms(0) });
    let later = t0 + s(DAY + 1);
    assert!(
        matches!(job(&mut f, later), Job::Robots { .. }),
        "stale robots.txt must be refetched before the next page"
    );
    let deny = RobotsRules::from_response(200, b"User-agent: *\nDisallow: /\n", "mccrawl");
    f.complete_robots(h, deny, ms(0), later);
    assert!(matches!(f.next(later + s(1)), Next::Done));
    assert_eq!(f.stats().dropped_disallowed, 1);
}

#[test]
fn retry_after_is_capped_at_24h_and_never_overflows() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    f.push(e("https://a.com/1", 0), t0);
    f.push(e("https://a.com/2", 0), t0);
    let h = allow(&mut f, t0);
    let t = t0 + s(1);
    job(&mut f, t);
    let hostile = Outcome::RateLimited {
        elapsed: ms(0),
        retry_after: Some(Duration::MAX),
    };
    f.complete_page(h, hostile, t);
    assert!(matches!(f.next(t), Next::WaitUntil(x) if x == t + s(DAY)));
}

#[test]
fn urls_without_a_host_are_rejected() {
    let mut f = Frontier::new(cfg());
    let mail = UrlEntry::seed(Url::parse("mailto:a@b.c").unwrap());
    assert!(!f.push(mail, Instant::now()));
    assert!(f.is_done());
    assert_eq!(f.stats().hosts, 0);
}

#[test]
fn child_entries_go_one_level_deeper() {
    let parent = e("https://a.com/p", 2);
    let url = Url::parse("https://a.com/c").unwrap();
    let c = UrlEntry::child(url.clone(), &parent, Arc::from("https://a.com/p"));
    assert_eq!(
        (c.depth, c.priority, c.retries, c.redirect_hops),
        (3, -3, 0, 0)
    );
    assert_eq!(c.parent.as_deref(), Some("https://a.com/p"));
    let deepest = UrlEntry {
        depth: u16::MAX,
        ..parent
    };
    let c = UrlEntry::child(url, &deepest, Arc::from("x"));
    assert_eq!(c.depth, u16::MAX); // saturates instead of overflowing
}

// ---------- randomized check of the politeness rules ----------
//
// Thousands of random operations (push, next, complete, retry, time jumps) against a small
// model of what the rules allow. Fixed seeds, so every run replays the same operations.

/// xorshift64: tiny deterministic PRNG.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

const SIM_HOSTS: u64 = 12;

/// `h7.com` → 7
fn host_num(url: &Url) -> u64 {
    let host = url.host_str().unwrap();
    host[1..host.find('.').unwrap()].parse().unwrap()
}

/// Host 0, 4, 8: disallow /private. Host 1, 5, 9: Crawl-delay 2. Others: allow all.
fn sim_robots(n: u64) -> &'static [u8] {
    match n % 4 {
        0 => b"User-agent: *\nDisallow: /private\n",
        1 => b"User-agent: *\nCrawl-delay: 2\n",
        _ => b"User-agent: *\nAllow: /\n",
    }
}

fn sim_crawl_delay(n: u64) -> Duration {
    if n % 4 == 1 { s(2) } else { Duration::ZERO }
}

/// What the rules say, tracked independently of the frontier.
#[derive(Default)]
struct Model {
    busy: HashSet<HostId>,
    earliest: HashMap<HostId, Instant>,
    robots_ok: HashSet<HostId>,
    robots_fails: HashMap<HostId, u32>,
    page_fails: HashMap<HostId, u32>,
    dead: HashSet<HostId>,
    pages: HashMap<HostId, u32>,
    dispatched_pages: u64,
    accepted: u64,
    requeued: u64,
}

struct Sim {
    f: Frontier,
    cfg: FrontierConfig,
    m: Model,
    rng: Rng,
    now: Instant,
    in_flight: Vec<Job>,
}

impl Sim {
    fn push_random(&mut self) {
        let n = self.rng.below(SIM_HOSTS);
        let path = if self.rng.below(4) == 0 {
            format!("/private/{}", self.rng.below(10))
        } else {
            format!("/p/{}", self.rng.below(60))
        };
        let depth = self.rng.below(5) as u16;
        if self
            .f
            .push(e(&format!("https://h{n}.com{path}"), depth), self.now)
        {
            self.m.accepted += 1;
        }
    }

    /// Ask for the next job and check whatever comes back against the model.
    fn next(&mut self) -> Next {
        let next = self.f.next(self.now);
        match &next {
            Next::Job(j) => self.check_dispatch(j),
            Next::WaitUntil(t) => assert!(*t > self.now, "WaitUntil in the past"),
            Next::Idle => assert!(!self.in_flight.is_empty(), "Idle with nothing in flight"),
            Next::Done => assert!(self.in_flight.is_empty() && self.f.is_done(), "early Done"),
        }
        next
    }

    fn check_dispatch(&mut self, j: &Job) {
        let host = match j {
            Job::Robots { host, .. } | Job::Page { host, .. } => *host,
        };
        let m = &mut self.m;
        let name = self.f.host_name(host).to_owned();
        assert!(!m.dead.contains(&host), "{name}: job for a dead host");
        assert!(m.busy.insert(host), "{name}: two requests in flight");
        if let Some(&t) = m.earliest.get(&host) {
            assert!(
                self.now >= t,
                "{name}: contacted {:?} too early",
                t - self.now
            );
        }
        if let Job::Page { entry, .. } = j {
            assert!(
                m.robots_ok.contains(&host),
                "{name}: page before robots.txt"
            );
            let private = entry.url.path().starts_with("/private");
            assert!(
                !(private && host_num(&entry.url).is_multiple_of(4)),
                "{name}: fetched a disallowed URL"
            );
            let pages = m.pages.entry(host).or_default();
            *pages += 1;
            assert!(*pages <= self.cfg.per_host_cap, "{name}: over the page cap");
            m.dispatched_pages += 1;
        }
    }

    fn complete(&mut self, j: Job) {
        let (cfg, m, now) = (&self.cfg, &mut self.m, self.now);
        match j {
            Job::Robots { host, url } => {
                m.busy.remove(&host);
                let n = host_num(&url);
                let elapsed = ms(self.rng.below(300));
                if self.rng.below(5) == 0 {
                    m.robots_ok.remove(&host);
                    let fails = m.robots_fails.entry(host).or_default();
                    *fails += 1;
                    if *fails >= cfg.max_robots_attempts {
                        m.dead.insert(host);
                    } else {
                        let wait = cfg.robots_retry * 2u32.pow(*fails - 1);
                        m.earliest.insert(host, now + wait);
                    }
                    self.f
                        .complete_robots(host, RobotsRules::Unreachable, elapsed, now);
                } else {
                    m.robots_fails.remove(&host);
                    m.robots_ok.insert(host);
                    let base = sim_crawl_delay(n)
                        .max(cfg.min_delay)
                        .max(elapsed.mul_f64(cfg.delay_factor));
                    m.earliest.insert(host, now + base);
                    let rules = RobotsRules::from_response(200, sim_robots(n), "mccrawl");
                    self.f.complete_robots(host, rules, elapsed, now);
                }
            }
            Job::Page { host, entry } => {
                m.busy.remove(&host);
                let n = host_num(&entry.url);
                let elapsed = ms(self.rng.below(500));
                let roll = self.rng.below(10);
                let retry_after = (self.rng.below(2) == 0).then(|| s(self.rng.below(20)));
                let outcome = match roll {
                    0..=5 => Outcome::Ok { elapsed },
                    6 => Outcome::RateLimited {
                        elapsed,
                        retry_after,
                    },
                    7 => Outcome::ServerError { elapsed },
                    _ => Outcome::NetworkError,
                };
                let fails = m.page_fails.entry(host).or_default();
                let (elapsed, wait) = match outcome {
                    Outcome::Ok { elapsed } => {
                        *fails = 0;
                        (elapsed, Duration::ZERO)
                    }
                    Outcome::RateLimited { elapsed, .. } => {
                        (elapsed, retry_after.unwrap_or_default())
                    }
                    Outcome::ServerError { elapsed } => {
                        *fails += 1;
                        (elapsed, Duration::ZERO)
                    }
                    Outcome::NetworkError => {
                        *fails += 1;
                        (Duration::ZERO, Duration::ZERO)
                    }
                };
                if *fails >= cfg.max_host_failures {
                    m.dead.insert(host);
                }
                let base = sim_crawl_delay(n)
                    .max(cfg.min_delay)
                    .max(elapsed.mul_f64(cfg.delay_factor));
                m.earliest.insert(host, now + base.max(wait));
                self.f.complete_page(host, outcome, now);
                if matches!(outcome, Outcome::NetworkError) && entry.retries < 2 {
                    let retry = UrlEntry {
                        retries: entry.retries + 1,
                        ..entry
                    };
                    self.f.requeue(retry, now);
                    m.requeued += 1;
                }
            }
        }
    }

    fn complete_random(&mut self) {
        let i = self.rng.below(self.in_flight.len() as u64) as usize;
        let j = self.in_flight.swap_remove(i);
        self.complete(j);
    }
}

fn simulate(seed: u64) -> FrontierStats {
    let cfg = FrontierConfig {
        min_delay: s(1),
        delay_factor: 2.0,
        per_host_cap: 25,
        max_backoff: s(60),
        max_host_failures: 5,
        robots_retry: s(10),
        max_robots_attempts: 3,
    };
    let mut sim = Sim {
        f: Frontier::new(cfg.clone()),
        cfg,
        m: Model::default(),
        rng: Rng(seed),
        now: Instant::now(),
        in_flight: Vec::new(),
    };

    // Random phase: URLs keep arriving while jobs start and finish in random order.
    for _ in 0..5_000 {
        match sim.rng.below(10) {
            0..=1 => sim.push_random(),
            2..=4 if !sim.in_flight.is_empty() => sim.complete_random(),
            5 => sim.now += ms(sim.rng.below(3000)),
            _ => match sim.next() {
                Next::Job(j) => sim.in_flight.push(j),
                Next::WaitUntil(t) if sim.in_flight.is_empty() || sim.rng.below(2) == 0 => {
                    sim.now = t
                }
                _ => {}
            },
        }
    }

    // Drain: no new URLs; finish everything until the frontier says Done.
    let mut done = false;
    for _ in 0..1_000_000 {
        if !sim.in_flight.is_empty() {
            sim.complete_random();
            continue;
        }
        match sim.next() {
            Next::Job(j) => sim.in_flight.push(j),
            Next::WaitUntil(t) => sim.now = t,
            Next::Idle => unreachable!("checked in Sim::next"),
            Next::Done => {
                done = true;
                break;
            }
        }
    }
    assert!(done, "seed {seed}: never reached Done");

    // Every URL is accounted for: fetched, or dropped for a counted reason. Nothing lost or doubled.
    let st = sim.f.stats();
    let m = &sim.m;
    assert_eq!(st.queued, 0, "seed {seed}");
    assert_eq!(st.dead_hosts, m.dead.len(), "seed {seed}");
    assert_eq!(
        m.dispatched_pages + st.dropped_disallowed + st.dropped_cap + st.dropped_dead,
        m.accepted + m.requeued,
        "seed {seed}: URLs lost or duplicated"
    );
    assert!(m.dispatched_pages > 0, "seed {seed}: fetched nothing");
    st
}

#[test]
fn random_operations_never_break_politeness_or_lose_urls() {
    let mut total = FrontierStats::default();
    for seed in 1..=25u64 {
        let st = simulate(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        total.dead_hosts += st.dead_hosts;
        total.dropped_disallowed += st.dropped_disallowed;
        total.dropped_cap += st.dropped_cap;
        total.dropped_dead += st.dropped_dead;
    }
    // The simulation must actually reach every rule it claims to check.
    assert!(total.dead_hosts > 0, "{total:?}");
    assert!(total.dropped_disallowed > 0, "{total:?}");
    assert!(total.dropped_cap > 0, "{total:?}");
    assert!(total.dropped_dead > 0, "{total:?}");
    eprintln!("simulation coverage: {total:?}");
}
