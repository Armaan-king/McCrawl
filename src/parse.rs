//! One streaming pass over a page's HTML (lol_html, no DOM): links with their anchor text, the
//! title, the visible text, the canonical URL and the robots meta directives.

use crate::urlnorm::normalize;
use lol_html::html_content::{TextChunk, TextType};
use lol_html::{HtmlRewriter, Settings, doc_text, element, text};
use std::borrow::Cow;
use std::cell::RefCell;
use url::Url;

#[derive(Debug)]
pub struct Link {
    pub url: Url,
    /// The link's visible text, plus the `alt` text of images inside it.
    pub anchor: String,
    /// `rel="nofollow"`: the page doesn't vouch for the target.
    pub nofollow: bool,
}

#[derive(Debug, Default)]
pub struct Parsed {
    /// The first HTML `<title>`, whitespace collapsed.
    pub title: String,
    /// Visible text (no script, style or title), whitespace collapsed to single spaces.
    pub text: String,
    pub canonical: Option<Url>,
    /// `<meta name="robots">` asks us not to index this page.
    pub noindex: bool,
    /// `<meta name="robots">` asks us not to follow any link on this page.
    pub nofollow: bool,
    /// Valid http(s) links in document order, normalized; duplicates kept.
    pub links: Vec<Link>,
}

/// Text is collected raw and cleaned once at the end, so entities split across chunks still decode.
struct State {
    out: Parsed,
    base: Url,
    base_seen: bool,
    /// The link collecting anchor text; `None` inside an `<a>` whose href was rejected.
    anchor: Option<usize>,
    title_done: bool,
}

// ponytail: streaming limits: a link before `<base>` resolves against the page URL, `<noscript>`
// is raw text to lol_html (links inside are missed), and only `<meta charset>` sets the encoding
// (the HTTP Content-Type charset isn't passed in). Sub-project 5 can re-parse the raw HTML from WARC.
pub fn parse(page_url: &Url, html: &[u8]) -> Parsed {
    // A UTF-8 byte-order mark is not text, and it settles the encoding (it beats <meta charset>).
    let (html, bom) = match html.strip_prefix(b"\xef\xbb\xbf") {
        Some(rest) => (rest, true),
        None => (html, false),
    };
    let st = RefCell::new(State {
        out: Parsed::default(),
        base: page_url.clone(),
        base_seen: false,
        anchor: None,
        title_done: false,
    });
    let settings = Settings::new()
        // lol_html's default bails out on ambiguous markup; a crawler keeps reading instead.
        .with_strict(false)
        .with_adjust_charset_on_meta_tag(!bom)
        .append_element_content_handler(element!("base[href]", |el| {
            let s = &mut *st.borrow_mut();
            if !s.base_seen {
                s.base_seen = true; // only the first <base href> counts, even if unusable
                if let Some(base) = resolve(page_url, el.get_attribute("href")) {
                    s.base = base;
                }
            }
            Ok(())
        }))
        .append_element_content_handler(element!("a[href]", |el| {
            let s = &mut *st.borrow_mut();
            let url = resolve(&s.base, el.get_attribute("href"));
            let nofollow = el
                .get_attribute("rel")
                .is_some_and(|r| has_token(&r, "nofollow"));
            s.anchor = url.map(|url| {
                s.out.links.push(Link {
                    url,
                    anchor: String::new(),
                    nofollow,
                });
                s.out.links.len() - 1
            });
            Ok(())
        }))
        .append_element_content_handler(text!("a[href]", |t| {
            let s = &mut *st.borrow_mut();
            if let Some(i) = s.anchor
                && t.text_type() == TextType::Data
            {
                push_text(&mut s.out.links[i].anchor, t);
            }
            Ok(())
        }))
        .append_element_content_handler(element!("a[href] img[alt]", |el| {
            let s = &mut *st.borrow_mut();
            if let Some(i) = s.anchor
                && let Some(alt) = el.get_attribute("alt")
            {
                let anchor = &mut s.out.links[i].anchor;
                anchor.push_str(&alt);
                anchor.push(' ');
            }
            Ok(())
        }))
        .append_element_content_handler(text!("title", |t| {
            let s = &mut *st.borrow_mut();
            // The HTML <title> is RCData; an SVG <title> (an icon's tooltip) is plain Data.
            if !s.title_done && t.text_type() == TextType::RCData {
                s.out.title.push_str(t.as_str());
                s.title_done = t.last_in_text_node();
            }
            Ok(())
        }))
        .append_element_content_handler(element!("link[rel][href]", |el| {
            let s = &mut *st.borrow_mut();
            if s.out.canonical.is_none()
                && el
                    .get_attribute("rel")
                    .is_some_and(|r| has_token(&r, "canonical"))
            {
                s.out.canonical = resolve(&s.base, el.get_attribute("href"));
            }
            Ok(())
        }))
        .append_element_content_handler(element!("meta[name][content]", |el| {
            let is_robots = el
                .get_attribute("name")
                .is_some_and(|n| n.trim().eq_ignore_ascii_case("robots"));
            if is_robots {
                let content = el.get_attribute("content").unwrap_or_default();
                let mut s = st.borrow_mut();
                for directive in content.split(|c: char| c == ',' || c.is_ascii_whitespace()) {
                    match directive.to_ascii_lowercase().as_str() {
                        "noindex" => s.out.noindex = true,
                        "nofollow" => s.out.nofollow = true,
                        "none" => (s.out.noindex, s.out.nofollow) = (true, true),
                        _ => {}
                    }
                }
            }
            Ok(())
        }))
        .append_document_content_handler(doc_text!(|t| {
            if t.text_type() == TextType::Data {
                push_text(&mut st.borrow_mut().out.text, t);
            }
            Ok(())
        }));

    let mut rewriter = HtmlRewriter::new(settings, |_: &[u8]| {});
    // An error (only a memory limit, with strict mode off) ends the pass; what was found is kept.
    let _ = rewriter.write(html).and_then(|()| rewriter.end());

    let mut out = st.into_inner().out;
    out.title = clean(&out.title);
    out.text = clean(&out.text);
    for link in &mut out.links {
        link.anchor = clean(&link.anchor);
    }
    out
}

fn resolve(base: &Url, href: Option<String>) -> Option<Url> {
    normalize(base, &decode_entities(&href?))
}

/// `rel` is a space-separated, case-insensitive token list.
fn has_token(list: &str, token: &str) -> bool {
    list.split_ascii_whitespace()
        .any(|t| t.eq_ignore_ascii_case(token))
}

/// Every text node ends with a space, so `<td>a</td><td>b</td>` reads "a b". The price is that
/// `<b>H</b>ello` reads "H ello": splitting a rare word beats gluing every table cell together.
fn push_text(buf: &mut String, t: &TextChunk) {
    buf.push_str(t.as_str());
    if t.last_in_text_node() {
        buf.push(' ');
    }
}

/// Decode entities, then collapse whitespace runs (`&nbsp;` included) to single spaces.
fn clean(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for word in decode_entities(raw).split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// Decode HTML character references: every numeric one (`&#65;`, `&#x42;`) and the common named
/// ones. The `;` is required, and anything that isn't a known reference is left as written.
pub fn decode_entities(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        // A name is ASCII letters, digits and `#`. The scan stops before the next `&`, so the
        // whole decode is linear even on hostile input like `&&&…`.
        let len = rest
            .bytes()
            .take_while(|&b| b.is_ascii_alphanumeric() || b == b'#')
            .count();
        if rest[len..].starts_with(';')
            && let Some(c) = char_ref(&rest[..len])
        {
            out.push(c);
            rest = &rest[len + 1..];
        } else {
            out.push('&');
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The Latin-1 entities, U+00A0 to U+00FF in order.
const LATIN1: [&str; 96] = [
    "nbsp", "iexcl", "cent", "pound", "curren", "yen", "brvbar", "sect", "uml", "copy", "ordf",
    "laquo", "not", "shy", "reg", "macr", "deg", "plusmn", "sup2", "sup3", "acute", "micro",
    "para", "middot", "cedil", "sup1", "ordm", "raquo", "frac14", "frac12", "frac34", "iquest",
    "Agrave", "Aacute", "Acirc", "Atilde", "Auml", "Aring", "AElig", "Ccedil", "Egrave", "Eacute",
    "Ecirc", "Euml", "Igrave", "Iacute", "Icirc", "Iuml", "ETH", "Ntilde", "Ograve", "Oacute",
    "Ocirc", "Otilde", "Ouml", "times", "Oslash", "Ugrave", "Uacute", "Ucirc", "Uuml", "Yacute",
    "THORN", "szlig", "agrave", "aacute", "acirc", "atilde", "auml", "aring", "aelig", "ccedil",
    "egrave", "eacute", "ecirc", "euml", "igrave", "iacute", "icirc", "iuml", "eth", "ntilde",
    "ograve", "oacute", "ocirc", "otilde", "ouml", "divide", "oslash", "ugrave", "uacute", "ucirc",
    "uuml", "yacute", "thorn", "yuml",
];

// ponytail: Latin-1 plus HTML 4's special characters, not the full 2,231-name HTML5 table
// (Greek letters and math symbols stay as written); add names if real pages need them.
fn char_ref(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let n = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return Some(numeric_ref(n));
    }
    if let Some(i) = LATIN1.iter().position(|&e| e == name) {
        return char::from_u32(0xA0 + i as u32);
    }
    Some(match name {
        "amp" | "AMP" => '&',
        "lt" | "LT" => '<',
        "gt" | "GT" => '>',
        "quot" | "QUOT" => '"',
        "apos" => '\'',
        "OElig" => 'Œ',
        "oelig" => 'œ',
        "Scaron" => 'Š',
        "scaron" => 'š',
        "Yuml" => 'Ÿ',
        "fnof" => 'ƒ',
        "circ" => 'ˆ',
        "tilde" => '˜',
        "ensp" => '\u{2002}',
        "emsp" => '\u{2003}',
        "thinsp" => '\u{2009}',
        "zwnj" => '\u{200C}',
        "zwj" => '\u{200D}',
        "lrm" => '\u{200E}',
        "rlm" => '\u{200F}',
        "ndash" => '–',
        "mdash" => '—',
        "lsquo" => '‘',
        "rsquo" => '’',
        "sbquo" => '‚',
        "ldquo" => '“',
        "rdquo" => '”',
        "bdquo" => '„',
        "dagger" => '†',
        "Dagger" => '‡',
        "bull" => '•',
        "hellip" => '…',
        "permil" => '‰',
        "prime" => '′',
        "Prime" => '″',
        "lsaquo" => '‹',
        "rsaquo" => '›',
        "euro" => '€',
        "trade" => '™',
        "larr" => '←',
        "uarr" => '↑',
        "rarr" => '→',
        "darr" => '↓',
        "harr" => '↔',
        _ => return None,
    })
}

/// HTML's numeric rules: NUL, surrogates and values past U+10FFFF become U+FFFD, and 0x80–0x9F
/// are read as Windows-1252, because old pages write `&#150;` meaning "–".
fn numeric_ref(n: u32) -> char {
    const CP1252: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž',
        '\u{8F}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}',
        'ž', 'Ÿ',
    ];
    match n {
        0 => '\u{FFFD}',
        0x80..=0x9F => CP1252[(n - 0x80) as usize],
        _ => char::from_u32(n).unwrap_or('\u{FFFD}'),
    }
}
