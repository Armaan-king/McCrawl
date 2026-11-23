//! robots.txt rules with RFC 9309 status semantics: which URLs a host lets us fetch, and how slowly.

use std::time::Duration;
use texting_robots::Robot;
use url::Url;

/// Longest Crawl-delay we honor. Rules are refetched every 24 h anyway, and the cap keeps a
/// hostile value (`inf`, `1e18`) from overflowing the scheduler's clock arithmetic.
pub const MAX_CRAWL_DELAY: Duration = Duration::from_secs(24 * 60 * 60);

pub enum RobotsRules {
    /// No usable robots.txt (4xx, or a file that fails to parse): everything is allowed.
    AllowAll,
    Parsed(Robot),
    /// Server error or no answer: assume everything is disallowed until it recovers.
    Unreachable,
}

impl RobotsRules {
    /// RFC 9309 §2.3.1: 2xx → obey the file; 4xx → there is no file, so allow all;
    /// 5xx or anything else → unreachable, so disallow all. 429 counts as a server
    /// error (as Google treats it): the server is overloaded, not missing a file.
    ///
    /// texting_robots rejects the *whole* file when one `$`-anchored rule compiles past its
    /// 42 KB pattern limit (a guard against hostile files). Treating that as "allow all" would
    /// ignore every other rule, so we retry with the `$` anchors removed: never less strict.
    pub fn from_response(status: u16, body: &[u8], agent: &str) -> Self {
        match status {
            200..=299 => Robot::new(agent, body)
                .or_else(|_| Robot::new(agent, &strip_end_anchors(body)))
                .map_or(Self::AllowAll, Self::Parsed), // still unparseable: no usable rules
            429 => Self::Unreachable,
            400..=499 => Self::AllowAll,
            _ => Self::Unreachable,
        }
    }

    pub fn allowed(&self, url: &Url) -> bool {
        match self {
            Self::AllowAll => true,
            Self::Parsed(r) => r.allowed(url.as_str()),
            Self::Unreachable => false,
        }
    }

    /// The file's Crawl-delay, capped at `MAX_CRAWL_DELAY`. `try_from_secs_f32` returns an
    /// error on `inf`/overflow, where `from_secs_f32` would panic and take down the crawl.
    pub fn crawl_delay(&self) -> Option<Duration> {
        let Self::Parsed(r) = self else { return None };
        let delay = Duration::try_from_secs_f32(r.delay?).unwrap_or(MAX_CRAWL_DELAY);
        Some(delay.min(MAX_CRAWL_DELAY))
    }
}

/// Rewrite rules containing `$` so they never need a compiled pattern, erring on the strict side:
/// a Disallow without its end anchor blocks a superset, and a dropped Allow only blocks more.
/// Lines end at `\n`, `\r` or `\0`, as in texting_robots; terminators are kept as they were.
fn strip_end_anchors(body: &[u8]) -> Vec<u8> {
    let starts_with = |line: &[u8], word: &[u8]| {
        let line = line.trim_ascii_start();
        line.get(..word.len())
            .is_some_and(|w| w.eq_ignore_ascii_case(word))
    };
    let mut out = Vec::with_capacity(body.len());
    for line in body.split_inclusive(|&b| matches!(b, b'\n' | b'\r' | b'\0')) {
        if !line.contains(&b'$') {
            out.extend_from_slice(line);
        } else if starts_with(line, b"disallow") {
            out.extend(line.iter().filter(|&&b| b != b'$'));
        } else if !starts_with(line, b"allow") {
            out.extend_from_slice(line);
        }
    }
    out
}

/// `scheme://host:port/robots.txt` for any URL on that host.
pub fn robots_url(url: &Url) -> Url {
    let mut u = url.clone();
    u.set_path("/robots.txt");
    u.set_query(None);
    u.set_fragment(None);
    u
}
