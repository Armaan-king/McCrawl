use mccrawl::parse::{Parsed, decode_entities, parse};
use std::borrow::Cow;
use url::Url;

/// Parse as if fetched from `https://a.com/dir/page`.
fn p(html: &str) -> Parsed {
    parse(
        &Url::parse("https://a.com/dir/page").unwrap(),
        html.as_bytes(),
    )
}

fn urls(p: &Parsed) -> Vec<&str> {
    p.links.iter().map(|l| l.url.as_str()).collect()
}

fn anchors(p: &Parsed) -> Vec<&str> {
    p.links.iter().map(|l| l.anchor.as_str()).collect()
}

const HTML: &str = r#"<html><head><title>Rust &amp; Crawlers</title>
<base href="https://example.com/docs/">
<link rel="canonical" href="/docs/page">
<meta name="robots" content="noindex">
<script>var x = "<a href='/nope'>";</script><style>.a{}</style></head>
<body><h1>Hello</h1><p>World of <b>crawlers</b></p>
<a href="intro.html">Intro <em>guide</em></a>
<a href="/q?a=1&amp;b=2" rel="nofollow ugc">Q</a>
<a href="mailto:x@y.z">mail</a>
<a href="https://other.org/x">Other</a></body></html>"#;

#[test]
fn extracts_links_anchors_meta_and_text() {
    let p = parse(
        &Url::parse("https://example.com/start").unwrap(),
        HTML.as_bytes(),
    );
    assert_eq!(p.title, "Rust & Crawlers");
    assert_eq!(
        p.canonical.unwrap().as_str(),
        "https://example.com/docs/page"
    );
    assert!(p.noindex && !p.nofollow);
    let links: Vec<_> = p
        .links
        .iter()
        .map(|l| (l.url.as_str(), l.anchor.as_str(), l.nofollow))
        .collect();
    assert_eq!(
        links,
        vec![
            ("https://example.com/docs/intro.html", "Intro guide", false),
            ("https://example.com/q?a=1&b=2", "Q", true),
            ("https://other.org/x", "Other", false),
        ]
    );
    assert!(p.text.contains("Hello World of crawlers"), "{}", p.text);
    assert!(!p.text.contains("var x") && !p.text.contains(".a{}"));
}

#[test]
fn meta_robots_none_means_noindex_and_nofollow() {
    let p = parse(
        &Url::parse("https://a.com/").unwrap(),
        br#"<meta name="ROBOTS" content="None">"#,
    );
    assert!(p.noindex && p.nofollow);
}

#[test]
fn decodes_entities_including_numeric() {
    assert_eq!(
        decode_entities("a &lt;b&gt; &#65;&#x42; &amp;amp; &bogus; &"),
        "a <b> AB &amp; &bogus; &"
    );
}

#[test]
fn entity_edge_cases() {
    let d = |s: &str| decode_entities(s).into_owned();
    // HTML's numeric rules: NUL, surrogates and out-of-range become U+FFFD; 0x80–0x9F are Windows-1252.
    assert_eq!(
        d("&#0;&#xD800;&#x110000;&#99999999;"),
        "\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}"
    );
    assert_eq!(
        d("&#150;&#x80;&#x9F;&#129;&#x7F;&#xA0;"),
        "–€Ÿ\u{81}\u{7F}\u{A0}"
    );
    assert_eq!(d("&#x1F600;&#X41;&#0065;&#x0000041;"), "😀AAA");
    // Not references, so left exactly as written: no `;`, no digits, bad digits, unknown or
    // overlong names, and `&` on its own.
    let long = format!("&{};", "a".repeat(40));
    for s in [
        "&amp", "&#65", "&#;", "&#x;", "&#xZZ;", "&#65x;", "&#-1;", "&#+1;", "&nosuch;", "&Amp;",
        "& amp;", "AT&T", "&&;", "&;", "&", &long,
    ] {
        assert_eq!(d(s), s);
    }
    // The Latin-1 table end to end (first, middle and last names), and typography.
    assert_eq!(
        d("&nbsp;&iexcl;&copy;&iquest;&Agrave;&eacute;&times;&szlig;&divide;&yuml;"),
        "\u{A0}¡©¿Àé×ß÷ÿ"
    );
    assert_eq!(
        d("&mdash;&ndash;&hellip;&lsquo;&rsquo;&ldquo;&rdquo;&trade;&euro;&AMP;&apos;&harr;"),
        "—–…‘’“”™€&'↔"
    );
    // Multibyte text around references survives; no `&` means no copy.
    assert_eq!(d("日本&amp;語&"), "日本&語&");
    assert!(matches!(decode_entities("plain"), Cow::Borrowed("plain")));
}

#[test]
fn text_is_visible_words_only() {
    let p = p(
        "<html><head><title>T</title><style>p{}</style><script>let a = 1;</script>\
        <noscript>enable js</noscript></head><body><!-- secret --><template>tpl</template>\
        <table><tr><td>a</td><td>b</td></tr></table><p>x&nbsp;&nbsp;y\n\t z &lt;tag&gt;</p>\
        <textarea>typed</textarea><a href=\"/l\">link</a></body></html>",
    );
    assert_eq!(p.text, "tpl a b x y z <tag> link");
}

#[test]
fn title_edge_cases() {
    assert_eq!(p("<title>  A &amp;\n B  </title>").title, "A & B");
    assert_eq!(
        p("<title>First</title><title>Second</title>").title,
        "First"
    );
    // An SVG <title> is an icon's tooltip, not the page title (but it is visible text).
    let svg = p("<title>Real</title><svg><title>Close</title></svg>");
    assert_eq!((svg.title.as_str(), svg.text.as_str()), ("Real", "Close"));
    assert_eq!(p("<body><svg><title>Close</title></svg>").title, "");
    assert_eq!(p("<p>no title</p>").title, "");
    assert_eq!(p("<title></title><title>Late</title>").title, "Late");
    // Inside <title>, tags are just text.
    assert_eq!(p("<title>a <b>b</b></title>").title, "a <b>b</b>");
}

#[test]
fn link_resolution() {
    let page = p(
        r#"<a href="x">1</a><a href="../up">2</a><a href="/abs">3</a>
        <a href="//cdn.b.org/s">4</a><a href="?q=1">5</a><a href="  /trim  ">6</a>
        <A HREF="/UP">7</A><a href="/d">8</a><a href="/d#frag">9</a>"#,
    );
    assert_eq!(
        urls(&page),
        [
            "https://a.com/dir/x",
            "https://a.com/up",
            "https://a.com/abs",
            "https://cdn.b.org/s",
            "https://a.com/dir/page?q=1",
            "https://a.com/trim",
            "https://a.com/UP",
            "https://a.com/d",
            "https://a.com/d", // duplicates are kept: each has its own anchor text
        ]
    );
    // Entities in an href are decoded; `&copy=` has no `;`, so it stays (as in browsers).
    let page = p(r#"<a href="/q?a=1&amp;b=2&#38;c=3&copy=4&#x2F;">q</a>"#);
    assert_eq!(urls(&page), ["https://a.com/q?a=1&b=2&c=3&copy=4/"]);
    // Links inside comments and script strings are not links.
    let page =
        p(r#"<!-- <a href="/c">c</a> --><script>w('<a href="/s">')</script><a href="/ok">ok</a>"#);
    assert_eq!(urls(&page), ["https://a.com/ok"]);
}

#[test]
fn only_the_first_base_counts() {
    let u = |html: &str| {
        p(html)
            .links
            .into_iter()
            .map(|l| l.url.to_string())
            .collect::<Vec<_>>()
    };
    // A relative base resolves against the page URL; a second base is ignored.
    assert_eq!(
        u(r#"<base href="/other/"><base href="https://evil.com/"><a href="x">x</a>"#),
        ["https://a.com/other/x"]
    );
    // An unusable first base falls back to the page URL, and later bases stay ignored.
    assert_eq!(
        u(r#"<base href="javascript:void(0)"><base href="https://evil.com/"><a href="x">x</a>"#),
        ["https://a.com/dir/x"]
    );
    // <base target> without an href doesn't count.
    assert_eq!(
        u(r#"<base target="_blank"><base href="https://b.com/r/"><a href="x">x</a>"#),
        ["https://b.com/r/x"]
    );
}

#[test]
fn rejected_links_are_skipped_and_their_text_goes_nowhere() {
    let page = p(
        r##"<a href="/a">A</a><a href="mailto:x@y.z">M</a><a href="javascript:go()">J</a>
        <a href="tel:1">T</a><a href="data:text/html,hi">D</a><a href="#top">H</a><a href="">E</a>
        <a href="ftp://f.com/">F</a><a name="anchor">N</a><a href="/b">B</a>"##,
    );
    assert_eq!(urls(&page), ["https://a.com/a", "https://a.com/b"]);
    assert_eq!(anchors(&page), ["A", "B"]);
}

#[test]
fn anchor_text_details() {
    let page = p(r#"<a href="/1">Read <img src="i.png" alt="more">now</a>
        <a href="/2"><img alt="Logo"></a>
        <a href="/3">Hi<script>evil()</script><style>.x{}</style></a>
        <a href="/4">  multi
           line &amp; spaced </a>
        <a href="/5"></a><img alt="not in a link">
        <a href="/6">one<a href="/7">two</a>"#);
    assert_eq!(
        anchors(&page),
        [
            "Read more now",
            "Logo",
            "Hi",
            "multi line & spaced",
            "",
            "one",
            "two"
        ]
    );
}

#[test]
fn rel_nofollow_is_a_token_match() {
    let page = p(
        r#"<a href="/1" rel="NoFollow">1</a><a href="/2" rel="external  nofollow noopener">2</a>
        <a href="/3" rel="nofollowx">3</a><a href="/4" rel="ugc sponsored">4</a><a href="/5">5</a>"#,
    );
    let flags: Vec<bool> = page.links.iter().map(|l| l.nofollow).collect();
    assert_eq!(flags, [true, true, false, false, false]);
    assert!(!page.nofollow); // a link's rel doesn't set the page-level flag
}

#[test]
fn canonical_edge_cases() {
    let c = |html: &str| p(html).canonical.map(|u| u.to_string());
    assert_eq!(
        c(r#"<link rel="canonical" href="/c#frag">"#).as_deref(),
        Some("https://a.com/c")
    );
    assert_eq!(
        c(r#"<link rel="Canonical" href="/1"><link rel="canonical" href="/2">"#).as_deref(),
        Some("https://a.com/1") // the first one wins
    );
    assert_eq!(
        c(r#"<base href="/b/"><link rel="canonical" href="c">"#).as_deref(),
        Some("https://a.com/b/c")
    );
    assert_eq!(
        c(r#"<link rel="alternate" href="/alt"><link rel="stylesheet" href="/s.css">"#),
        None
    );
    assert_eq!(c(r#"<link rel="canonical" href="javascript:x">"#), None);
    assert_eq!(c(r#"<a rel="canonical" href="/a">a</a>"#), None);
    assert_eq!(c("<p>none</p>"), None);
}

#[test]
fn meta_robots_directives() {
    let f = |html: &str| {
        let page = p(html);
        (page.noindex, page.nofollow)
    };
    assert_eq!(
        f(r#"<meta name="robots" content="noindex, nofollow">"#),
        (true, true)
    );
    assert_eq!(
        f(r#"<meta name="robots" content=",,NOFOLLOW ,">"#),
        (false, true)
    );
    assert_eq!(
        f(r#"<meta name=" Robots " content="noindex">"#),
        (true, false)
    );
    assert_eq!(
        f(r#"<meta name="robots" content="index, follow, all">"#),
        (false, false)
    );
    assert_eq!(
        f(r#"<meta name="robots" content="noindexing nofollowed">"#),
        (false, false)
    );
    assert_eq!(
        f(r#"<meta name="googlebot" content="noindex">"#),
        (false, false)
    );
    assert_eq!(
        f(r#"<meta name="description" content="none">"#),
        (false, false)
    );
    assert_eq!(
        f(r#"<meta property="robots" content="noindex">"#),
        (false, false)
    );
    // Several tags add up.
    assert_eq!(
        f(r#"<meta name="robots" content="noindex"><meta name="robots" content="nofollow">"#),
        (true, true)
    );
}

#[test]
fn encodings() {
    let url = Url::parse("https://a.com/").unwrap();
    // <meta charset> switches the decoder: 0xE9 is "é" in Windows-1252, not valid UTF-8.
    let page = parse(
        &url,
        b"<meta charset=\"windows-1252\"><title>caf\xe9</title><p>na\xefve</p>",
    );
    assert_eq!((page.title.as_str(), page.text.as_str()), ("café", "naïve"));
    let page = parse(
        &url,
        b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=windows-1252\"><p>\x93q\x94</p>",
    );
    assert_eq!(page.text, "“q”");
    // Default UTF-8: multibyte text survives, and invalid bytes become U+FFFD instead of a panic.
    assert_eq!(
        parse(&url, "<p>日本語 текст</p>".as_bytes()).text,
        "日本語 текст"
    );
    assert_eq!(parse(&url, b"<p>a\xff\xfeb</p>").text, "a\u{FFFD}\u{FFFD}b");
    // A UTF-8 byte-order mark is not text, and it outranks <meta charset> (HTML spec).
    assert_eq!(parse(&url, b"\xef\xbb\xbf<p>hi</p>").text, "hi");
    let page = parse(
        &url,
        b"\xef\xbb\xbf<meta charset=\"windows-1252\"><p>caf\xc3\xa9</p>",
    );
    assert_eq!(page.text, "café");
    assert_eq!(parse(&url, b"<p>\xef\xbb\xbfmid</p>").text, "\u{FEFF}mid"); // only a leading BOM
}

#[test]
fn broken_and_hostile_html_never_panics() {
    let url = Url::parse("https://a.com/").unwrap();
    let cases: [&[u8]; 14] = [
        b"",
        b"<",
        b"<a href=\"/x",
        b"\x00\x01\x02<<<>>>",
        b"</a></a></div></html>",
        b"<!--",
        b"<script>",
        b"<title>",
        b"<![CDATA[x",
        b"<a href=/x><a href=/y></a></a></a>",
        b"<svg><a href=\"/s\">s</a><math><mi>x",
        b"<!DOCTYPE html PUBLIC \"\"><html><<a href=/z>",
        b"<meta charset=\"no-such-encoding\"><p>ok</p>",
        b"<meta charset=\"utf-16\"><p>ok</p>",
    ];
    for c in cases {
        let page = parse(&url, c);
        let _ = (page.title, page.text, page.links);
    }
    // Unclosed and truncated pages keep what was read.
    let page = parse(&url, b"<title>Cut<a href=\"/never\">");
    assert_eq!(page.links.len(), 0);
    let page = parse(&url, b"<p>Cut off <a href=/x>mid");
    assert_eq!(
        (urls(&page), anchors(&page)),
        (vec!["https://a.com/x"], vec!["mid"])
    );
    // Deep nesting doesn't overflow the stack.
    let deep = "<div>".repeat(100_000) + "<a href=\"/deep\">d</a>";
    assert_eq!(urls(&parse(&url, deep.as_bytes())), ["https://a.com/deep"]);
}

#[test]
fn ambiguous_markup_does_not_stop_the_parse() {
    // A <script> inside <select> is valid HTML, but lol_html's default strict mode stops reading
    // there (and after any <frameset>). A crawler must read the rest of the page.
    for html in [
        r#"<a href="/before">b</a><select><option>o</option><script>var s;</script></select><a href="/after">a</a>"#,
        r#"<a href="/before">b</a><frameset><noscript>no</noscript></frameset><a href="/after">a</a>"#,
    ] {
        assert_eq!(
            urls(&p(html)),
            ["https://a.com/before", "https://a.com/after"]
        );
    }
}

#[test]
fn big_page() {
    let body: String = (0..50_000)
        .map(|i| format!("<p>w{i} <a href=\"/p/{i}\">l{i}</a></p>"))
        .collect();
    let page = parse(&Url::parse("https://a.com/").unwrap(), body.as_bytes());
    assert_eq!(page.links.len(), 50_000);
    assert_eq!(page.links[49_999].url.as_str(), "https://a.com/p/49999");
    assert_eq!(page.links[49_999].anchor, "l49999");
    assert!(page.text.starts_with("w0 l0 w1 l1 "));
}
