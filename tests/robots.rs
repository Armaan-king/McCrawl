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

fn delay(v: &str) -> Option<Duration> {
    let txt = format!("User-agent: *\nCrawl-delay: {v}\n");
    RobotsRules::from_response(200, txt.as_bytes(), "mccrawl").crawl_delay()
}

#[test]
fn hostile_crawl_delay_never_panics_and_is_capped() {
    // "Forever" or absurdly long becomes the longest wait we honor, never a crash or zero.
    for v in ["inf", "1e30", "1e15"] {
        assert_eq!(delay(v), Some(MAX_CRAWL_DELAY), "{v}");
    }
    assert_eq!(delay("0.5"), Some(Duration::from_millis(500)));
    assert_eq!(delay("0"), Some(Duration::ZERO));
    for v in ["-5", "NaN", "abc", ""] {
        assert_eq!(delay(v), None, "{v:?}"); // the parser rejects these lines
    }
    assert_eq!(RobotsRules::AllowAll.crawl_delay(), None);
    assert_eq!(RobotsRules::Unreachable.crawl_delay(), None);
}

#[test]
fn status_code_boundaries() {
    let txt = b"User-agent: *\nDisallow: /x\n";
    let allows = |status: u16, path: &str| {
        RobotsRules::from_response(status, txt, "mccrawl")
            .allowed(&u(&format!("https://a.com{path}")))
    };
    for st in [200, 204, 299] {
        assert!(
            allows(st, "/y") && !allows(st, "/x"),
            "{st}: 2xx obeys the file"
        );
    }
    for st in [400, 401, 403, 404, 410, 428, 430, 499] {
        assert!(allows(st, "/x"), "{st}: 4xx means no file, so allow all");
    }
    for st in [
        0, 100, 199, 300, 301, 304, 399, 429, 500, 503, 599, 600, 999,
    ] {
        assert!(!allows(st, "/y"), "{st}: unreachable, so allow nothing");
    }
}

#[test]
fn odd_files_never_panic() {
    let bodies: [&[u8]; 5] = [
        b"",
        b"\xff\xfe\x00garbage\x00\n\n",
        b"User-agent: *\nDisallow: /a\x00b\n",
        "User-agent: *\nDisallow: /é\n".as_bytes(),
        b"Disallow: /orphan-rule-without-a-group\n",
    ];
    for body in bodies {
        let r = RobotsRules::from_response(200, body, "mccrawl");
        let _ = (r.allowed(&u("https://a.com/x")), r.crawl_delay());
    }
    // An empty file has no rules, so everything is allowed.
    assert!(RobotsRules::from_response(200, b"", "mccrawl").allowed(&u("https://a.com/x")));
}

#[test]
fn big_file_parses_and_matches() {
    let rules: String = (0..20_000).map(|i| format!("Disallow: /p{i}/\n")).collect();
    let txt = format!("User-agent: *\n{rules}");
    let r = RobotsRules::from_response(200, txt.as_bytes(), "mccrawl");
    assert!(!r.allowed(&u("https://a.com/p19999/x")));
    assert!(r.allowed(&u("https://a.com/q")));
}

#[test]
fn rule_matching_details() {
    let txt =
        b"User-agent: *\nDisallow: /a\nAllow: /a/b\nDisallow: /*.pdf$\nDisallow: /search?q=\n";
    let r = RobotsRules::from_response(200, txt, "mccrawl");
    assert!(!r.allowed(&u("https://a.com/a/c")));
    assert!(r.allowed(&u("https://a.com/a/b/c"))); // the longer Allow beats the shorter Disallow
    assert!(!r.allowed(&u("https://a.com/doc.pdf")));
    assert!(r.allowed(&u("https://a.com/doc.pdf?v=1"))); // `$` anchors the end
    assert!(!r.allowed(&u("https://a.com/search?q=rust"))); // rules can match the query
    assert!(r.allowed(&u("https://a.com/search")));
    let txt = b"User-agent: McCrawl\nDisallow: /\n"; // agent names match case-insensitively
    assert!(!RobotsRules::from_response(200, txt, "mccrawl").allowed(&u("https://a.com/x")));
}

#[test]
fn one_oversized_rule_does_not_void_the_others() {
    // The library rejects a whole file when one `$` rule compiles too large. We must still obey
    // the other rules, and treat the oversized one strictly (without its `$`), never "allow all".
    let huge = format!("/{}x$", "a*".repeat(5_000));
    for eol in ["\n", "\r\n", "\r"] {
        let txt = [
            "User-agent: *",
            "Disallow: /private",
            &format!("Disallow: {huge}"),
            "Disallow: /pub",
            "Allow: /pub/*.html$",
            "",
        ]
        .join(eol);
        let r = RobotsRules::from_response(200, txt.as_bytes(), "mccrawl");
        let eol = eol.escape_debug();
        assert!(
            !r.allowed(&u("https://a.com/private/1")),
            "{eol}: other rules still apply"
        );
        let big = format!("https://a.com/{}xyz", "a".repeat(5_000));
        assert!(
            !r.allowed(&u(&big)),
            "{eol}: the oversized rule, minus `$`, blocks more"
        );
        assert!(
            !r.allowed(&u("https://a.com/pub/x.html")),
            "{eol}: the `$` Allow is dropped"
        );
        assert!(r.allowed(&u("https://a.com/other")), "{eol}");
    }
}

#[test]
fn robots_url_edge_cases() {
    let r = |s: &str| robots_url(&u(s)).to_string();
    assert_eq!(r("https://a.com/"), "https://a.com/robots.txt");
    assert_eq!(r("https://a.com:443/x"), "https://a.com/robots.txt");
    assert_eq!(r("http://[::1]:8080/x"), "http://[::1]:8080/robots.txt");
    assert_eq!(
        r("https://a.com/deep/robots.txt?x"),
        "https://a.com/robots.txt"
    );
}

#[test]
fn robots_url_keeps_scheme_host_port() {
    let r = robots_url(&u("http://h1.mock:8080/p/1?x=1#f"));
    assert_eq!(r.as_str(), "http://h1.mock:8080/robots.txt");
}
