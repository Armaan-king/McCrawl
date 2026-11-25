use mccrawl::dedup::{body_hash, hamming, simhash};
use mccrawl::mockweb::{
    self, Behavior, MockConfig, MockWeb, behavior, host_index, host_name, page_html,
    politeness_ceiling, robots_txt,
};
use mccrawl::parse::{Parsed, parse};
use mccrawl::robots::RobotsRules;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use url::Url;

#[test]
fn pages_are_deterministic_and_linked() {
    let cfg = MockConfig::default();
    let a = page_html(&cfg, 8080, 4, 7);
    assert_eq!(a, page_html(&cfg, 8080, 4, 7));
    assert_ne!(a, page_html(&cfg, 8080, 4, 8));
    assert_eq!(a.matches("<a href=").count(), cfg.links_per_page as usize);
    assert!(a.len() > 20_000);
    assert!(a.contains(".mock:8080/p/")); // cross-host links carry the port
}

#[test]
fn behaviors_and_robots() {
    assert_eq!(behavior(10), Behavior::DisallowPrivate);
    assert_eq!(robots_txt(12).0, 503);
    assert!(robots_txt(10).1.contains("Disallow: /private"));
    assert!(robots_txt(11).1.contains("Crawl-delay: 2"));
    assert_eq!(robots_txt(4).0, 404);
    assert_eq!(host_index(&host_name(42)), Some(42));
}

#[test]
fn ceiling_excludes_robots_down_and_respects_crawl_delay() {
    let cfg = MockConfig {
        hosts: 10,
        ..Default::default()
    };
    // 8 hosts at 1/s, 1 host at 0.5/s (crawl-delay 2), 1 host down
    assert!((politeness_ceiling(&cfg, Duration::from_secs(1)) - 8.5).abs() < 1e-9);
}

#[test]
fn ceiling_math() {
    let c = |hosts, ms| {
        let cfg = MockConfig {
            hosts,
            ..Default::default()
        };
        politeness_ceiling(&cfg, Duration::from_millis(ms))
    };
    assert!((c(20, 1000) - 17.0).abs() < 1e-9);
    assert!((c(10, 500) - 16.5).abs() < 1e-9); // 8 hosts × 2/s + the crawl-delay host at 0.5/s
    assert!((c(10, 3000) - 3.0).abs() < 1e-9); // min_delay above the crawl-delay: 9 hosts × 1/3
    assert_eq!(c(3, 1000), 1.5); // h0 1/s, h1 0.5/s, h2 down
    assert_eq!(c(0, 1000), 0.0);
    assert!(c(10, 0).is_infinite()); // no delay means no politeness limit
}

#[test]
fn host_names_round_trip_only_in_their_exact_spelling() {
    for i in [0, 1, 42, 999, u32::MAX] {
        assert_eq!(host_index(&host_name(i)), Some(i));
    }
    for bad in [
        "h01.mock",
        "h+1.mock",
        "h-1.mock",
        "h.mock",
        "h 1.mock",
        "h1.mockx",
        "x1.mock",
        "h1.mock:80",
        "H1.mock",
        "h4294967296.mock",
        "",
    ] {
        assert_eq!(host_index(bad), None, "{bad:?}");
    }
}

#[test]
fn each_behavior_means_what_the_crawler_reads_in_its_robots_txt() {
    use Behavior::*;
    let u = |p: &str| Url::parse(&format!("http://h.mock{p}")).unwrap();
    let cycle = [
        DisallowPrivate,
        CrawlDelay2,
        RobotsDown,
        RateLimited,
        Normal,
        Normal,
        Normal,
        Normal,
        Normal,
        Normal,
    ];
    for host in 0..30 {
        let b = behavior(host);
        assert_eq!(b, cycle[host as usize % 10], "h{host}");
        let (status, body) = robots_txt(host);
        let rules = RobotsRules::from_response(status, body.as_bytes(), "mccrawl");
        match b {
            DisallowPrivate => {
                assert!(!rules.allowed(&u("/private/1")) && rules.allowed(&u("/p/1")));
                assert_eq!(rules.crawl_delay(), None);
            }
            CrawlDelay2 => {
                assert!(rules.allowed(&u("/private/1")));
                assert_eq!(rules.crawl_delay(), Some(Duration::from_secs(2)));
            }
            RobotsDown => assert!(matches!(rules, RobotsRules::Unreachable)),
            RateLimited | Normal => {
                assert!(status == 404 && matches!(rules, RobotsRules::AllowAll))
            }
        }
    }
}

fn parse_page(cfg: &MockConfig, port: u16, host: u32, page: u32) -> Parsed {
    let url = Url::parse(&format!("http://{}:{port}/p/{page}", host_name(host))).unwrap();
    parse(&url, page_html(cfg, port, host, page).as_bytes())
}

#[test]
fn pages_parse_into_the_links_the_config_asks_for() {
    let cfg = MockConfig {
        hosts: 50,
        ..Default::default()
    };
    let (mut total, mut cross, mut private) = (0, 0, 0);
    for host in [0, 4, 13] {
        for page in 0..30 {
            let p = parse_page(&cfg, 8080, host, page);
            assert_eq!(p.title, format!("{} page {page}", host_name(host)));
            assert_eq!(p.links.len(), cfg.links_per_page as usize);
            for l in &p.links {
                let h = host_index(l.url.host_str().unwrap()).expect("links stay on mock hosts");
                assert!(h < cfg.hosts && l.url.port() == Some(8080), "{}", l.url);
                let n: u32 = l.url.path().rsplit('/').next().unwrap().parse().unwrap();
                assert!(n < cfg.pages_per_host, "{}", l.url);
                assert_eq!(l.anchor.split(' ').count(), 2);
                total += 1;
                cross += u32::from(h != host);
                if l.url.path().starts_with("/private/") {
                    // Only DisallowPrivate hosts link to /private, and only on their own host.
                    assert!(
                        h == host && behavior(host) == Behavior::DisallowPrivate,
                        "{}",
                        l.url
                    );
                    private += 1;
                }
            }
        }
    }
    let pct = f64::from(cross) * 100.0 / f64::from(total);
    assert!((25.0..35.0).contains(&pct), "{pct:.1}% cross-host");
    assert!(private > 20, "{private} links to /private");
}

#[test]
fn every_tenth_page_is_a_near_duplicate_of_the_one_before() {
    let cfg = MockConfig::default();
    for (copy, orig) in [(9, 8), (19, 18), (499, 498)] {
        let (a, b) = (parse_page(&cfg, 80, 4, copy), parse_page(&cfg, 80, 4, orig));
        assert_eq!(a.text, b.text);
        assert_ne!(a.title, b.title);
        assert_eq!(simhash(&a.text), simhash(&b.text));
        let (ha, hb) = (page_html(&cfg, 80, 4, copy), page_html(&cfg, 80, 4, orig));
        assert_ne!(body_hash(ha.as_bytes()), body_hash(hb.as_bytes())); // not an exact copy
    }
    // Other neighbours are unrelated pages, far apart.
    for (x, y) in [(7, 8), (9, 10), (0, 1)] {
        let (a, b) = (parse_page(&cfg, 80, 4, x), parse_page(&cfg, 80, 4, y));
        assert!(
            hamming(simhash(&a.text), simhash(&b.text)) > 10,
            "{x} vs {y}"
        );
    }
    // The same page number on another host is different too.
    let (a, b) = (parse_page(&cfg, 80, 4, 7), parse_page(&cfg, 80, 5, 7));
    assert!(hamming(simhash(&a.text), simhash(&b.text)) > 10);
}

#[test]
fn config_edge_cases_still_give_valid_pages() {
    let links = |cfg: &MockConfig| parse_page(cfg, 1, 1, 3).links;
    let d = MockConfig::default;
    assert_eq!(
        links(&MockConfig {
            links_per_page: 0,
            ..d()
        })
        .len(),
        0
    );
    assert_eq!(
        links(&MockConfig {
            page_words: 0,
            ..d()
        })
        .len(),
        20
    );
    let few_words = MockConfig {
        page_words: 5,
        links_per_page: 50,
        ..d()
    };
    assert_eq!(links(&few_words).len(), 50);
    // No cross-host links at 0%, only cross-host ones at 100%.
    let same = MockConfig {
        cross_host_pct: 0,
        ..d()
    };
    for page in 0..50 {
        let p = parse_page(&same, 1, 1, page);
        assert!(p.links.iter().all(|l| l.url.host_str() == Some("h1.mock")));
    }
    let one = MockConfig {
        hosts: 1,
        pages_per_host: 1,
        cross_host_pct: 100,
        ..d()
    };
    let p = parse_page(&one, 1, 0, 0);
    assert!(
        p.links
            .iter()
            .all(|l| l.url.as_str() == "http://h0.mock:1/p/0")
    );
    // Degenerate sizes don't panic.
    let empty = MockConfig {
        hosts: 0,
        pages_per_host: 0,
        cross_host_pct: 50,
        ..d()
    };
    let _ = page_html(&empty, 1, 0, 0);
}

/// One raw HTTP/1.1 request; returns (status, lowercased head, body).
async fn get(web: &MockWeb, host: &str, path: &str) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(web.addr).await.unwrap();
    let port = web.addr.port();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head");
    let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
    (head[9..12].parse().unwrap(), head, buf[end + 4..].to_vec())
}

fn quick(hosts: u32) -> MockConfig {
    MockConfig {
        hosts,
        latency_ms: (0, 0),
        ..Default::default()
    }
}

#[tokio::test]
async fn serves_pages_robots_and_special_routes() {
    let cfg = quick(20);
    let web = mockweb::start(cfg.clone(), 0).await.unwrap();
    let port = web.addr.port();
    let (status, head, body) = get(&web, "h4.mock", "/p/7").await;
    assert_eq!(status, 200);
    assert!(head.contains("content-type: text/html"), "{head}");
    assert_eq!(body, page_html(&cfg, port, 4, 7).into_bytes());
    assert_eq!(
        get(&web, "h4.mock", "/").await.2,
        page_html(&cfg, port, 4, 0).into_bytes()
    );
    assert_eq!(get(&web, "h4.mock", "/p/499").await.0, 200);
    assert_eq!(get(&web, "h0.mock", "/private/3").await.0, 200);

    for (host, status, needle) in [
        ("h0.mock", 200, "disallow: /private"),
        ("h1.mock", 200, "crawl-delay: 2"),
        ("h2.mock", 503, ""),
        ("h3.mock", 404, ""),
        ("h4.mock", 404, ""),
    ] {
        let (st, _, body) = get(&web, host, "/robots.txt").await;
        assert_eq!(st, status, "{host}");
        assert!(
            String::from_utf8(body)
                .unwrap()
                .to_ascii_lowercase()
                .contains(needle)
        );
    }
    let (status, head, _) = get(&web, "h4.mock", "/redirect").await;
    assert!(status == 301 && head.contains("location: /p/0"), "{head}");
    let (status, head, _) = get(&web, "h4.mock", "/file.pdf").await;
    assert!(
        status == 200 && head.contains("content-type: application/pdf"),
        "{head}"
    );
    let (status, head, body) = get(&web, "h4.mock", "/big").await;
    assert!(status == 200 && head.contains("text/html") && body.len() >= 3 << 20);

    // Past the last page, malformed numbers, unknown paths and unknown hosts are all 404.
    for (host, path) in [
        ("h4.mock", "/p/500"),
        ("h4.mock", "/p/"),
        ("h4.mock", "/p/x"),
        ("h4.mock", "/nope"),
        ("h20.mock", "/"),
        ("h01.mock", "/"),
        ("example.com", "/"),
    ] {
        assert_eq!(get(&web, host, path).await.0, 404, "{host}{path}");
    }
    assert_eq!(get(&web, "H4.MOCK", "/p/1").await.0, 200); // host names are case-insensitive
}

#[tokio::test]
async fn rate_limited_hosts_answer_429_on_every_third_page() {
    let web = mockweb::start(quick(20), 0).await.unwrap();
    let (mut h3, mut h13) = (Vec::new(), Vec::new());
    for i in 0..6 {
        let (status, head, _) = get(&web, "h3.mock", &format!("/p/{i}")).await;
        if status == 429 {
            assert!(
                head.lines().any(|l| l.trim_end() == "retry-after: 2"),
                "{head}"
            );
        }
        h3.push(status);
        // Requests interleaved with h3's: each rate-limited host keeps its own count.
        h13.push(get(&web, "h13.mock", "/").await.0);
        // robots.txt, other routes and other hosts don't count toward the limit.
        assert_eq!(get(&web, "h3.mock", "/robots.txt").await.0, 404);
        assert_eq!(get(&web, "h3.mock", "/file.pdf").await.0, 200);
        assert_eq!(get(&web, "h4.mock", "/p/1").await.0, 200);
    }
    assert_eq!(h3, [200, 200, 429, 200, 200, 429]);
    assert_eq!(h13, h3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_are_all_served_logged_and_counted() {
    let web = Arc::new(mockweb::start(quick(20), 0).await.unwrap());
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..300 {
        let web = Arc::clone(&web);
        let host = if i % 2 == 0 { "h3.mock" } else { "h4.mock" };
        tasks.spawn(async move { get(&web, host, &format!("/p/{i}")).await.0 });
    }
    let statuses = tasks.join_all().await;
    // 150 requests to the rate-limited host: exactly every 3rd one is refused, even in parallel.
    assert_eq!(statuses.iter().filter(|&&s| s == 429).count(), 50);
    assert_eq!(statuses.iter().filter(|&&s| s == 200).count(), 250);
    assert_eq!(web.hits().len(), 300);
}

#[tokio::test]
async fn every_request_is_logged_with_host_path_and_status() {
    let web = mockweb::start(quick(20), 0).await.unwrap();
    let t0 = Instant::now();
    for (host, path) in [
        ("h4.mock", "/p/1"),
        ("H2.Mock", "/robots.txt"),
        ("nowhere.org", "/x"),
        ("h4.mock", "/p/999"),
        ("h4.mock", "/p/2?utm=1"),
    ] {
        get(&web, host, path).await;
    }
    let hits = web.hits();
    let got: Vec<_> = hits
        .iter()
        .map(|h| (h.host.as_str(), h.path.as_str(), h.status))
        .collect();
    assert_eq!(
        got,
        [
            ("h4.mock", "/p/1", 200),
            ("h2.mock", "/robots.txt", 503),
            ("nowhere.org", "/x", 404),
            ("h4.mock", "/p/999", 404),
            ("h4.mock", "/p/2", 200),
        ]
    );
    assert!(hits[0].at >= t0 && hits.windows(2).all(|w| w[0].at <= w[1].at));
}

#[tokio::test]
async fn responses_wait_the_latency_but_hits_record_arrival() {
    let cfg = MockConfig {
        hosts: 5,
        latency_ms: (150, 150),
        ..Default::default()
    };
    let web = mockweb::start(cfg, 0).await.unwrap();
    let t = Instant::now();
    assert_eq!(get(&web, "h4.mock", "/p/1").await.0, 200);
    assert!(
        t.elapsed() >= Duration::from_millis(150),
        "{:?}",
        t.elapsed()
    );
    // Politeness is judged by when requests arrive, so the log must not wait for the delay.
    assert!(web.hits()[0].at < t + Duration::from_millis(75));
    // Unknown hosts are answered at once.
    let t = Instant::now();
    assert_eq!(get(&web, "nowhere.org", "/").await.0, 404);
    assert!(t.elapsed() < Duration::from_millis(150));
    // The widest range doesn't overflow: the request is logged and then just waits.
    let cfg = MockConfig {
        hosts: 1,
        latency_ms: (0, u64::MAX),
        ..Default::default()
    };
    let web = mockweb::start(cfg, 0).await.unwrap();
    let wait = tokio::time::timeout(Duration::from_millis(300), get(&web, "h0.mock", "/p/1"));
    assert!(
        wait.await.is_err(),
        "answered or dropped instead of waiting"
    );
    assert_eq!(web.hits().len(), 1);
}

#[tokio::test]
async fn resolve_overrides_seeds_and_ports() {
    let web = mockweb::start(quick(3), 0).await.unwrap();
    let port = web.addr.port();
    assert!(web.addr.ip().is_loopback() && port != 0);
    let names: Vec<_> = web.resolve_overrides();
    assert_eq!(
        names,
        (0..3).map(|i| (host_name(i), web.addr)).collect::<Vec<_>>()
    );
    let seeds: Vec<String> = web.seeds(10).iter().map(|u| u.to_string()).collect();
    assert_eq!(
        seeds,
        (0..3)
            .map(|i| format!("http://h{i}.mock:{port}/"))
            .collect::<Vec<_>>()
    );
    assert!(web.seeds(0).is_empty());
    assert_eq!(web.seeds(2).len(), 2);
    // A port that's already taken is an error, not a panic.
    assert!(mockweb::start(quick(1), port).await.is_err());
}
