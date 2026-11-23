use mccrawl::urlnorm::{MAX_URL_LEN, host_key, normalize};
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

#[test]
fn host_key_for_ip_hosts() {
    let k = |s: &str| host_key(&Url::parse(s).unwrap()).map(str::to_owned);
    assert_eq!(k("http://[::1]:8080/").as_deref(), Some("[::1]"));
    assert_eq!(k("http://127.0.0.1:8080/").as_deref(), Some("127.0.0.1"));
}

#[test]
fn percent_encoding_is_normalized() {
    let b = "https://a.com/";
    // Unreserved characters are decoded; other escapes get uppercase hex (RFC 3986 §6.2.2).
    assert_eq!(
        n(b, "/%7euser/%41b").as_deref(),
        Some("https://a.com/~user/Ab")
    );
    assert_eq!(
        n(b, "/caf%c3%a9").as_deref(),
        Some("https://a.com/caf%C3%A9")
    );
    assert_eq!(n(b, "/café").as_deref(), Some("https://a.com/caf%C3%A9"));
    assert_eq!(
        n(b, "/s?q=a%2fb&x=%7e").as_deref(),
        Some("https://a.com/s?q=a%2Fb&x=~")
    );
    // Reserved characters stay encoded: decoding %2F or %26 would change the meaning.
    assert_eq!(
        n(b, "/a%2Fb?x=1%26y%3D2").as_deref(),
        Some("https://a.com/a%2Fb?x=1%26y%3D2")
    );
    // Malformed escapes are left alone, and don't stop later escapes from being fixed.
    assert_eq!(
        n(b, "/100%/x%zz/%4/a%+1").as_deref(),
        Some("https://a.com/100%/x%zz/%4/a%+1")
    );
    assert_eq!(
        n(b, "/x%zz/%7e?a=%-1&b=%7e").as_deref(),
        Some("https://a.com/x%zz/~?a=%-1&b=~")
    );
}

#[test]
fn host_spellings_collapse() {
    let b = "https://a.com/";
    // A trailing dot is the same host; otherwise it would get a second politeness budget.
    assert_eq!(
        n(b, "https://Example.COM./x").as_deref(),
        Some("https://example.com/x")
    );
    // Credentials are dropped: never meant for a crawler, and they'd leak into the stored output.
    assert_eq!(
        n(b, "https://user:pw@example.com/x").as_deref(),
        Some("https://example.com/x")
    );
    assert_eq!(
        n(b, "https://Bücher.de/").as_deref(),
        Some("https://xn--bcher-kva.de/")
    );
    assert_eq!(n(b, "http://0x7f.1/").as_deref(), Some("http://127.0.0.1/"));
    assert_eq!(
        n(b, "http://[0:0::1]:8080/").as_deref(),
        Some("http://[::1]:8080/")
    );
    assert_eq!(
        n(b, "https://a.com:8443/").as_deref(),
        Some("https://a.com:8443/")
    );
    assert_eq!(n(b, "http://./x"), None); // a host made only of dots
}

#[test]
fn relative_forms_resolve() {
    let b = "https://a.com/dir/page?old=1";
    assert_eq!(n(b, "//b.com/x").as_deref(), Some("https://b.com/x"));
    assert_eq!(
        n(b, "?new=2").as_deref(),
        Some("https://a.com/dir/page?new=2")
    );
    assert_eq!(n(b, "./").as_deref(), Some("https://a.com/dir/"));
    assert_eq!(n(b, "../../../x").as_deref(), Some("https://a.com/x")); // can't climb above root
    assert_eq!(n(b, "/a\tb\nc").as_deref(), Some("https://a.com/abc")); // tabs and newlines stripped
    assert_eq!(n(b, "JavaScript:alert(1)"), None);
    assert_eq!(n(b, " #top"), None);
}

#[test]
fn normalizing_twice_changes_nothing() {
    let b = "https://a.com/dir/";
    for href in [
        "/%7euser/%41b?q=a%2fb&x=%7e#f",
        "https://user:pw@Example.COM.:443/caf%c3%a9?",
        "HTTP://[0:0::1]:80/a/../b/./c",
        "x%zz/100%",
        "?only=query",
        "/a%2Fb?x=1%26y%3D2",
    ] {
        let once = normalize(&Url::parse(b).unwrap(), href).unwrap();
        let twice = normalize(&once, once.as_str()).unwrap();
        assert_eq!(once, twice, "{href}");
    }
}

#[test]
fn length_limit_boundary() {
    let b = "https://a.com/"; // 14 characters
    let exact = n(b, &format!("/{}", "a".repeat(MAX_URL_LEN - 14)));
    assert_eq!(exact.map(|u| u.len()), Some(MAX_URL_LEN));
    assert_eq!(n(b, &format!("/{}", "a".repeat(MAX_URL_LEN - 13))), None);
}

#[test]
fn repeated_segment_rule_ignores_empty_segments_and_counts_anywhere() {
    let b = "https://a.com/";
    assert!(n(b, "/x//y//z//w//v/").is_some()); // "//" isn't a trap
    assert_eq!(n(b, "/a/x/a/y/a/z/a"), None); // non-adjacent repeats count too
}
