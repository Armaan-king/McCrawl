use mccrawl::urlnorm::{host_key, normalize};
use url::Url;

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
