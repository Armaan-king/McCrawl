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

#[cfg(test)]
mod tests {
    use super::*;

    fn n(base: &str, href: &str) -> Option<String> {
        normalize(&Url::parse(base).unwrap(), href).map(|u| u.to_string())
    }

    #[test]
    fn canonicalizes() {
        let b = "https://Example.com/a/b.html";
        assert_eq!(
            n(b, "c.html").as_deref(),
            Some("https://example.com/a/c.html")
        );
        assert_eq!(
            n(b, "../x?q=1#frag").as_deref(),
            Some("https://example.com/x?q=1")
        );
        assert_eq!(
            n(b, "HTTP://EXAMPLE.com:80/").as_deref(),
            Some("http://example.com/")
        );
        assert_eq!(
            n(b, "https://example.com:443/p?").as_deref(),
            Some("https://example.com/p")
        );
        assert_eq!(
            n(b, "  /trim  ").as_deref(),
            Some("https://example.com/trim")
        );
    }

    #[test]
    fn rejects() {
        let b = "https://example.com/";
        for bad in [
            "mailto:a@b.c",
            "javascript:void(0)",
            "ftp://x.com/",
            "",
            "#top",
            "   ",
        ] {
            assert_eq!(n(b, bad), None, "{bad:?}");
        }
        assert_eq!(n(b, &format!("/{}", "a".repeat(2100))), None);
        assert_eq!(n(b, "/a/b/a/b/a/b/a/b"), None); // "a" appears 4 times
        assert!(n(b, "/a/b/a/b/a/b").is_some()); // 3 times is fine
    }

    #[test]
    fn host_key_is_lowercase_hostname_without_port() {
        let u = Url::parse("https://Docs.Rust-lang.org:8443/x").unwrap();
        assert_eq!(host_key(&u), Some("docs.rust-lang.org"));
    }
}
