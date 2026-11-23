//! URL canonicalization: one spelling per page, so the seen-set dedups reliably
//! and crawler traps (endless URL spaces) are rejected before they reach the frontier.

use std::collections::HashMap;
use url::Url;

/// Longer URLs are almost always session junk or traps.
pub const MAX_URL_LEN: usize = 2048;

/// Resolve `href` against `base` and canonicalize it. `None` means "don't crawl this".
/// The WHATWG parser already lowercases scheme/host, drops default ports and resolves `..`;
/// we add what it deliberately keeps: fragments, empty queries, credentials, trailing host dots,
/// percent-encoding variants, non-HTTP schemes, traps. Idempotent: normalizing a result is a no-op.
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
    // Credentials are never meant for a crawler and would leak into stored output.
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    // `example.com.` is the same host as `example.com`; without this it gets a second politeness budget.
    if let Some(host) = url.host_str().filter(|h| h.ends_with('.')) {
        let trimmed = host.trim_end_matches('.').to_owned();
        url.set_host(Some(&trimmed)).ok()?; // fails for an all-dots host
    }
    if let Some(path) = normalize_percent(url.path()) {
        url.set_path(&path);
    }
    match url.query() {
        Some("") => url.set_query(None),
        Some(q) => {
            if let Some(q) = normalize_percent(q) {
                url.set_query(Some(&q));
            }
        }
        None => {}
    }
    if url.as_str().len() > MAX_URL_LEN || has_repeated_segment(&url) {
        return None;
    }
    Some(url)
}

/// RFC 3986 §6.2.2: decode escapes of unreserved characters (`%7E` → `~`, `%41` → `A`) and
/// uppercase the hex of all others (`%2f` → `%2F`). Reserved characters stay encoded, since
/// decoding `%2F` or `%26` would change the URL's meaning; malformed escapes are left alone.
/// `None` if nothing changes. Input is a serialized URL component, so it is pure ASCII.
fn normalize_percent(s: &str) -> Option<String> {
    if !s.contains('%') {
        return None;
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(hex) = s
                .get(i + 1..i + 3)
                .filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit()))
        {
            let v = u8::from_str_radix(hex, 16).ok()?;
            if v.is_ascii_alphanumeric() || b"-._~".contains(&v) {
                out.push(char::from(v));
            } else {
                out.push('%');
                out.push_str(&hex.to_ascii_uppercase());
            }
            i += 3;
        } else {
            out.push(char::from(b[i]));
            i += 1;
        }
    }
    (out != s).then_some(out)
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
