use std::{
    collections::{BTreeSet, HashSet},
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use mccrawl::store::{
    LinkOut, PageRecord, Store, StoreMsg, WarcPayload, WarcWriter, iso8601, spawn, warc_record,
};
use serde_json::Value;
use url::Url;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mccrawl-store-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn mkdir(name: &str) -> PathBuf {
    let d = tmp(name);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn payload(body: &[u8], truncated: bool) -> WarcPayload {
    let http_head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\n\r\n",
        body.len()
    );
    WarcPayload {
        http_head: http_head.into_bytes(),
        body: body.to_vec(),
        truncated,
    }
}

fn msg(url: &str, simhash: Option<u64>, body_hash: Option<u64>) -> StoreMsg {
    let record = PageRecord {
        url: url.into(),
        ..Default::default()
    };
    StoreMsg {
        record,
        simhash,
        body_hash,
        warc: None,
    }
}

fn rows(dir: &Path) -> Vec<Value> {
    let jsonl = std::fs::read_to_string(dir.join("pages.jsonl")).unwrap();
    jsonl
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn warc_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".warc.gz"))
        .collect();
    names.sort();
    names
}

/// One WARC record, as read back by `read_warc`.
struct Rec {
    headers: Vec<(String, String)>,
    block: Vec<u8>,
    /// Compressed size of the gzip member that held it.
    gz_len: usize,
}

impl Rec {
    fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn names(&self) -> Vec<&str> {
        self.headers.iter().map(|(k, _)| k.as_str()).collect()
    }
}

/// Decodes a `.warc.gz` one gzip member at a time and parses each member as exactly one record,
/// so a member holding two records, or a record split across two members, fails.
fn read_warc(path: &Path) -> Vec<Rec> {
    let data = std::fs::read(path).unwrap();
    let mut rest = data.as_slice();
    let mut recs = Vec::new();
    while !rest.is_empty() {
        let before = rest.len();
        let mut d = flate2::bufread::GzDecoder::new(rest);
        let mut member = Vec::new();
        d.read_to_end(&mut member).unwrap();
        rest = d.into_inner();
        assert!(rest.len() < before, "empty gzip member");
        let mut rec = parse_record(&member);
        rec.gz_len = before - rest.len();
        recs.push(rec);
    }
    recs
}

fn parse_record(m: &[u8]) -> Rec {
    let end = m
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("no end of the header");
    let mut lines = std::str::from_utf8(&m[..end]).unwrap().split("\r\n");
    assert_eq!(lines.next(), Some("WARC/1.1"));
    let headers = lines
        .map(|l| {
            let (k, v) = l.split_once(": ").expect(l);
            (k.to_owned(), v.to_owned())
        })
        .collect();
    let rec = Rec {
        headers,
        block: Vec::new(),
        gz_len: 0,
    };
    let len: usize = rec.get("Content-Length").unwrap().parse().unwrap();
    let rest = &m[end + 4..];
    assert_eq!(
        rest.len(),
        len + 4,
        "Content-Length disagrees with the record"
    );
    assert!(
        rest.ends_with(b"\r\n\r\n"),
        "record does not end with CRLF CRLF"
    );
    Rec {
        block: rest[..len].to_vec(),
        ..rec
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

// ---- dates ----

#[test]
fn iso8601_known_dates() {
    assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
    assert_eq!(iso8601(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(iso8601(1_763_424_000 + 3661), "2025-11-18T01:01:01Z");
    // Reference values from Python's datetime.
    assert_eq!(iso8601(946_684_799), "1999-12-31T23:59:59Z");
    assert_eq!(iso8601(1_709_251_199), "2024-02-29T23:59:59Z");
    assert_eq!(iso8601(4_107_542_399), "2100-02-28T23:59:59Z");
    assert_eq!(iso8601(4_107_542_400), "2100-03-01T00:00:00Z"); // 2100 is not a leap year
    assert_eq!(iso8601(13_574_606_400), "2400-02-29T12:00:00Z"); // 2400 is
    assert_eq!(iso8601(253_402_300_799), "9999-12-31T23:59:59Z");
}

#[test]
fn iso8601_agrees_with_a_day_by_day_calendar_for_450_years() {
    let (mut y, mut m, mut d) = (1970u64, 1, 1);
    let mut day = 0u64;
    while y < 2420 {
        let t = day * 7919 % 86_400; // a different second of the day each day; covers all of them
        let want = format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
            t / 3600,
            t / 60 % 60,
            t % 60
        );
        assert_eq!(iso8601(day * 86_400 + t), want, "day {day}");
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let len = [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ][m - 1];
        d += 1;
        if d > len {
            (d, m) = (1, m + 1);
            if m > 12 {
                (m, y) = (1, y + 1);
            }
        }
        day += 1;
    }
}

#[test]
fn iso8601_never_panics() {
    let s = iso8601(u64::MAX);
    assert!(s.ends_with('Z') && s.len() > 20, "{s}");
}

// ---- record framing ----

#[test]
fn warc_record_framing() {
    let rec = warc_record(
        "response",
        Some("http://a.com/"),
        "2025-11-18T00:00:00Z",
        "application/http;msgtype=response",
        &[],
        b"HTTP/1.1 200 OK\r\n\r\nhi",
        "<urn:uuid:x>",
    );
    let s = String::from_utf8(rec).unwrap();
    assert!(s.starts_with("WARC/1.1\r\nWARC-Type: response\r\n"));
    assert!(
        s.contains("WARC-Target-URI: http://a.com/\r\n") && s.contains("Content-Length: 21\r\n")
    );
    assert!(s.ends_with("\r\n\r\nHTTP/1.1 200 OK\r\n\r\nhi\r\n\r\n"));
}

#[test]
fn warc_record_exact_bytes() {
    let rec = warc_record(
        "response",
        Some("http://a.com/"),
        "2025-11-18T00:00:00Z",
        "application/http;msgtype=response",
        &[("WARC-Truncated", "length".into()), ("X-Two", "2".into())],
        b"HTTP/1.1 200 OK\r\n\r\nhi",
        "<urn:uuid:x>",
    );
    assert_eq!(
        String::from_utf8(rec).unwrap(),
        "WARC/1.1\r\nWARC-Type: response\r\nWARC-Record-ID: <urn:uuid:x>\r\n\
         WARC-Date: 2025-11-18T00:00:00Z\r\nWARC-Target-URI: http://a.com/\r\n\
         WARC-Truncated: length\r\nX-Two: 2\r\nContent-Type: application/http;msgtype=response\r\n\
         Content-Length: 21\r\n\r\nHTTP/1.1 200 OK\r\n\r\nhi\r\n\r\n"
    );
    // No target, empty payload.
    let info = warc_record(
        "warcinfo",
        None,
        "D",
        "application/warc-fields",
        &[],
        b"",
        "<id>",
    );
    assert_eq!(
        String::from_utf8(info).unwrap(),
        "WARC/1.1\r\nWARC-Type: warcinfo\r\nWARC-Record-ID: <id>\r\nWARC-Date: D\r\n\
         Content-Type: application/warc-fields\r\nContent-Length: 0\r\n\r\n\r\n\r\n"
    );
    // Binary payloads are kept byte for byte, and Content-Length counts bytes, not characters.
    let bin: Vec<u8> = (0..=255u8).chain("é\r\n\r\nWARC/1.1".bytes()).collect();
    let rec = warc_record("resource", None, "D", "x", &[], &bin, "<id>");
    assert!(rec.ends_with(&[bin.as_slice(), b"\r\n\r\n"].concat()));
    let len = format!("Content-Length: {}\r\n\r\n", bin.len());
    assert!(rec.windows(len.len()).any(|w| w == len.as_bytes()));
}

// ---- WarcWriter ----

#[test]
fn rotates_files_and_each_record_is_a_gzip_member() {
    let dir = mkdir("warc");
    let mut w = WarcWriter::create(&dir, 1).unwrap(); // rotate after every record
    let p = WarcPayload {
        http_head: b"HTTP/1.1 200 OK\r\n\r\n".to_vec(),
        body: b"x".to_vec(),
        truncated: false,
    };
    w.write_response("http://a.com/", 0, &p).unwrap();
    w.write_response("http://a.com/2", 0, &p).unwrap();
    w.flush().unwrap();
    let mut s = String::new();
    flate2::read::MultiGzDecoder::new(
        std::fs::File::open(dir.join("crawl-00000.warc.gz")).unwrap(),
    )
    .read_to_string(&mut s)
    .unwrap();
    assert!(s.starts_with("WARC/1.1\r\nWARC-Type: warcinfo"));

    let names = warc_files(&dir);
    assert_eq!(names, ["crawl-00000.warc.gz", "crawl-00001.warc.gz"]);
    for (name, url) in names.iter().zip(["http://a.com/", "http://a.com/2"]) {
        let recs = read_warc(&dir.join(name));
        assert_eq!(recs.len(), 2, "{name}");
        assert_eq!(recs[0].get("WARC-Type"), Some("warcinfo"));
        assert_eq!(recs[0].get("WARC-Filename"), Some(name.as_str()));
        assert_eq!(recs[1].get("WARC-Type"), Some("response"));
        assert_eq!(recs[1].get("WARC-Target-URI"), Some(url));
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn warcinfo_describes_the_file() {
    let dir = mkdir("info");
    let before = iso8601(now_secs());
    let mut w = WarcWriter::create(&dir, 1 << 30).unwrap();
    w.flush().unwrap();
    let after = iso8601(now_secs());
    let recs = read_warc(&dir.join("crawl-00000.warc.gz"));
    assert_eq!(
        recs.len(),
        1,
        "a writer with nothing archived leaves just the warcinfo"
    );
    let info = &recs[0];
    assert_eq!(
        info.names(),
        [
            "WARC-Type",
            "WARC-Record-ID",
            "WARC-Date",
            "WARC-Filename",
            "Content-Type",
            "Content-Length"
        ]
    );
    assert_eq!(info.get("WARC-Type"), Some("warcinfo"));
    assert_eq!(info.get("WARC-Filename"), Some("crawl-00000.warc.gz"));
    assert_eq!(info.get("Content-Type"), Some("application/warc-fields"));
    let date = info.get("WARC-Date").unwrap();
    assert!(before.as_str() <= date && date <= after.as_str(), "{date}");
    let fields = String::from_utf8(info.block.clone()).unwrap();
    assert!(fields.starts_with("software: McCrawl/0.1\r\n"), "{fields}");
    assert!(
        fields.contains("format: WARC File Format 1.1\r\n"),
        "{fields}"
    );
    assert!(fields.ends_with("\r\n") && fields.split_terminator("\r\n").all(|l| l.contains(": ")));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn one_file_until_full_and_never_a_trailing_file_with_only_warcinfo() {
    let dir = mkdir("one");
    let mut w = WarcWriter::create(&dir, 1 << 30).unwrap();
    for i in 0..50 {
        w.write_response(&format!("http://a.com/{i}"), 0, &payload(b"x", false))
            .unwrap();
    }
    w.flush().unwrap();
    assert_eq!(warc_files(&dir), ["crawl-00000.warc.gz"]);
    assert_eq!(read_warc(&dir.join("crawl-00000.warc.gz")).len(), 51);

    let dir2 = mkdir("each");
    let mut w = WarcWriter::create(&dir2, 0).unwrap();
    for i in 0..3 {
        w.write_response(&format!("http://a.com/{i}"), 0, &payload(b"x", false))
            .unwrap();
    }
    w.flush().unwrap();
    let names = warc_files(&dir2);
    assert_eq!(names.len(), 3, "{names:?}");
    for name in &names {
        let types: Vec<_> = read_warc(&dir2.join(name))
            .iter()
            .map(|r| r.get("WARC-Type").unwrap().to_owned())
            .collect();
        assert_eq!(types, ["warcinfo", "response"], "{name}");
    }
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&dir2).ok();
}

#[test]
fn rotation_starts_a_new_file_as_soon_as_the_size_is_reached() {
    let dir = mkdir("size");
    let rotate_at = 20_000;
    let mut w = WarcWriter::create(&dir, rotate_at).unwrap();
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    for i in 0..60 {
        // Incompressible bodies of varying size, so files fill up at different points.
        let len = 500 + (i * 997) % 4000;
        let body: Vec<u8> = (0..len)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng as u8
            })
            .collect();
        w.write_response(&format!("http://a.com/{i}"), 0, &payload(&body, false))
            .unwrap();
    }
    w.flush().unwrap();
    let names = warc_files(&dir);
    assert!(names.len() >= 4, "{names:?}");
    let mut urls = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let recs = read_warc(&dir.join(name));
        let size: usize = recs.iter().map(|r| r.gz_len).sum();
        assert_eq!(
            size as u64,
            std::fs::metadata(dir.join(name)).unwrap().len()
        );
        assert_eq!(recs[0].get("WARC-Type"), Some("warcinfo"));
        assert!(
            recs.len() >= 2
                && recs[1..]
                    .iter()
                    .all(|r| r.get("WARC-Type") == Some("response"))
        );
        if i + 1 < names.len() {
            // Full, and only its last record took it over the limit.
            assert!(size as u64 >= rotate_at, "{name}: {size} bytes");
            let without_last = size - recs.last().unwrap().gz_len;
            assert!((without_last as u64) < rotate_at, "{name}: rotated late");
        }
        urls.extend(
            recs[1..]
                .iter()
                .map(|r| r.get("WARC-Target-URI").unwrap().to_owned()),
        );
    }
    let want: Vec<String> = (0..60).map(|i| format!("http://a.com/{i}")).collect();
    assert_eq!(urls, want, "every record once, in order");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn responses_hold_the_exact_payload_and_mark_truncation() {
    let dir = mkdir("payload");
    let mut w = WarcWriter::create(&dir, 1 << 30).unwrap();
    let body: Vec<u8> = (0..=255u8).chain(*b"\r\n\r\nWARC/1.1\r\n").collect();
    w.write_response(
        "http://a.com/bin",
        1_763_424_000 + 3661,
        &payload(&body, true),
    )
    .unwrap();
    w.write_response("http://a.com/empty", 0, &payload(b"", false))
        .unwrap();
    w.flush().unwrap();
    let recs = read_warc(&dir.join("crawl-00000.warc.gz"));
    let (bin, empty) = (&recs[1], &recs[2]);
    assert_eq!(
        bin.names(),
        [
            "WARC-Type",
            "WARC-Record-ID",
            "WARC-Date",
            "WARC-Target-URI",
            "WARC-Truncated",
            "Content-Type",
            "Content-Length"
        ]
    );
    assert_eq!(bin.block, [payload(&body, true).http_head, body].concat());
    assert_eq!(bin.get("WARC-Truncated"), Some("length"));
    assert_eq!(
        bin.get("Content-Type"),
        Some("application/http;msgtype=response")
    );
    assert_eq!(bin.get("WARC-Date"), Some("2025-11-18T01:01:01Z"));
    assert_eq!(empty.block, payload(b"", false).http_head);
    assert_eq!(empty.get("WARC-Truncated"), None);
    assert_eq!(empty.get("WARC-Date"), Some("1970-01-01T00:00:00Z"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn record_ids_are_unique_well_formed_uuids() {
    let (a, b) = (mkdir("ids-a"), mkdir("ids-b"));
    for dir in [&a, &b] {
        let mut w = WarcWriter::create(dir, 1 << 30).unwrap();
        for i in 0..300 {
            w.write_response(&format!("http://a.com/{i}"), 0, &payload(b"x", false))
                .unwrap();
        }
        w.flush().unwrap();
    }
    let mut ids = HashSet::new();
    let mut n = 0;
    for dir in [&a, &b] {
        for rec in read_warc(&dir.join("crawl-00000.warc.gz")) {
            let id = rec.get("WARC-Record-ID").unwrap();
            let uuid = id
                .strip_prefix("<urn:uuid:")
                .and_then(|s| s.strip_suffix('>'));
            let uuid = uuid.unwrap_or_else(|| panic!("{id}"));
            let parts: Vec<&str> = uuid.split('-').collect();
            assert_eq!(
                parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
                [8, 4, 4, 4, 12],
                "{id}"
            );
            assert!(
                uuid.chars()
                    .all(|c| c == '-' || c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{id}"
            );
            assert!(parts[2].starts_with('8'), "not version 8: {id}");
            assert!(
                parts[3].starts_with(['8', '9', 'a', 'b']),
                "not the RFC variant: {id}"
            );
            ids.insert(id.to_owned());
            n += 1;
        }
    }
    assert_eq!((ids.len(), n), (602, 602));
    std::fs::remove_dir_all(&a).ok();
    std::fs::remove_dir_all(&b).ok();
}

#[test]
fn hostile_urls_cannot_break_the_header_lines() {
    let dir = mkdir("hostile");
    let mut w = WarcWriter::create(&dir, 1 << 30).unwrap();
    // The url crate drops CR/LF/tab and percent-encodes the rest, so a serialized Url is always
    // one line of visible ASCII; this checks that holds all the way into the file.
    let url = Url::parse("http://a.com/a b\r\n\tWARC-Type: evil/ü?q=\"<x>\"").unwrap();
    w.write_response(url.as_str(), 0, &payload(b"x", false))
        .unwrap();
    w.flush().unwrap();
    let recs = read_warc(&dir.join("crawl-00000.warc.gz"));
    assert_eq!(recs.len(), 2);
    let target = recs[1].get("WARC-Target-URI").unwrap();
    assert_eq!(target, url.as_str());
    assert!(target.bytes().all(|b| b.is_ascii_graphic()), "{target}");
    assert_eq!(
        recs[1]
            .names()
            .iter()
            .filter(|&&k| k == "WARC-Type")
            .count(),
        1
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn never_overwrites_an_earlier_crawl() {
    let dir = mkdir("exists");
    std::fs::write(dir.join("crawl-00000.warc.gz"), "old").unwrap();
    let err = WarcWriter::create(&dir, 1).err().unwrap();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read_to_string(dir.join("crawl-00000.warc.gz")).unwrap(),
        "old"
    );

    let dir2 = mkdir("exists-jsonl");
    std::fs::write(dir2.join("pages.jsonl"), "old").unwrap();
    let err = Store::open(&dir2, 1).err().unwrap();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read_to_string(dir2.join("pages.jsonl")).unwrap(),
        "old"
    );

    // A rotation onto a leftover file fails instead of replacing it, and the current file stays whole.
    let dir3 = mkdir("exists-next");
    std::fs::write(dir3.join("crawl-00001.warc.gz"), "old").unwrap();
    let mut w = WarcWriter::create(&dir3, 1).unwrap();
    w.write_response("http://a.com/1", 0, &payload(b"x", false))
        .unwrap();
    let err = w
        .write_response("http://a.com/2", 0, &payload(b"x", false))
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read_to_string(dir3.join("crawl-00001.warc.gz")).unwrap(),
        "old"
    );
    w.flush().unwrap();
    assert_eq!(read_warc(&dir3.join("crawl-00000.warc.gz")).len(), 2);

    // The output path is a file, not a directory.
    let file = tmp("not-a-dir");
    std::fs::write(&file, "x").unwrap();
    assert!(Store::open(&file, 1).is_err());

    for d in [&dir, &dir2, &dir3] {
        std::fs::remove_dir_all(d).ok();
    }
    std::fs::remove_file(&file).ok();
}

#[test]
fn open_creates_missing_directories() {
    let root = tmp("nested");
    let dir = root.join("a/b/c");
    let sum = Store::open(&dir, 1 << 30).unwrap().finish().unwrap();
    assert_eq!((sum.records, sum.warc_records, sum.warc_files), (0, 0, 1));
    assert_eq!(
        std::fs::read_to_string(dir.join("pages.jsonl")).unwrap(),
        ""
    );
    assert_eq!(read_warc(&dir.join("crawl-00000.warc.gz")).len(), 1);
    std::fs::remove_dir_all(&root).ok();
}

// ---- Store: duplicates and pages.jsonl ----

#[test]
fn marks_near_duplicates_and_writes_jsonl() {
    let dir = tmp("store");
    let mut st = Store::open(&dir, 1 << 30).unwrap();
    for u in ["http://a.com/1", "http://b.com/1"] {
        st.write(StoreMsg {
            record: PageRecord {
                url: u.into(),
                ..Default::default()
            },
            simhash: Some(0xABCD),
            body_hash: Some(1),
            warc: None,
        })
        .unwrap();
    }
    let sum = st.finish().unwrap();
    assert_eq!((sum.records, sum.near_dups, sum.exact_dups), (2, 1, 1));
    let jsonl = std::fs::read_to_string(dir.join("pages.jsonl")).unwrap();
    let second: Value = serde_json::from_str(jsonl.lines().nth(1).unwrap()).unwrap();
    assert_eq!(second["near_dup"], "http://a.com/1");
    assert_eq!(second["simhash"], "000000000000abcd");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn near_dup_points_at_the_earliest_page_within_three_bits() {
    let dir = tmp("near");
    let mut st = Store::open(&dir, 1 << 30).unwrap();
    let fp = 0xDEAD_BEEF_1234_5678u64;
    let pages = [
        ("a", fp),
        ("b", fp ^ 0b111),                       // 3 bits from a
        ("c", fp),                               // same as a; also 3 bits from b
        ("d", fp ^ (0xF << 32)),                 // 4 bits from a: not a near-duplicate
        ("e", !fp),                              // far from everything
        ("f", fp ^ (1 | (1 << 20) | (1 << 40))), // 3 bits from a, in 3 different blocks
        ("g", fp ^ 0b111_111), // 3 bits from b, 6 from a: b must have been indexed too
    ];
    for (name, sim) in pages {
        st.write(msg(&format!("http://x.com/{name}"), Some(sim), None))
            .unwrap();
    }
    let sum = st.finish().unwrap();
    let near: Vec<Value> = rows(&dir).iter().map(|r| r["near_dup"].clone()).collect();
    let (a, b) = (Value::from("http://x.com/a"), Value::from("http://x.com/b"));
    let null = Value::Null;
    let want = [null.clone(), a.clone(), a.clone(), null.clone(), null, a, b];
    assert_eq!(near, want);
    assert_eq!((sum.near_dups, sum.exact_dups), (4, 0));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn empty_text_and_missing_hashes_are_not_indexed() {
    let dir = tmp("empty");
    let mut st = Store::open(&dir, 1 << 30).unwrap();
    st.write(msg("http://x.com/empty1", Some(0), Some(0)))
        .unwrap();
    st.write(msg("http://x.com/empty2", Some(0), Some(0)))
        .unwrap();
    st.write(msg("http://x.com/err1", None, None)).unwrap();
    st.write(msg("http://x.com/err2", None, None)).unwrap();
    let sum = st.finish().unwrap();
    // Two pages without text are not near-duplicates; their equal bodies are still exact ones
    // (0 is an ordinary body hash, unlike a SimHash of 0).
    assert_eq!((sum.records, sum.near_dups, sum.exact_dups), (4, 0, 1));
    let r = rows(&dir);
    assert_eq!(r[1]["simhash"], "0000000000000000");
    assert_eq!(r[1]["body_hash"], "0000000000000000");
    assert_eq!(r[1]["near_dup"], Value::Null);
    for row in &r[2..] {
        assert_eq!(
            (&row["simhash"], &row["body_hash"], &row["near_dup"]),
            (&Value::Null, &Value::Null, &Value::Null)
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn exact_and_near_duplicates_are_counted_separately() {
    let dir = tmp("counts");
    let mut st = Store::open(&dir, 1 << 30).unwrap();
    let fp = 0x1111_2222_3333_4444u64;
    st.write(msg("http://x.com/a", Some(fp), Some(7))).unwrap();
    st.write(msg("http://x.com/b", Some(!fp), Some(7))).unwrap(); // same body, far SimHash
    st.write(msg("http://x.com/c", Some(fp), Some(8))).unwrap(); // other body, same SimHash
    let sum = st.finish().unwrap();
    assert_eq!((sum.records, sum.exact_dups, sum.near_dups), (3, 1, 1));
    let r = rows(&dir);
    assert_eq!(
        (&r[1]["near_dup"], &r[2]["near_dup"]),
        (&Value::Null, &Value::from("http://x.com/a"))
    );
    assert_eq!(r[0]["body_hash"], "0000000000000007");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_store_owns_the_hash_and_near_dup_fields() {
    let dir = tmp("owns");
    let mut st = Store::open(&dir, 1 << 30).unwrap();
    let mut m = msg("http://x.com/a", None, None);
    m.record.simhash = Some("junk".into());
    m.record.body_hash = Some("junk".into());
    m.record.near_dup = Some("junk".into());
    st.write(m).unwrap();
    st.finish().unwrap();
    let r = &rows(&dir)[0];
    assert_eq!(
        (&r["simhash"], &r["body_hash"], &r["near_dup"]),
        (&Value::Null, &Value::Null, &Value::Null)
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn rows_are_one_line_each_and_round_trip() {
    let dir = tmp("rows");
    let mut st = Store::open(&dir, 1 << 30).unwrap();
    let nasty = "line\nbreak\r\n \"quoted\" \\ back\u{0}nul\u{1f} tab\t ü 😀 \u{2028}";
    let record = PageRecord {
        url: "http://x.com/a?q=1".into(),
        host: "x.com".into(),
        depth: u16::MAX,
        parent_url: Some("http://x.com/".into()),
        priority: -7,
        fetched_at: u64::MAX,
        status: Some(599),
        error: None,
        content_type: Some("text/html; charset=utf-8".into()),
        bytes: 2 << 20,
        truncated: true,
        title: nasty.into(),
        text: nasty.repeat(100),
        canonical: Some("http://x.com/c".into()),
        noindex: true,
        links: vec![
            LinkOut {
                url: "http://y.com/".into(),
                anchor: nasty.into(),
                nofollow: true,
            },
            LinkOut {
                url: "http://z.com/".into(),
                anchor: String::new(),
                nofollow: false,
            },
        ],
        ..Default::default()
    };
    st.write(StoreMsg {
        record,
        simhash: Some(u64::MAX),
        body_hash: Some(1),
        warc: None,
    })
    .unwrap();
    let error = PageRecord {
        url: "http://x.com/err".into(),
        error: Some("timeout".into()),
        ..Default::default()
    };
    st.write(StoreMsg {
        record: error,
        simhash: None,
        body_hash: None,
        warc: None,
    })
    .unwrap();
    st.finish().unwrap();

    let raw = std::fs::read_to_string(dir.join("pages.jsonl")).unwrap();
    assert_eq!(
        raw.split('\n').collect::<Vec<_>>().len(),
        3,
        "a newline inside a row"
    );
    assert!(raw.ends_with('\n') && raw.starts_with("{\"url\":"));
    let r = rows(&dir);
    let keys: BTreeSet<&str> = r[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let want = [
        "body_hash",
        "bytes",
        "canonical",
        "content_type",
        "depth",
        "error",
        "fetched_at",
        "host",
        "links",
        "near_dup",
        "noindex",
        "parent_url",
        "priority",
        "simhash",
        "status",
        "text",
        "title",
        "truncated",
        "url",
    ];
    assert_eq!(keys, BTreeSet::from(want));
    let a = &r[0];
    assert_eq!(
        (a["title"].as_str(), a["links"][0]["anchor"].as_str()),
        (Some(nasty), Some(nasty))
    );
    assert_eq!(a["text"].as_str().unwrap(), nasty.repeat(100));
    assert_eq!(
        (a["depth"].as_u64(), a["priority"].as_i64()),
        (Some(u16::MAX.into()), Some(-7))
    );
    assert_eq!(
        (a["fetched_at"].as_u64(), a["bytes"].as_u64()),
        (Some(u64::MAX), Some(2 << 20))
    );
    assert_eq!(
        (a["status"].as_u64(), &a["error"]),
        (Some(599), &Value::Null)
    );
    assert_eq!(
        (&a["truncated"], &a["noindex"]),
        (&Value::Bool(true), &Value::Bool(true))
    );
    assert_eq!(
        (&a["parent_url"], &a["canonical"]),
        (
            &Value::from("http://x.com/"),
            &Value::from("http://x.com/c")
        )
    );
    assert_eq!(a["content_type"], "text/html; charset=utf-8");
    assert_eq!(
        (&a["links"][0]["nofollow"], &a["links"][1]["anchor"]),
        (&Value::Bool(true), &Value::from(""))
    );
    assert_eq!(
        (&a["simhash"], &a["body_hash"]),
        (
            &Value::from("ffffffffffffffff"),
            &Value::from("0000000000000001")
        )
    );
    let e = &r[1];
    assert_eq!(
        (&e["error"], &e["status"], &e["links"]),
        (&Value::from("timeout"), &Value::Null, &Value::Array(vec![]))
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_store_archives_pages_that_have_a_payload() {
    let dir = tmp("archive");
    let mut st = Store::open(&dir, 1).unwrap(); // a file per response
    for (i, archived) in [true, false, true, true].into_iter().enumerate() {
        let mut m = msg(&format!("http://x.com/{i}"), None, None);
        m.record.fetched_at = 1_763_424_000_999 + i as u64 * 1000; // ms; the WARC date rounds down
        if archived {
            m.warc = Some(payload(format!("page {i}").as_bytes(), i == 3));
        }
        st.write(m).unwrap();
    }
    let sum = st.finish().unwrap();
    assert_eq!((sum.records, sum.warc_records, sum.warc_files), (4, 3, 3));
    let responses: Vec<Rec> = warc_files(&dir)
        .iter()
        .flat_map(|n| read_warc(&dir.join(n)))
        .filter(|r| r.get("WARC-Type") == Some("response"))
        .collect();
    let got: Vec<_> = responses
        .iter()
        .map(|r| {
            (
                r.get("WARC-Target-URI").unwrap(),
                r.get("WARC-Date").unwrap(),
                r.get("WARC-Truncated"),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("http://x.com/0", "2025-11-18T00:00:00Z", None),
            ("http://x.com/2", "2025-11-18T00:00:02Z", None),
            ("http://x.com/3", "2025-11-18T00:00:03Z", Some("length")),
        ]
    );
    assert_eq!(
        responses[1].block,
        [payload(b"page 2", false).http_head, b"page 2".to_vec()].concat()
    );
    assert_eq!(rows(&dir).len(), 4);
    std::fs::remove_dir_all(&dir).ok();
}

// ---- the store thread ----

#[test]
fn the_store_thread_keeps_each_producers_order_and_returns_the_summary() {
    let dir = tmp("thread");
    let (tx, rx) = tokio::sync::mpsc::channel(4); // small, so producers block on it
    let handle = spawn(Store::open(&dir, 1 << 30).unwrap(), rx);
    let producers: Vec<_> = (0..4)
        .map(|p| {
            let tx = tx.clone();
            thread::spawn(move || {
                for i in 0..250 {
                    tx.blocking_send(msg(&format!("http://h{p}.com/{i}"), None, None))
                        .unwrap();
                }
            })
        })
        .collect();
    drop(tx);
    for p in producers {
        p.join().unwrap();
    }
    let sum = handle.join().unwrap().unwrap();
    assert_eq!(sum.records, 1000);
    let mut next = [0usize; 4];
    for row in rows(&dir) {
        let url = row["url"].as_str().unwrap();
        let (p, i) = url
            .strip_prefix("http://h")
            .unwrap()
            .split_once(".com/")
            .unwrap();
        let (p, i): (usize, usize) = (p.parse().unwrap(), i.parse().unwrap());
        assert_eq!(i, next[p], "out of order: {url}");
        next[p] += 1;
    }
    assert_eq!(next, [250; 4]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_store_thread_flushes_while_the_channel_is_idle() {
    let dir = tmp("idle");
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let handle = spawn(Store::open(&dir, 1 << 30).unwrap(), rx);
    let mut m = msg("http://x.com/1", None, None);
    m.warc = Some(payload(b"hi", false));
    tx.blocking_send(m).unwrap();
    // Both files buffer 64 KB, so only the idle flush can put this one small page on disk.
    let warc = dir.join("crawl-00000.warc.gz");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let rows_on_disk = std::fs::read_to_string(dir.join("pages.jsonl"))
            .unwrap()
            .lines()
            .count();
        let warc_on_disk =
            std::fs::metadata(&warc).unwrap().len() > 0 && read_warc(&warc).len() == 2;
        if rows_on_disk == 1 && warc_on_disk {
            break;
        }
        assert!(Instant::now() < deadline, "not flushed while idle");
        thread::sleep(Duration::from_millis(10));
    }
    drop(tx);
    let sum = handle.join().unwrap().unwrap();
    assert_eq!((sum.records, sum.warc_records), (1, 1));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_store_thread_finishes_cleanly_with_no_messages() {
    let dir = tmp("nothing");
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let handle = spawn(Store::open(&dir, 1 << 30).unwrap(), rx);
    drop(tx);
    let sum = handle.join().unwrap().unwrap();
    assert_eq!(
        (
            sum.records,
            sum.warc_records,
            sum.warc_files,
            sum.exact_dups,
            sum.near_dups
        ),
        (0, 0, 1, 0, 0)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("pages.jsonl")).unwrap(),
        ""
    );
    assert_eq!(read_warc(&dir.join("crawl-00000.warc.gz")).len(), 1);
    std::fs::remove_dir_all(&dir).ok();
}
