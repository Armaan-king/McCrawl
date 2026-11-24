use flate2::{Compression, write::GzEncoder};
use mccrawl::fetch::{FetchConfig, FetchError, FetchResponse, Fetcher, parse_retry_after};
use mccrawl::mockweb::{self, MockConfig, MockWeb};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use url::Url;

fn cfg(resolve: Vec<(String, SocketAddr)>) -> FetchConfig {
    FetchConfig {
        user_agent: "McCrawl/0.1 (+test)".into(),
        connect_timeout: Duration::from_secs(2),
        timeout: Duration::from_secs(5),
        max_body: 2 << 20,
        resolve,
    }
}

fn fetcher() -> Fetcher {
    Fetcher::new(&cfg(vec![])).unwrap()
}

fn head(r: &FetchResponse) -> String {
    String::from_utf8_lossy(&r.http_head).into_owned()
}

// ---------- against the mock web ----------

fn mock_cfg(latency: u64) -> MockConfig {
    MockConfig {
        hosts: 10,
        latency_ms: (latency, latency),
        ..Default::default()
    }
}

async fn mock(latency: u64) -> (MockWeb, Fetcher) {
    let web = mockweb::start(mock_cfg(latency), 0).await.unwrap();
    let f = Fetcher::new(&cfg(web.resolve_overrides())).unwrap();
    (web, f)
}

fn mock_page(web: &MockWeb, host: u32, page: u32) -> Vec<u8> {
    mockweb::page_html(&mock_cfg(0), web.addr.port(), host, page).into_bytes()
}

fn mock_url(web: &MockWeb, host: u32, path: &str) -> Url {
    let name = mockweb::host_name(host);
    Url::parse(&format!("http://{name}:{}{path}", web.addr.port())).unwrap()
}

#[tokio::test]
async fn classifies_mock_responses() {
    let (web, f) = mock(0).await;
    let u = |h: u32, p: &str| mock_url(&web, h, p);

    let r = f.fetch_page(&u(4, "/p/1")).await.unwrap();
    assert_eq!(r.status, 200);
    assert!(r.is_html && !r.body.is_empty() && !r.truncated);
    assert_eq!(r.content_type.as_deref(), Some("text/html; charset=utf-8"));
    assert!(r.body == mock_page(&web, 4, 1));
    let h = head(&r);
    assert!(h.starts_with("HTTP/1.1 200 OK\r\n"), "{h}");
    assert!(!h.to_ascii_lowercase().contains("content-encoding"));
    assert!(
        h.contains(&format!("content-length: {}\r\n", r.body.len())),
        "{h}"
    );
    assert!(h.ends_with("\r\n\r\n"));
    assert_eq!((r.location, r.retry_after), (None, None));

    let r = f.fetch_page(&u(4, "/redirect")).await.unwrap();
    assert_eq!((r.status, r.location.as_deref()), (301, Some("/p/0")));
    assert!(head(&r).starts_with("HTTP/1.1 301 Moved Permanently\r\n"));

    let r = f.fetch_page(&u(4, "/file.pdf")).await.unwrap();
    assert!(!r.is_html && r.body.is_empty() && !r.truncated);
    assert_eq!(r.content_type.as_deref(), Some("application/pdf"));

    let r = f.fetch_page(&u(4, "/big")).await.unwrap();
    assert!(r.truncated && r.body.len() == 2 << 20);
    // The server said 3 MiB; only the stored length may appear.
    assert_eq!(head(&r).matches("content-length").count(), 1);
    assert!(head(&r).contains("content-length: 2097152\r\n"));

    let mut statuses = vec![];
    for i in 0..3 {
        let r = f.fetch_page(&u(3, &format!("/p/{i}"))).await.unwrap();
        let want = (r.status == 429).then_some(Duration::from_secs(2));
        assert_eq!(r.retry_after, want);
        statuses.push(r.status);
    }
    assert_eq!(statuses, [200, 200, 429]);

    assert_eq!(
        f.fetch_robots(&u(2, "/robots.txt")).await.unwrap().status,
        503
    );
    assert_eq!(
        f.fetch_robots(&u(4, "/robots.txt")).await.unwrap().status,
        404
    );
    let r = f.fetch_robots(&u(0, "/robots.txt")).await.unwrap();
    assert_eq!(r.status, 200);
    assert!(
        !r.is_html
            && String::from_utf8(r.body)
                .unwrap()
                .contains("Disallow: /private")
    );
}

#[tokio::test]
async fn pages_never_follow_redirects_but_robots_does() {
    let (web, f) = mock(0).await;
    let r = f.fetch_page(&mock_url(&web, 5, "/redirect")).await.unwrap();
    assert_eq!(r.status, 301);
    let r = f
        .fetch_robots(&mock_url(&web, 6, "/redirect"))
        .await
        .unwrap();
    assert_eq!(r.status, 200); // landed on /p/0
    let paths = |host: &str| -> Vec<String> {
        web.hits()
            .into_iter()
            .filter(|h| h.host == host)
            .map(|h| h.path)
            .collect()
    };
    assert_eq!(paths("h5.mock"), ["/redirect"]); // exactly one request, nothing followed
    assert_eq!(paths("h6.mock"), ["/redirect", "/p/0"]);
}

#[tokio::test]
async fn elapsed_includes_the_server_latency() {
    let (web, f) = mock(150).await;
    let r = f.fetch_page(&mock_url(&web, 4, "/p/2")).await.unwrap();
    assert!(r.elapsed >= Duration::from_millis(150), "{:?}", r.elapsed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_fetcher_serves_many_tasks_at_once() {
    let (web, f) = mock(5).await;
    let f = Arc::new(f);
    let mut tasks = vec![];
    for i in 0..40 {
        let (f, url) = (f.clone(), mock_url(&web, 4 + i % 6, &format!("/p/{i}")));
        tasks.push(tokio::spawn(
            async move { f.fetch_page(&url).await.unwrap() },
        ));
    }
    for (i, t) in tasks.into_iter().enumerate() {
        let r = t.await.unwrap();
        assert!(
            r.status == 200 && r.body == mock_page(&web, 4 + i as u32 % 6, i as u32),
            "{i}"
        );
    }
    assert_eq!(web.hits().len(), 40);
}

// ---------- against a hand-written server ----------

/// Answers the i-th connection with `replies[i]`, then closes it. Returns the base URL and,
/// once every reply is sent, the request heads it received (lowercased).
async fn serve(replies: Vec<Vec<u8>>) -> (Url, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move {
        let mut requests = vec![];
        for reply in replies {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = vec![];
            let mut buf = [0; 4096];
            while !req.ends_with(b"\r\n\r\n") {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            requests.push(String::from_utf8_lossy(&req).to_lowercase());
            let _ = sock.write_all(&reply).await; // the client may hang up early
            let _ = sock.shutdown().await;
        }
        requests
    });
    (url, task)
}

/// One connection: read once, send `parts` with `gap` between them, then hold it open for
/// `hold` before closing.
async fn trickle(parts: Vec<Vec<u8>>, gap: Duration, hold: Duration) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let _ = sock.read(&mut [0; 4096]).await;
        for (i, part) in parts.into_iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(gap).await;
            }
            let _ = sock.write_all(&part).await;
        }
        tokio::time::sleep(hold).await;
    });
    url
}

const HOLD: Duration = Duration::from_secs(10);

/// A complete response with `Connection: close` and a Content-Length, then the body.
fn resp(status: &str, headers: &[&str], body: &[u8]) -> Vec<u8> {
    let mut r = format!(
        "HTTP/1.1 {status}\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    for h in headers {
        r += h;
        r += "\r\n";
    }
    r += "\r\n";
    [r.as_bytes(), body].concat()
}

fn html(body: &[u8]) -> Vec<u8> {
    resp("200 OK", &["content-type: text/html"], body)
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// Chunked transfer encoding, one chunk per part.
fn chunked(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![];
    for p in parts {
        out.extend_from_slice(format!("{:x}\r\n", p.len()).as_bytes());
        out.extend_from_slice(p);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

async fn page_from(reply: Vec<u8>, max_body: usize) -> Result<FetchResponse, FetchError> {
    let (url, _) = serve(vec![reply]).await;
    Fetcher::new(&FetchConfig {
        max_body,
        ..cfg(vec![])
    })
    .unwrap()
    .fetch_page(&url)
    .await
}

#[tokio::test]
async fn sends_identity_accept_and_compression_headers() {
    let (url, reqs) = serve(vec![html(b"x"), resp("200 OK", &[], b"")]).await;
    let f = fetcher();
    f.fetch_page(&url).await.unwrap();
    f.fetch_robots(&url.join("/robots.txt").unwrap())
        .await
        .unwrap();
    let reqs = reqs.await.unwrap();
    for r in &reqs {
        assert!(r.contains("\r\nuser-agent: mccrawl/0.1 (+test)\r\n"), "{r}");
        let enc = r
            .split("\r\naccept-encoding: ")
            .nth(1)
            .unwrap()
            .split("\r\n")
            .next()
            .unwrap();
        for e in ["gzip", "br", "zstd"] {
            assert!(
                enc.split(',').any(|x| x.trim() == e),
                "{e} missing from {enc:?}"
            );
        }
    }
    assert!(reqs[0].starts_with("get / http/1.1\r\n"));
    assert!(reqs[0].contains("\r\naccept: text/html,application/xhtml+xml;q=0.9,*/*;q=0.8\r\n"));
    assert!(reqs[1].starts_with("get /robots.txt http/1.1\r\n"));
    assert!(reqs[1].contains("\r\naccept: text/plain,*/*;q=0.8\r\n"));
}

#[tokio::test]
async fn gzip_and_chunking_are_undone_and_the_head_matches_the_stored_body() {
    let page = b"<html><title>gz</title>hello hello hello</html>".repeat(50);
    let gz = gzip(&page);
    let (a, b) = gz.split_at(gz.len() / 2);
    let sent = "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: text/html\r\n\
                content-encoding: gzip\r\ntransfer-encoding: chunked\r\nx-test: 1\r\n\
                set-cookie: a=1\r\nset-cookie: b=2\r\n\r\n";
    let r = page_from([sent.as_bytes(), &chunked(&[a, b])].concat(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(r.body, page);
    assert!(r.is_html && !r.truncated);
    let h = head(&r);
    assert!(
        h.starts_with("HTTP/1.1 200 OK\r\n") && h.ends_with("\r\n\r\n"),
        "{h}"
    );
    assert!(
        !h.contains("content-encoding") && !h.contains("transfer-encoding"),
        "{h}"
    );
    assert_eq!(h.matches("content-length").count(), 1, "{h}");
    assert!(
        h.contains(&format!("\r\ncontent-length: {}\r\n", page.len())),
        "{h}"
    );
    for kept in [
        "\r\nx-test: 1\r\n",
        "\r\nset-cookie: a=1\r\n",
        "\r\nset-cookie: b=2\r\n",
    ] {
        assert!(h.contains(kept), "{kept:?} missing from {h}");
    }
}

#[tokio::test]
async fn an_encoding_not_decoded_stays_in_the_head() {
    // deflate isn't enabled (or advertised), so the bytes arrive as sent.
    let r = page_from(
        resp(
            "200 OK",
            &["content-type: text/html", "content-encoding: deflate"],
            b"raw",
        ),
        100,
    )
    .await
    .unwrap();
    assert_eq!(r.body, b"raw");
    assert!(head(&r).contains("\r\ncontent-encoding: deflate\r\n"));
}

#[tokio::test]
async fn corrupt_gzip_is_an_error_not_garbage() {
    let reply = resp(
        "200 OK",
        &["content-type: text/html", "content-encoding: gzip"],
        b"not gzip at all",
    );
    let e = page_from(reply, 1000).await.unwrap_err();
    assert_eq!(e.kind(), "other", "{e}");
    assert!(!e.retryable());
}

#[tokio::test]
async fn a_decompression_bomb_stops_at_the_cap() {
    let bomb = gzip(&vec![0; 64 << 20]); // 64 MiB of zeros, about 64 KiB on the wire
    assert!(bomb.len() < 200_000);
    let reply = resp(
        "200 OK",
        &["content-type: text/html", "content-encoding: gzip"],
        &bomb,
    );
    let r = page_from(reply, 1 << 20).await.unwrap();
    assert!(r.truncated);
    assert_eq!(r.body.len(), 1 << 20);
}

#[tokio::test]
async fn body_cap_boundaries() {
    let r = page_from(html(&[b'a'; 1000]), 1000).await.unwrap();
    assert!(
        !r.truncated && r.body.len() == 1000,
        "exactly the cap is not truncated"
    );
    let r = page_from(html(&[b'a'; 1001]), 1000).await.unwrap();
    assert!(r.truncated && r.body == [b'a'; 1000]);
    let h = head(&r);
    assert!(h.matches("content-length").count() == 1 && h.contains("content-length: 1000\r\n"));
    let r = page_from(html(b""), 1000).await.unwrap();
    assert!(!r.truncated && r.body.is_empty());
    assert!(head(&r).contains("\r\ncontent-length: 0\r\n"));
    let r = page_from(html(b"x"), 0).await.unwrap();
    assert!(r.truncated && r.body.is_empty());
    // The cap falls inside the second chunk, which is cut to fit.
    let body = chunked(&[&[b'a'; 600], &[b'b'; 600]]);
    let reply = [
        b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ntransfer-encoding: chunked\r\n\r\n",
        &body[..],
    ]
    .concat();
    let r = page_from(reply, 1000).await.unwrap();
    assert!(r.truncated);
    assert_eq!(r.body, [[b'a'; 600].as_slice(), &[b'b'; 400]].concat());
    assert!(head(&r).contains("\r\ncontent-length: 1000\r\n"));
}

#[tokio::test]
async fn a_huge_content_length_does_not_allocate_it() {
    // Claims 1 PB, sends 10 bytes, hangs up. Reserving the claim would abort the process.
    let reply = b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: 1000000000000000\r\n\r\n0123456789";
    let e = page_from(reply.to_vec(), 2 << 20).await.unwrap_err();
    assert_eq!(e.kind(), "other", "{e}");
}

#[tokio::test]
async fn content_type_decides_html() {
    let cases = [
        (Some("text/html"), true),
        (Some("TEXT/HTML; charset=ISO-8859-1"), true),
        (Some("text/html ; charset=utf-8"), true),
        (Some("application/xhtml+xml"), true),
        (Some("Application/XHTML+XML;charset=utf-8"), true),
        (Some("text/htmlx"), false),
        (Some("text/plain"), false),
        (Some("application/pdf"), false),
        (Some(""), false),
        (None, false),
    ];
    for (ct, want) in cases {
        let line = ct.map(|c| format!("content-type: {c}"));
        let headers: Vec<&str> = line.iter().map(String::as_str).collect();
        let r = page_from(resp("200 OK", &headers, b"<p>hi</p>"), 100)
            .await
            .unwrap();
        assert_eq!(r.is_html, want, "{ct:?}");
        assert_eq!(r.body.is_empty(), !want, "{ct:?}: body read only for HTML");
        assert_eq!(r.content_type.as_deref(), ct);
    }
    // robots.txt is read whatever the type.
    let (url, _) = serve(vec![resp(
        "200 OK",
        &["content-type: application/octet-stream"],
        b"User-agent: *",
    )])
    .await;
    let r = fetcher().fetch_robots(&url).await.unwrap();
    assert!(!r.is_html && r.body == b"User-agent: *");
}

#[tokio::test]
async fn odd_statuses_and_header_bytes_survive() {
    let r = page_from(
        resp("599 Whatever", &["content-type: text/html"], b"x"),
        100,
    )
    .await
    .unwrap();
    assert_eq!(r.status, 599);
    assert!(head(&r).starts_with("HTTP/1.1 599 \r\n"), "{}", head(&r));

    // Raw UTF-8 in Location, and a non-UTF-8 byte kept as is in the head.
    let reply = b"HTTP/1.1 301 Moved\r\nconnection: close\r\nlocation: /caf\xc3\xa9\r\nx-raw: caf\xe9\r\ncontent-length: 0\r\n\r\n";
    let r = page_from(reply.to_vec(), 100).await.unwrap();
    assert_eq!(r.location.as_deref(), Some("/café"));
    assert!(r.http_head.windows(13).any(|w| w == b"x-raw: caf\xe9\r\n"));

    let r = page_from(resp("503 Busy", &["retry-after:   7  "], b""), 100)
        .await
        .unwrap();
    assert_eq!(
        (r.status, r.retry_after),
        (503, Some(Duration::from_secs(7)))
    );
    let r = page_from(
        resp(
            "429 Slow",
            &["retry-after: Wed, 21 Oct 2015 07:28:00 GMT"],
            b"",
        ),
        100,
    )
    .await
    .unwrap();
    assert_eq!((r.status, r.retry_after), (429, None));
}

#[tokio::test]
async fn robots_follows_five_redirects_and_errors_past_that() {
    let hop = |i: u32| resp("301 Moved", &[&format!("location: /r{}", i + 1)], b"");
    let mut replies: Vec<_> = (0..5).map(hop).collect();
    replies.push(resp(
        "200 OK",
        &["content-type: text/plain"],
        b"User-agent: *\nDisallow: /x\n",
    ));
    let (url, reqs) = serve(replies).await;
    let r = fetcher()
        .fetch_robots(&url.join("/r0").unwrap())
        .await
        .unwrap();
    assert_eq!(
        (r.status, &r.body[..]),
        (200, &b"User-agent: *\nDisallow: /x\n"[..])
    );
    let paths: Vec<_> = reqs
        .await
        .unwrap()
        .iter()
        .map(|r| r.split(' ').nth(1).unwrap().to_owned())
        .collect();
    assert_eq!(paths, ["/r0", "/r1", "/r2", "/r3", "/r4", "/r5"]);

    let (url, reqs) = serve((0..6).map(hop).collect()).await;
    let e = fetcher()
        .fetch_robots(&url.join("/r0").unwrap())
        .await
        .unwrap_err();
    assert_eq!(e.kind(), "other", "{e}");
    assert!(e.to_string().contains("too many redirects"), "{e}");
    assert_eq!(reqs.await.unwrap().len(), 6); // the first request plus 5 redirects
}

#[tokio::test]
async fn elapsed_runs_from_send_until_the_body_ends() {
    let gap = Duration::from_millis(200);
    let head = b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: 5\r\n\r\n".to_vec();
    // Nothing at first, then the head after one gap and the body after another.
    let url = trickle(vec![vec![], head, b"hello".to_vec()], gap, Duration::ZERO).await;
    let r = fetcher().fetch_page(&url).await.unwrap();
    assert_eq!(r.body, b"hello");
    assert!(r.elapsed >= 2 * gap, "{:?}", r.elapsed);
}

// ---------- errors ----------

fn quick_timeout() -> Fetcher {
    Fetcher::new(&FetchConfig {
        timeout: Duration::from_millis(300),
        ..cfg(vec![])
    })
    .unwrap()
}

#[tokio::test]
async fn a_server_that_never_answers_times_out() {
    let url = trickle(vec![], Duration::ZERO, HOLD).await;
    let t = Instant::now();
    let e = quick_timeout().fetch_page(&url).await.unwrap_err();
    assert!(matches!(e, FetchError::Timeout) && e.retryable(), "{e}");
    assert!(t.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn a_body_that_stalls_times_out() {
    let part =
        b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: 100\r\n\r\n0123456789"
            .to_vec();
    let url = trickle(vec![part], Duration::ZERO, HOLD).await; // then silence
    let t = Instant::now();
    let e = quick_timeout().fetch_page(&url).await.unwrap_err();
    assert!(matches!(e, FetchError::Timeout), "{e}");
    assert!(t.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn the_cap_stops_reading_without_waiting_for_the_rest() {
    // Promises 10,000 bytes, sends 2,000 and stalls. Past the 1,000-byte cap the fetch must
    // return at once, not wait for the rest (it would time out).
    let part = [
        &b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: 10000\r\n\r\n"[..],
        &[b'a'; 2000],
    ]
    .concat();
    let url = trickle(vec![part], Duration::ZERO, HOLD).await;
    let f = Fetcher::new(&FetchConfig {
        max_body: 1000,
        timeout: Duration::from_millis(300),
        ..cfg(vec![])
    })
    .unwrap();
    let r = f.fetch_page(&url).await.unwrap();
    assert!(r.truncated && r.body.len() == 1000);
}

#[tokio::test]
async fn connect_timeout_is_separate_from_the_total() {
    // A listener whose accept queue is full: the kernel drops new SYNs, so connect hangs.
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = sock.listen(1).unwrap();
    let addr = listener.local_addr().unwrap();
    let mut held = vec![];
    while let Ok(s) =
        tokio::time::timeout(Duration::from_millis(200), TcpStream::connect(addr)).await
    {
        held.push(s.unwrap());
        assert!(held.len() < 100, "the accept queue never filled");
    }
    let f = Fetcher::new(&FetchConfig {
        connect_timeout: Duration::from_millis(300),
        ..cfg(vec![]) // total timeout 5 s
    })
    .unwrap();
    let t = Instant::now();
    let e = f
        .fetch_page(&Url::parse(&format!("http://{addr}/")).unwrap())
        .await
        .unwrap_err();
    assert!(matches!(e, FetchError::Timeout) && e.retryable(), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
}

#[tokio::test]
async fn connection_refused_is_retryable() {
    let addr = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap(); // closed again
    let e = fetcher()
        .fetch_page(&Url::parse(&format!("http://{addr}/")).unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(e, FetchError::Connect(_)) && e.retryable(),
        "{e:?}"
    );
    let msg = e.to_string();
    assert!(
        msg.starts_with("connect: ") && msg.to_lowercase().contains("refused"),
        "{msg}"
    );
    assert!(
        !msg.contains("http://"),
        "the URL is in the record already: {msg}"
    );
}

#[tokio::test]
async fn a_tls_handshake_failure_counts_as_connect() {
    // An https URL to a server that answers in plain HTTP.
    let url = trickle(vec![html(b"x")], Duration::ZERO, HOLD).await;
    let url = Url::parse(&url.as_str().replacen("http:", "https:", 1)).unwrap();
    let e = fetcher().fetch_page(&url).await.unwrap_err();
    assert!(
        matches!(e, FetchError::Connect(_)) && e.retryable(),
        "{e:?}"
    );
}

#[tokio::test]
async fn broken_responses_are_other_and_not_retried() {
    for reply in [
        b"hello, not http\r\n\r\n".to_vec(),
        // Promises 100 bytes, sends 10, hangs up.
        b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: 100\r\n\r\n0123456789"
            .to_vec(),
    ] {
        let e = page_from(reply, 1000).await.unwrap_err();
        assert!(matches!(e, FetchError::Other(_)) && !e.retryable(), "{e:?}");
        assert!(e.to_string().starts_with("other: "));
    }
}

#[test]
fn error_kinds_and_messages() {
    let cases = [
        (FetchError::Timeout, "timeout", true, "timeout"),
        (
            FetchError::Connect("dns".into()),
            "connect",
            true,
            "connect: dns",
        ),
        (
            FetchError::Other("bad".into()),
            "other",
            false,
            "other: bad",
        ),
    ];
    for (e, kind, retry, msg) in cases {
        assert_eq!(
            (e.kind(), e.retryable(), e.to_string().as_str()),
            (kind, retry, msg)
        );
    }
}

#[test]
fn a_user_agent_that_is_not_a_valid_header_is_refused() {
    assert!(
        Fetcher::new(&FetchConfig {
            user_agent: "bad\nagent".into(),
            ..cfg(vec![])
        })
        .is_err()
    );
}

#[test]
fn retry_after_forms() {
    let s = |n| Some(Duration::from_secs(n));
    for (v, want) in [
        (" 120 ", s(120)),
        ("0", s(0)),
        ("\t3\t", s(3)),
        ("99999999999999999999999", s(u64::MAX)), // too big: saturates, the frontier caps it
        ("Wed, 21 Oct 2015 07:28:00 GMT", None),  // HTTP-date: not parsed
        ("", None),
        ("   ", None),
        ("-1", None),
        ("+5", None),
        ("1.5", None),
        ("12a", None),
        ("1 2", None),
    ] {
        assert_eq!(parse_retry_after(v), want, "{v:?}");
    }
}
