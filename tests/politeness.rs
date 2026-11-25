//! The politeness proof: a crawl of 1,000 mock hosts, checked from the server's side, with no
//! slack. The crawler and the mock run in one process on one monotonic clock, so every request
//! to a host must arrive at least this long after the one before it: that request's response
//! time, plus the whole delay it earned. That delay is:
//! - after a page or robots.txt: the larger of the minimum delay, the site's Crawl-delay and
//!   k × the response time; after a 429, at least its Retry-After;
//! - after robots.txt failed for the n-th time: the robots retry × 2^(n-1).
//!
//! robots.txt must come first, be fetched once and be obeyed; a host whose robots.txt never
//! answers gets nothing else, and is given up after 3 tries.

use mccrawl::crawler::{CrawlConfig, run};
use mccrawl::mockweb::{self, Behavior, Hit, MockConfig};
use std::collections::BTreeMap;
use std::future::pending;
use std::time::Duration;

const HOSTS: u32 = 1000;
const BUDGET: u64 = 12_000;
const MIN_DELAY: Duration = Duration::from_millis(200);
const K: f64 = 5.0;
const ROBOTS_RETRY: Duration = Duration::from_millis(300);
/// What the mock's robots.txt sets for `CrawlDelay2` hosts.
const CRAWL_DELAY: Duration = Duration::from_secs(2);
/// What the mock's 429s ask for.
const RETRY_AFTER: Duration = Duration::from_secs(2);

/// How often each rule decided a gap, so the proof can't pass by never testing a rule.
#[derive(Default, Debug)]
struct Seen {
    gaps: usize,
    adaptive: usize,
    crawl_delay: usize,
    retry_after: usize,
    robots_retries: usize,
    robots_given_up: usize,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_crawl_of_1000_hosts_never_breaks_politeness() {
    // Responses take 1–60 ms, so k × the response time (5–300 ms) is sometimes above the
    // 200 ms minimum. Small pages keep parsing and archiving out of the way.
    let mock = MockConfig {
        hosts: HOSTS,
        pages_per_host: 50,
        page_words: 300,
        latency_ms: (1, 60),
        ..Default::default()
    };
    let web = mockweb::start(mock.clone(), 0).await.unwrap();
    let out = std::env::temp_dir().join(format!("mccrawl-politeness-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let cfg = CrawlConfig {
        budget: BUDGET,
        concurrency: 256,
        min_delay: MIN_DELAY,
        delay_factor: K,
        robots_retry: ROBOTS_RETRY,
        user_agent: "McCrawl/0.1 (+test)".into(),
        resolve: web.resolve_overrides(),
        ..CrawlConfig::new(web.seeds(HOSTS), out.clone())
    };
    let r = tokio::time::timeout(Duration::from_secs(120), run(cfg, pending()))
        .await
        .expect("the crawl did not finish")
        .unwrap();
    let errors: u64 = r.errors.values().sum();
    assert_eq!(
        (r.pages, errors),
        (BUDGET, 0),
        "the budget is spent, with no errors"
    );
    assert!(
        r.frontier.dropped_disallowed > 0,
        "no disallowed link was ever met"
    );
    let hits = web.hits();
    // The server saw exactly what the crawler reports: no request goes uncounted.
    assert_eq!(hits.len() as u64, r.pages + r.robots_fetches);

    let mut by_host: BTreeMap<u32, Vec<Hit>> = BTreeMap::new();
    for hit in hits {
        let host = mockweb::host_index(&hit.host)
            .filter(|&i| i < HOSTS)
            .unwrap_or_else(|| panic!("a request to {}, not a mock host", hit.host));
        by_host.entry(host).or_default().push(hit);
    }
    assert_eq!(by_host.len() as u32, HOSTS, "every seed host was contacted");

    let mut seen = Seen::default();
    for (&host, hits) in &mut by_host {
        hits.sort_by_key(|h| h.at);
        let behavior = mockweb::behavior(host);
        let name = mockweb::host_name(host);
        let robots = hits.iter().filter(|h| h.path == "/robots.txt").count();
        assert_eq!(
            hits[0].path, "/robots.txt",
            "{name}: {} came before robots.txt",
            hits[0].path
        );
        match behavior {
            Behavior::RobotsDown => {
                let pages: Vec<&str> = hits
                    .iter()
                    .map(|h| h.path.as_str())
                    .filter(|p| *p != "/robots.txt")
                    .collect();
                assert!(
                    pages.is_empty(),
                    "{name}: fetched {pages:?} while robots.txt was down"
                );
                assert!(robots <= 3, "{name}: robots.txt tried {robots} times");
                seen.robots_given_up += usize::from(robots == 3);
            }
            _ => assert_eq!(robots, 1, "{name}: robots.txt fetched {robots} times"),
        }
        for hit in hits.iter() {
            let disallowed =
                behavior == Behavior::DisallowPrivate && hit.path.starts_with("/private");
            assert!(
                !disallowed,
                "{name}: fetched {}, which robots.txt disallows",
                hit.path
            );
            let expected = if hit.path == "/robots.txt" {
                matches!(hit.status, 200 | 404 | 503)
            } else {
                matches!(hit.status, 200 | 429)
            };
            assert!(expected, "{name}{}: got {}", hit.path, hit.status);
        }

        let mut robots_failures = 0;
        for pair in hits.windows(2) {
            let (prev, next) = (&pair[0], &pair[1]);
            let took = mockweb::latency(&mock, host, &prev.path);
            let delay = if prev.path == "/robots.txt" && prev.status == 503 {
                robots_failures += 1;
                seen.robots_retries += 1;
                ROBOTS_RETRY * 2u32.pow(robots_failures - 1)
            } else {
                let mut delay = MIN_DELAY.max(took.mul_f64(K));
                seen.adaptive += usize::from(took.mul_f64(K) > MIN_DELAY);
                if behavior == Behavior::CrawlDelay2 {
                    delay = delay.max(CRAWL_DELAY);
                    seen.crawl_delay += 1;
                }
                if prev.status == 429 {
                    delay = delay.max(RETRY_AFTER);
                    seen.retry_after += 1;
                }
                delay
            };
            let gap = next.at - prev.at;
            assert!(
                gap >= took + delay,
                "{name}: {} came {gap:?} after {} (answered {} in {took:?}); the least allowed is {:?}",
                next.path,
                prev.path,
                prev.status,
                took + delay
            );
            seen.gaps += 1;
        }
    }
    println!("{seen:?}"); // shown with --nocapture
    // Each rule decided some of the gaps, so none of them passed untested.
    assert!(seen.gaps as u64 >= BUDGET, "{seen:?}");
    assert!(
        seen.adaptive > 100 && seen.crawl_delay >= 100 && seen.retry_after > 10,
        "{seen:?}"
    );
    assert!(
        seen.robots_retries >= 200 && seen.robots_given_up > 10,
        "{seen:?}"
    );
    std::fs::remove_dir_all(&out).ok();
}
