//! The `mccrawl` binary end to end: bad arguments refused before anything is created, a crawl
//! of a local site, a stop signal (graceful, then forced), the mock web server and a bench.
//! Output is piped here, so these runs also check the plain mode: no color, no redraws.
//! Linux only: the tests send signals and read `/proc`.
#![cfg(target_os = "linux")]

use axum::{
    Router,
    http::{HeaderMap, StatusCode, Uri},
    response::{Html, IntoResponse, Response},
};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_mccrawl");
const CONTACT: &str = "https://example.com/bot";

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mccrawl-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn command(args: &[&str]) -> Command {
    let mut c = Command::new(BIN);
    c.args(args).env_remove("NO_COLOR");
    c
}

fn mccrawl(args: &[&str]) -> Output {
    command(args).output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// A started binary that is killed when dropped, so a failing test can't leave it running:
/// it holds the test's output open, and cargo would wait for it forever.
struct Running(Child);

impl std::ops::Deref for Running {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl std::ops::DerefMut for Running {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(cmd: &mut Command) -> Running {
    Running(cmd.spawn().unwrap())
}

fn signal(child: &Child, sig: i32) {
    // SAFETY: kill only sends a signal to the child we started.
    assert_eq!(unsafe { libc::kill(child.id() as i32, sig) }, 0);
}

/// Waits up to `limit` for the child to exit.
fn wait(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    while start.elapsed() < limit {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Runs to the end, or is killed after `limit` (then None): a broken build that keeps serving
/// fails the test instead of hanging it.
fn run_for(args: &[&str], limit: Duration) -> Option<(ExitStatus, String)> {
    let mut child = start(command(args).stdout(Stdio::null()).stderr(Stdio::piped()));
    let status = wait(&mut child, limit)?; // on a timeout, dropping `child` kills it
    Some((status, read_all(child.stderr.take())))
}

fn read_all(r: Option<impl Read>) -> String {
    let mut out = String::new();
    r.unwrap().read_to_string(&mut out).unwrap();
    out
}

/// A local site, served from its own thread for the rest of the test. It records each
/// request's User-Agent.
///
/// - `/` links to `/a` and `/b`, which are leaves;
/// - `/endless/{n}` takes 50 ms and links to `/endless/{n+1}`;
/// - `/hang` never answers.
fn site() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let agents = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&agents);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let app = Router::new().fallback(move |headers: HeaderMap, uri: Uri| {
                let log = Arc::clone(&log);
                async move { respond(&log, &headers, uri.path()).await }
            });
            axum::serve(listener, app).await.unwrap();
        });
    });
    (addr, agents)
}

async fn respond(log: &Mutex<Vec<String>>, headers: &HeaderMap, path: &str) -> Response {
    let agent = headers
        .get("user-agent")
        .map(|v| v.to_str().unwrap().to_owned());
    log.lock().unwrap().push(agent.unwrap_or_default());
    if path == "/robots.txt" {
        return StatusCode::NOT_FOUND.into_response();
    }
    if path == "/hang" {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
    if let Some(n) = path.strip_prefix("/endless/") {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let next = n.parse::<u64>().unwrap() + 1;
        return Html(format!(r#"<a href="/endless/{next}">next</a>"#)).into_response();
    }
    match path {
        "/" => Html(r#"<a href="/a">a</a> <a href="/b">b</a>"#).into_response(),
        _ => Html("leaf").into_response(),
    }
}

fn json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn help_lists_every_command_and_flag() {
    let o = mccrawl(&["--help"]);
    assert!(o.status.success());
    let help = text(&o.stdout);
    for word in ["crawl", "mockweb", "bench", "--theme", "monokai", "mocha"] {
        assert!(help.contains(word), "{word} missing from:\n{help}");
    }
    assert!(!help.contains('\x1b'), "color in piped help");
    let help = text(&mccrawl(&["crawl", "--help"]).stdout);
    for flag in [
        "--seeds",
        "--contact",
        "--out",
        "--budget",
        "--concurrency",
        "--per-host-cap",
        "--min-delay-ms",
    ] {
        assert!(help.contains(flag), "{flag} missing");
    }
    let o = mccrawl(&["--version"]);
    assert_eq!(
        text(&o.stdout).trim(),
        format!("mccrawl {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn crawl_refuses_bad_input_before_creating_anything() {
    let d = dir("refuse");
    let seeds = d.join("seeds.txt");
    std::fs::write(&seeds, "http://127.0.0.1:9/\n").unwrap();
    let out = d.join("out");
    let crawl = |extra: &[&str]| {
        let mut args = vec!["crawl", "--seeds", s(&seeds), "--out", s(&out)];
        args.extend(extra);
        mccrawl(&args)
    };
    // Usage errors: clap's exit code 2.
    let usage: [&[&str]; 9] = [
        &[],
        &["--contact", CONTACT, "--budget", "0"],
        &["--contact", CONTACT, "--budget", "-1"],
        &["--contact", CONTACT, "--budget", "lots"],
        &["--contact", CONTACT, "--concurrency", "0"],
        &["--contact", CONTACT, "--per-host-cap", "0"],
        &["--contact", CONTACT, "--min-delay-ms", "86400001"],
        &["--contact", CONTACT, "--theme", "neon"],
        &["--contact", CONTACT, "--bogus"],
    ];
    for extra in usage {
        let o = crawl(extra);
        assert_eq!(o.status.code(), Some(2), "{extra:?}: {}", text(&o.stderr));
        assert!(!out.exists(), "{extra:?} created the output");
    }
    assert!(text(&crawl(&[]).stderr).contains("--contact"));
    // Errors found by the crawler: exit code 1, one plain `error:` line.
    let o = crawl(&["--contact", "not a contact"]);
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        err.contains("error: --contact: the contact must be"),
        "{err}"
    );
    let missing = mccrawl(&[
        "crawl",
        "--seeds",
        s(&d.join("nope.txt")),
        "--contact",
        CONTACT,
    ]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(text(&missing.stderr).contains("error: can't read the seeds file"));
    std::fs::write(&seeds, "# nothing usable\nftp://x.com/\nexample.com\n").unwrap();
    let o = crawl(&["--contact", CONTACT]);
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        err.contains("line 2: skipped") && err.contains("line 3: skipped"),
        "{err}"
    );
    assert!(err.contains("error: no usable seed URLs in"), "{err}");
    assert!(!out.exists());
    assert!(!err.contains('\x1b'));
    // An earlier crawl's output: refused, and left as it was.
    std::fs::write(&seeds, "http://127.0.0.1:9/\n").unwrap();
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("pages.jsonl"), "keep").unwrap();
    let o = crawl(&["--contact", CONTACT]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("already holds crawl output; pick another --out"));
    assert_eq!(std::fs::read(out.join("pages.jsonl")).unwrap(), b"keep");
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn crawl_fetches_a_site_and_prints_a_summary() {
    let (addr, agents) = site();
    let d = dir("crawl");
    let seeds = d.join("seeds.txt");
    std::fs::write(
        &seeds,
        format!("# a test site\nhttp://{addr}/\nnot a url\n"),
    )
    .unwrap();
    let out = d.join("out");
    let args = [
        "crawl",
        "--seeds",
        s(&seeds),
        "--contact",
        CONTACT,
        "--out",
        s(&out),
        "--min-delay-ms",
        "0",
        "--budget",
        "100",
        "--concurrency",
        "7",
        "--per-host-cap",
        "9",
    ];
    let o = mccrawl(&args);
    let (stdout, stderr) = (text(&o.stdout), text(&o.stderr));
    assert!(o.status.success(), "{stderr}");
    assert!(
        stderr.contains("warning: ") && stderr.contains("line 3: skipped"),
        "{stderr}"
    );
    let banner = format!(
        "McCrawl {} · crawl · 1 seed · budget 100 · 7 slots · out ",
        env!("CARGO_PKG_VERSION")
    );
    assert!(stderr.contains(&banner), "{stderr}");
    for want in [
        "✓ done",
        "3 pages",
        &format!("out     {}", out.display()),
        "pages.jsonl · report.json · 1 WARC file",
    ] {
        assert!(stdout.contains(want), "{want:?} missing from:\n{stdout}");
    }
    assert!(
        !stdout.contains('\x1b') && !stderr.contains('\x1b'),
        "color in piped output"
    );
    // Every request (robots.txt and 3 pages) carried the contact.
    let agents = agents.lock().unwrap().clone();
    assert_eq!(agents, vec![format!("McCrawl/0.1 (+{CONTACT})"); 4]);
    let report = json(&out.join("report.json"));
    assert_eq!(
        (
            report["pages"].as_u64(),
            report["config"]["budget"].as_u64()
        ),
        (Some(3), Some(100))
    );
    assert_eq!(
        report["config"]["user_agent"],
        format!("McCrawl/0.1 (+{CONTACT})")
    );
    let config = &report["config"];
    assert_eq!(
        (&config["concurrency"], &config["per_host_cap"]),
        (&Value::from(7), &Value::from(9))
    );
    assert_eq!(config["min_delay_secs"], 0.0);
    let rows = std::fs::read_to_string(out.join("pages.jsonl")).unwrap();
    assert_eq!(rows.lines().count(), 3);

    // Without --out: a new run directory under $HOME/mccrawl-data, shown with `~`.
    let home = d.join("home");
    let o = command(&args[..5]).env("HOME", &home).output().unwrap();
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(
        text(&o.stdout).contains("out     ~/mccrawl-data/run-"),
        "{}",
        text(&o.stdout)
    );
    let runs: Vec<_> = std::fs::read_dir(home.join("mccrawl-data"))
        .unwrap()
        .collect();
    assert_eq!(runs.len(), 1);
    assert!(
        runs[0]
            .as_ref()
            .unwrap()
            .path()
            .join("report.json")
            .exists()
    );
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn a_stop_signal_ends_the_crawl_gracefully_and_a_second_quits_at_once() {
    let (addr, _) = site();
    let d = dir("stop");
    let seeds = d.join("seeds.txt");
    let crawl = |path: &str, out: &Path| {
        std::fs::write(&seeds, format!("http://{addr}{path}\n")).unwrap();
        let args = [
            "crawl",
            "--seeds",
            s(&seeds),
            "--contact",
            CONTACT,
            "--out",
            s(out),
            "--min-delay-ms",
            "0",
        ];
        start(command(&args).stdout(Stdio::piped()).stderr(Stdio::piped()))
    };

    // An endless site: past 5 s a plain progress line is logged; Ctrl-C then stops the crawl
    // within moments, with every output written.
    let out = d.join("graceful");
    let mut child = crawl("/endless/0", &out);
    std::thread::sleep(Duration::from_millis(5600));
    signal(&child, libc::SIGINT);
    let status = wait(&mut child, Duration::from_secs(5)).expect("still running after Ctrl-C");
    let (stdout, stderr) = (read_all(child.stdout.take()), read_all(child.stderr.take()));
    assert!(status.success(), "{status:?}: {stderr}");
    assert!(stdout.contains("✓ stopped early"), "{stdout}");
    // One plain progress line at 5 s (the next would come at 10 s).
    assert_eq!(stderr.matches("s  pages ").count(), 1, "{stderr}");
    assert!(stderr.contains("/100000 ("), "{stderr}");
    let report = json(&out.join("report.json"));
    // 50 ms answers, then k = 5 × 50 ms between them: about 3 pages a second.
    assert!(report["pages"].as_u64().unwrap() >= 10, "{report}");

    // A fetch that never ends: SIGTERM starts the 10 s grace period, and a second signal
    // quits at once with 130.
    let out = d.join("forced");
    let mut child = crawl("/hang", &out);
    std::thread::sleep(Duration::from_millis(800));
    signal(&child, libc::SIGTERM);
    assert!(
        wait(&mut child, Duration::from_millis(1000)).is_none(),
        "didn't wait for the fetch"
    );
    let t = Instant::now();
    signal(&child, libc::SIGINT);
    let status = wait(&mut child, Duration::from_secs(2)).expect("a second signal didn't quit");
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    assert_eq!(status.code(), Some(130));
    let stderr = read_all(child.stderr.take());
    assert!(stderr.contains("forced stop: the output may be incomplete"));
    assert!(
        stderr.contains("\x1b[?7h\x1b[?25h"),
        "the terminal isn't restored"
    );
    assert!(!out.join("report.json").exists());
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn mockweb_serves_until_ctrl_c() {
    use std::os::unix::process::CommandExt;
    let mut cmd = command(&[
        "mockweb",
        "--port",
        "0",
        "--hosts",
        "20",
        "--latency-ms",
        "0",
    ]);
    // Cargo raises its own open-file limit and children inherit it, so start the binary at a
    // low one, the way a login shell usually does.
    // SAFETY: getrlimit and setrlimit are async-signal-safe and only touch the rlimit given.
    unsafe {
        cmd.pre_exec(|| {
            let mut lim = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim);
            lim.rlim_cur = lim.rlim_max.min(256);
            libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
            Ok(())
        });
    }
    let mut child = start(cmd.stdout(Stdio::piped()));
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let try_line = lines
        .by_ref()
        .map(Result::unwrap)
        .find(|l| l.contains("curl --resolve"))
        .expect("no curl example");
    let port: u16 = try_line
        .rsplit("http://h4.mock:")
        .next()
        .and_then(|rest| rest.strip_suffix("/p/1"))
        .unwrap()
        .parse()
        .unwrap();
    // h1 is the Crawl-delay host.
    let mut tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        tcp,
        "GET /robots.txt HTTP/1.1\r\nHost: h1.mock:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut reply = String::new();
    tcp.read_to_string(&mut reply).unwrap();
    assert!(
        reply.starts_with("HTTP/1.1 200") && reply.contains("Crawl-delay: 2"),
        "{reply}"
    );
    // The open-file limit was raised to the hard limit.
    let limits = std::fs::read_to_string(format!("/proc/{}/limits", child.id())).unwrap();
    let files: Vec<&str> = limits
        .lines()
        .find(|l| l.starts_with("Max open files"))
        .unwrap()
        .split_whitespace()
        .collect();
    assert_eq!(files[3], files[4], "soft limit not raised: {files:?}");
    assert_ne!(
        files[3], "256",
        "the hard limit is 256 too, so this proves nothing"
    );
    signal(&child, libc::SIGINT);
    let status = wait(&mut child, Duration::from_secs(3)).expect("still serving after Ctrl-C");
    assert!(status.success(), "{status:?}");

    // Bad settings are usage errors; a taken port is a plain error.
    for bad in [
        ["--latency-ms", "5..1"],
        ["--latency-ms", "x"],
        ["--latency-ms", "60001"],
        ["--hosts", "0"],
    ] {
        let args = ["mockweb", "--port", "0", bad[0], bad[1]];
        let (status, _) = run_for(&args, Duration::from_secs(5)).expect("accepted, and served");
        assert_eq!(status.code(), Some(2), "{bad:?}");
    }
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port().to_string();
    let (status, stderr) = run_for(&["mockweb", "--port", &port], Duration::from_secs(5))
        .expect("served on another port");
    assert_eq!(status.code(), Some(1));
    assert!(
        stderr.contains(&format!("error: can't listen on 127.0.0.1:{port}")),
        "{stderr}"
    );
}

#[test]
fn bench_runs_each_concurrency_and_writes_bench_json() {
    let d = dir("bench");
    let out = d.join("raw");
    let o = mccrawl(&[
        "bench",
        "--hosts",
        "20",
        "--pages",
        "40",
        "--mode",
        "raw",
        "--concurrency",
        "2,4,4",
        "--out",
        s(&out),
    ]);
    let (stdout, stderr) = (text(&o.stdout), text(&o.stderr));
    assert!(o.status.success(), "{stderr}");
    let table: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        table[0],
        "| slots | pages/s | p50 ms | p99 ms | CPU s | peak RSS MiB |"
    );
    assert_eq!(table.len(), 5, "{stdout}");
    for (line, slots) in table[2..].iter().zip(["2", "4", "4"]) {
        assert!(line.starts_with(&format!("| {slots:>5} |")), "{line}");
    }
    assert_eq!(stderr.matches("✓ ").count(), 3, "{stderr}");
    let bench = json(&out.join("bench.json"));
    assert_eq!(
        (
            bench["mode"].as_str(),
            bench["ceiling_pages_per_sec"].is_null()
        ),
        (Some("raw"), true)
    );
    let settings =
        ["min_delay_secs", "delay_factor", "mock_latency_ms"].map(|k| bench[k].to_string());
    assert_eq!(settings, ["0.0", "0.0", "[0,0]"]);
    let runs = bench["runs"].as_array().unwrap();
    let slots: Vec<u64> = runs
        .iter()
        .map(|r| r["concurrency"].as_u64().unwrap())
        .collect();
    assert_eq!(slots, [2, 4, 4]);
    for (i, run) in runs.iter().enumerate() {
        let fetched = run["pages"].as_u64().unwrap() + run["errors"].as_u64().unwrap();
        assert_eq!(fetched, 40, "{run}");
        assert!(run["ceiling_pct"].is_null() && run["stopped_early"] == false);
        let c = slots[i];
        assert!(
            out.join(format!("{}-c{c}", i + 1))
                .join("report.json")
                .exists()
        );
    }
    // The same --out again is refused before anything runs.
    let again = mccrawl(&["bench", "--hosts", "20", "--pages", "40", "--out", s(&out)]);
    assert_eq!(again.status.code(), Some(1));
    assert!(text(&again.stderr).contains("already holds a bench"));

    // Polite mode compares against the politeness ceiling: 8 hosts at 1/s and 1 at 1/2 s
    // (the 10th has robots.txt down).
    let out = d.join("polite");
    let o = mccrawl(&[
        "bench",
        "--hosts",
        "10",
        "--pages",
        "12",
        "--concurrency",
        "8",
        "--out",
        s(&out),
    ]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(text(&o.stdout).starts_with("| slots | pages/s | % of ceiling |"));
    let bench = json(&out.join("bench.json"));
    assert_eq!(bench["ceiling_pages_per_sec"], 8.5);
    let settings =
        ["min_delay_secs", "delay_factor", "mock_latency_ms"].map(|k| bench[k].to_string());
    assert_eq!(settings, ["1.0", "5.0", "[20,200]"]);
    let pct = bench["runs"][0]["ceiling_pct"].as_f64().unwrap();
    assert!(pct > 0.0 && pct <= 100.0, "{pct}");

    // Ctrl-C during the first of three polite runs: it stops early, and the others don't run.
    let out = d.join("stopped");
    let args = [
        "bench",
        "--hosts",
        "10",
        "--pages",
        "1000",
        "--concurrency",
        "4,8,16",
        "--out",
        s(&out),
    ];
    let mut child = start(command(&args).stdout(Stdio::piped()).stderr(Stdio::piped()));
    std::thread::sleep(Duration::from_millis(1500));
    signal(&child, libc::SIGINT);
    let status = wait(&mut child, Duration::from_secs(5)).expect("still running after Ctrl-C");
    assert!(status.success(), "{}", read_all(child.stderr.take()));
    let table = read_all(child.stdout.take());
    assert_eq!(table.lines().count(), 3, "{table}");
    let runs = json(&out.join("bench.json"))["runs"].clone();
    assert_eq!(runs.as_array().unwrap().len(), 1, "{runs}");
    assert_eq!(
        (&runs[0]["concurrency"], &runs[0]["stopped_early"]),
        (&Value::from(4), &Value::from(true))
    );
    assert!(!out.join("2-c8").exists());
    std::fs::remove_dir_all(&d).ok();
}
