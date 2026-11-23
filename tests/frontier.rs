use mccrawl::frontier::{Frontier, FrontierConfig, HostId, Job, Next, Outcome, UrlEntry};
use mccrawl::robots::RobotsRules;
use std::time::{Duration, Instant};
use url::Url;

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
