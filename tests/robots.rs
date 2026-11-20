use mccrawl::robots::{MAX_CRAWL_DELAY, RobotsRules, robots_url};
use std::time::Duration;
use url::Url;

fn u(s: &str) -> Url {
    Url::parse(s).unwrap()
}

#[test]
fn rfc9309_status_semantics() {
    let txt = b"User-agent: *\nDisallow: /private\nCrawl-delay: 2\n";
    let r = RobotsRules::from_response(200, txt, "mccrawl");
    assert!(r.allowed(&u("https://a.com/ok")));
    assert!(!r.allowed(&u("https://a.com/private/x")));
    assert_eq!(r.crawl_delay(), Some(Duration::from_secs(2)));

    // 4xx: the site has no robots.txt, so everything is allowed.
    let none = RobotsRules::from_response(404, b"", "mccrawl");
    assert!(none.allowed(&u("https://a.com/private")));

    // 5xx, and 429 (overloaded): nothing is allowed until the server recovers.
    for status in [500, 503, 429] {
        let down = RobotsRules::from_response(status, b"", "mccrawl");
        assert!(matches!(down, RobotsRules::Unreachable), "{status}");
        assert!(!down.allowed(&u("https://a.com/")));
    }
}

#[test]
fn agent_specific_group_wins() {
    let txt = b"User-agent: mccrawl\nDisallow: /\n\nUser-agent: *\nAllow: /\n";
    let r = RobotsRules::from_response(200, txt, "mccrawl");
    assert!(!r.allowed(&u("https://a.com/x")));
}

#[test]
fn hostile_crawl_delay_never_panics_and_is_capped() {
    for delay in ["inf", "1e30", "1e15"] {
        let txt = format!("User-agent: *\nCrawl-delay: {delay}\n");
        let r = RobotsRules::from_response(200, txt.as_bytes(), "mccrawl");
        assert!(
            r.crawl_delay().is_none_or(|d| d <= MAX_CRAWL_DELAY),
            "{delay}"
        );
    }
}

#[test]
fn robots_url_keeps_scheme_host_port() {
    let r = robots_url(&u("http://h1.mock:8080/p/1?x=1#f"));
    assert_eq!(r.as_str(), "http://h1.mock:8080/robots.txt");
}
