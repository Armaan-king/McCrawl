//! URL canonicalization: one spelling per page, so the seen-set dedups reliably
//! and crawler traps (endless URL spaces) are rejected before they reach the frontier.

use std::collections::HashMap;
use url::Url;

/// Longer URLs are almost always session junk or traps.
pub const MAX_URL_LEN: usize = 2048;

/// Resolve `href` against `base` and canonicalize it. `None` means "don't crawl this".
/// The WHATWG parser already lowercases scheme/host, drops default ports and resolves `..`;
/// we add what it deliberately keeps: fragments, empty queries, non-HTTP schemes, traps.
pub fn normalize(base: &Url, href: &str) -> Option<Url> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None; // same-page anchor
    }
    let mut url = base.join(href).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None; // mailto:, javascript:, ftp:, data:, ...
    }
    url.set_fragment(None);
    if url.query() == Some("") {
        url.set_query(None);
    }
    if url.as_str().len() > MAX_URL_LEN || has_repeated_segment(&url) {
        return None;
    }
    Some(url)
}

/// Trap signature: one path segment appearing more than 3 times (`/a/b/a/b/a/b/a/`),
/// typical of relative-link loops and calendar-style infinite URL spaces.
fn has_repeated_segment(url: &Url) -> bool {
    let mut counts: HashMap<&str, u8> = HashMap::new();
    url.path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .any(|seg| {
            let c = counts.entry(seg).or_insert(0);
            *c += 1;
            *c > 3
        })
}

/// Politeness key: the lowercase hostname (no port).
pub fn host_key(url: &Url) -> Option<&str> {
    url.host_str()
}
