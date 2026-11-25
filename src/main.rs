//! The `mccrawl` command: `crawl` the web from a seeds file, serve the synthetic `mockweb`, or
//! `bench` the crawler against it.

use std::{
    fs::OpenOptions,
    io::{IsTerminal, Write as _},
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use clap::{Args, Parser, Subcommand, ValueEnum, value_parser};
use mccrawl::{
    crawler::{self, BoxError, CrawlConfig},
    metrics::{self, Progress, Report},
    mockweb::{self, MockConfig},
    ui::{self, Dashboard, Live, Paint, Theme},
};
use serde::Serialize;
use tokio::sync::watch;

/// When stderr isn't a terminal, a plain progress line comes this often.
const PLAIN_EVERY: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(name = "mccrawl", version, about = "A fast, polite web crawler.", styles = ui::help_styles())]
struct Cli {
    /// Color theme
    #[arg(long, value_enum, default_value_t, global = true)]
    theme: Theme,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Crawl the web, starting from a file of seed URLs
    Crawl(CrawlArgs),
    /// Serve the synthetic test web on localhost until Ctrl-C
    Mockweb(MockArgs),
    /// Measure crawl speed against an in-process mock web
    Bench(BenchArgs),
}

#[derive(Args)]
struct CrawlArgs {
    /// File with one seed URL per line; `#` starts a comment
    #[arg(long)]
    seeds: PathBuf,
    /// URL or email address where site owners can reach you; it goes in the User-Agent
    #[arg(long)]
    contact: String,
    /// Output directory, which must not hold an earlier crawl [default: ~/mccrawl-data/run-<unix time>]
    #[arg(long)]
    out: Option<PathBuf>,
    /// Page fetches to make, retries and redirects included
    #[arg(long, default_value_t = 100_000, value_parser = value_parser!(u64).range(1..))]
    budget: u64,
    /// Fetches in flight at once
    #[arg(long, default_value_t = 512, value_parser = value_parser!(u32).range(1..))]
    concurrency: u32,
    /// Most pages fetched from one host
    #[arg(long, default_value_t = 500, value_parser = value_parser!(u32).range(1..))]
    per_host_cap: u32,
    /// Least time between two requests to one host, in ms (at most a day)
    #[arg(long, default_value_t = 1000, value_parser = value_parser!(u64).range(..=86_400_000))]
    min_delay_ms: u64,
}

#[derive(Args)]
struct MockArgs {
    /// Hosts to serve, h0.mock to h<N-1>.mock
    #[arg(long, default_value_t = 10_000, value_parser = value_parser!(u32).range(1..=1_000_000))]
    hosts: u32,
    /// Port on 127.0.0.1; 0 picks a free one
    #[arg(long, default_value_t = 8080)]
    port: u16,
    /// Response delay in ms: LOW..HIGH, or one number
    #[arg(long, default_value = "20..200", value_parser = parse_latency)]
    latency_ms: (u64, u64),
}

#[derive(Args)]
struct BenchArgs {
    /// Mock hosts, every one a seed
    #[arg(long, default_value_t = 10_000, value_parser = value_parser!(u32).range(1..=1_000_000))]
    hosts: u32,
    /// Page fetches per run
    #[arg(long, default_value_t = 200_000, value_parser = value_parser!(u64).range(1..))]
    pages: u64,
    #[arg(long, value_enum, default_value_t = Mode::Polite)]
    mode: Mode,
    /// Concurrency of each run, comma-separated
    #[arg(long, value_delimiter = ',', default_values_t = [64, 256, 1024], value_parser = value_parser!(u32).range(1..))]
    concurrency: Vec<u32>,
    /// Output directory [default: ~/mccrawl-data/bench-<unix time>]
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// 1 s between requests to a host and k = 5, against 20-200 ms responses
    Polite,
    /// No delays anywhere: the crawler's own ceiling
    Raw,
}

/// One bench run, as written to `bench.json`.
#[derive(Serialize)]
struct BenchRun {
    concurrency: u32,
    pages: u64,
    errors: u64,
    wall_secs: f64,
    pages_per_sec: f64,
    /// Polite mode: pages/s as a share of the most politeness allows.
    ceiling_pct: Option<f64>,
    latency_p50_ms: u32,
    latency_p99_ms: u32,
    /// This run's CPU, not the process total.
    cpu_secs: Option<f64>,
    peak_rss_mb: Option<f64>,
    stopped_early: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    raise_fd_limit();
    let result = match watch_signals() {
        Err(e) => Err(format!("can't watch for Ctrl-C: {e}").into()),
        Ok(stop) => match cli.command {
            Command::Crawl(a) => crawl(a, cli.theme, stop).await,
            Command::Mockweb(a) => serve_mock(a, cli.theme, stop).await,
            Command::Bench(a) => bench(a, cli.theme, stop).await,
        },
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let k = Paint::detect(cli.theme, std::io::stderr().is_terminal());
            ui::emit(&format!("{}\n", ui::error(&k, &e.to_string())));
            ExitCode::FAILURE
        }
    }
}

async fn crawl(a: CrawlArgs, theme: Theme, stop: watch::Receiver<bool>) -> Result<(), BoxError> {
    let k = Paint::detect(theme, std::io::stderr().is_terminal());
    let name = a.seeds.display();
    let text = std::fs::read_to_string(&a.seeds)
        .map_err(|e| format!("can't read the seeds file {name}: {e}"))?;
    let (seeds, warnings) = crawler::parse_seeds(&text);
    for w in &warnings {
        ui::emit(&format!("{}\n", ui::warning(&k, &format!("{name}: {w}"))));
    }
    if seeds.is_empty() {
        return Err(format!("no usable seed URLs in {name}").into());
    }
    let user_agent = crawler::user_agent(&a.contact).map_err(|e| format!("--contact: {e}"))?;
    let out = match a.out {
        Some(out) => out,
        None => default_out("run")?,
    };
    let noun = if seeds.len() == 1 { " seed" } else { " seeds" };
    let facts = k.value(&ui::count(seeds.len() as u64))
        + &k.label(noun)
        + &k.label(" · budget ")
        + &k.value(&ui::count(a.budget))
        + &k.label(" · ")
        + &k.value(&ui::count(a.concurrency.into()))
        + &k.label(" slots");
    banner(
        &k,
        "crawl",
        &facts,
        &(k.label("out ") + &k.value(&tilde(&out))),
    );
    let cfg = CrawlConfig {
        budget: a.budget,
        concurrency: a.concurrency as usize,
        per_host_cap: a.per_host_cap,
        min_delay: Duration::from_millis(a.min_delay_ms),
        user_agent,
        ..CrawlConfig::new(seeds, out.clone())
    };
    let report = run_live(cfg, &stop, &k, "crawling")
        .await
        .map_err(|e| explain(e, &out))?;
    let k_out = Paint::detect(theme, std::io::stdout().is_terminal());
    print(&ui::summary(&k_out, &report, *stop.borrow(), &tilde(&out)));
    Ok(())
}

async fn serve_mock(
    a: MockArgs,
    theme: Theme,
    stop: watch::Receiver<bool>,
) -> Result<(), BoxError> {
    let (lo, hi) = a.latency_ms;
    let cfg = MockConfig {
        hosts: a.hosts,
        latency_ms: a.latency_ms,
        ..MockConfig::default()
    };
    let web = mockweb::start(cfg, a.port)
        .await
        .map_err(|e| format!("can't listen on 127.0.0.1:{}: {e}", a.port))?;
    let k = Paint::detect(theme, std::io::stdout().is_terminal());
    let facts = k.value(&ui::count(a.hosts.into()))
        + &k.label(" hosts · latency ")
        + &k.value(&format!("{lo}..{hi} ms"));
    let place = k.label("on ") + &k.value(&web.addr.to_string());
    let (host, port) = (mockweb::host_name(4.min(a.hosts - 1)), web.addr.port());
    let mut lines = ui::banner(&k, "mockweb", &facts, &place);
    lines.push(String::new());
    lines.push(format!(
        "  {}  curl --resolve {host}:{port}:127.0.0.1 http://{host}:{port}/p/1",
        k.label("try ")
    ));
    lines.push(format!("  {}  ctrl-c", k.label("stop")));
    print(&lines);
    stopped(stop).await;
    Ok(())
}

async fn bench(a: BenchArgs, theme: Theme, stop: watch::Receiver<bool>) -> Result<(), BoxError> {
    let k = Paint::detect(theme, std::io::stderr().is_terminal());
    let raw = a.mode == Mode::Raw;
    let defaults = MockConfig::default();
    let mock = MockConfig {
        hosts: a.hosts,
        latency_ms: if raw { (0, 0) } else { defaults.latency_ms },
        ..defaults
    };
    let (min_delay, delay_factor) = if raw {
        (Duration::ZERO, 0.0)
    } else {
        (Duration::from_secs(1), 5.0)
    };
    let ceiling = (!raw).then(|| mockweb::politeness_ceiling(&mock, min_delay));
    let out = match a.out {
        Some(out) => out,
        None => default_out("bench")?,
    };
    // Checked now, not after a long bench.
    if out.join("bench.json").exists() {
        return Err(format!("{} already holds a bench; pick another --out", tilde(&out)).into());
    }
    let mode = if raw { "raw" } else { "polite" };
    let facts = k.value(&ui::count(a.hosts.into()))
        + &k.label(" mock hosts · ")
        + &k.value(&ui::count(a.pages))
        + &k.label(" pages a run · ")
        + &k.value(mode);
    banner(
        &k,
        "bench",
        &facts,
        &(k.label("out ") + &k.value(&tilde(&out))),
    );
    let web = mockweb::start(mock.clone(), 0).await?;

    let mut runs = Vec::new();
    for (i, &c) in a.concurrency.iter().enumerate() {
        if *stop.borrow() {
            break;
        }
        // Linux resets the peak RSS on this write, so each run reports its own.
        let _ = std::fs::write("/proc/self/clear_refs", "5");
        let cpu_before = metrics::cpu_secs();
        let cfg = CrawlConfig {
            budget: a.pages,
            concurrency: c as usize,
            min_delay,
            delay_factor,
            user_agent: "McCrawl/0.1 (+bench)".into(),
            resolve: web.resolve_overrides(),
            ..CrawlConfig::new(web.seeds(a.hosts), out.join(format!("{}-c{c}", i + 1)))
        };
        let title = format!(
            "bench · {} slots · {}/{}",
            ui::count(c.into()),
            i + 1,
            a.concurrency.len()
        );
        let r = run_live(cfg, &stop, &k, &title)
            .await
            .map_err(|e| explain(e, &out))?;
        let run = BenchRun {
            concurrency: c,
            pages: r.pages,
            errors: r.errors.values().sum(),
            wall_secs: r.wall_secs,
            pages_per_sec: r.pages_per_sec,
            ceiling_pct: ceiling.map(|top| r.pages_per_sec / top * 100.0),
            latency_p50_ms: r.latency_p50_ms,
            latency_p99_ms: r.latency_p99_ms,
            cpu_secs: cpu_before.zip(metrics::cpu_secs()).map(|(a, b)| b - a),
            peak_rss_mb: r.peak_rss_mb,
            stopped_early: *stop.borrow(),
        };
        let share = run
            .ceiling_pct
            .map(|p| k.label(" · ") + &k.value(&format!("{p:.1}%")) + &k.label(" of ceiling"))
            .unwrap_or_default();
        ui::emit(&format!(
            "  {} {}{}{}{}{}{}{}\n",
            k.good("✓"),
            k.value(&ui::count(c.into())),
            k.label(" slots · "),
            k.value(&format!("{:.1}", run.pages_per_sec)),
            k.label(" pages/s"),
            share,
            k.label(" · p99 "),
            k.value(&ui::ms(run.latency_p99_ms)),
        ));
        runs.push(run);
    }

    let json = serde_json::json!({
        "mode": mode,
        "hosts": a.hosts,
        "pages_per_run": a.pages,
        "min_delay_secs": min_delay.as_secs_f64(),
        "delay_factor": delay_factor,
        "mock_latency_ms": [mock.latency_ms.0, mock.latency_ms.1],
        "ceiling_pages_per_sec": ceiling,
        "runs": runs,
    });
    std::fs::create_dir_all(&out)?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out.join("bench.json"))?
        .write_all(&serde_json::to_vec_pretty(&json)?)?;

    let mut head = vec!["slots", "pages/s"];
    if !raw {
        head.push("% of ceiling");
    }
    head.extend(["p50 ms", "p99 ms", "CPU s", "peak RSS MiB"]);
    let opt = |v: Option<f64>, digits: usize| v.map_or("-".into(), |v| format!("{v:.digits$}"));
    let rows: Vec<Vec<String>> = runs
        .iter()
        .map(|r| {
            let mut row = vec![r.concurrency.to_string(), format!("{:.1}", r.pages_per_sec)];
            if !raw {
                row.push(opt(r.ceiling_pct, 1));
            }
            row.extend([
                r.latency_p50_ms.to_string(),
                r.latency_p99_ms.to_string(),
                opt(r.cpu_secs, 1),
                opt(r.peak_rss_mb, 0),
            ]);
            row
        })
        .collect();
    let k_out = Paint::detect(theme, std::io::stdout().is_terminal());
    print(&ui::table(&k_out, &head, &rows));
    Ok(())
}

/// Runs a crawl and shows its progress on stderr: the live panel in a terminal, otherwise a
/// plain line every few seconds.
async fn run_live(
    mut cfg: CrawlConfig,
    stop: &watch::Receiver<bool>,
    k: &Paint,
    title: &str,
) -> Result<Report, BoxError> {
    let (tx, rx) = watch::channel(Progress::default());
    cfg.progress = Some(tx);
    let dash = Dashboard::new(*k, title, cfg.budget, cfg.concurrency);
    let view = tokio::spawn(show(rx, dash, k.color()));
    let result = crawler::run(cfg, stopped(stop.clone())).await;
    let _ = view.await; // it ends once `run` has dropped the sender
    result
}

async fn show(mut rx: watch::Receiver<Progress>, mut dash: Dashboard, live: bool) {
    if !live {
        let mut next = PLAIN_EVERY;
        while rx.changed().await.is_ok() {
            let p = rx.borrow_and_update().clone();
            if p.elapsed >= next {
                ui::emit(&format!("{}\n", dash.plain(&p)));
                next = p.elapsed + PLAIN_EVERY;
            }
        }
        return;
    }
    let mut screen = Live::start();
    // New numbers come 4 times a second; the spinner turns 10 times, so a long save at the
    // end doesn't look like a hang.
    let mut frames = tokio::time::interval(Duration::from_millis(100));
    let mut last: Option<(Progress, Option<f64>)> = None;
    loop {
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    break; // `run` returned; dropping `screen` erases the panel
                }
                let p = rx.borrow_and_update().clone();
                dash.update(&p, metrics::cpu_secs());
                last = Some((p, metrics::peak_rss_mb()));
            }
            _ = frames.tick() => dash.tick(),
        }
        if let Some((p, mem)) = &last {
            let mut lines = dash.panel(p, *mem);
            if !ui::fits(lines.len()) {
                lines = vec![dash.line(p)];
            }
            screen.draw(&lines);
        }
    }
}

/// Watches Ctrl-C and SIGTERM for the whole process. The first turns the flag on, which
/// stops the crawl gracefully; a second quits at once.
fn watch_signals() -> std::io::Result<watch::Receiver<bool>> {
    let (tx, rx) = watch::channel(false);
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut int = signal(SignalKind::interrupt())?;
        let mut term = signal(SignalKind::terminate())?;
        tokio::spawn(async move {
            tokio::select! { _ = int.recv() => {}, _ = term.recv() => {} }
            tx.send_replace(true);
            tokio::select! { _ = int.recv() => {}, _ = term.recv() => {} }
            quit_now();
        });
    }
    #[cfg(not(unix))]
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tx.send_replace(true);
        let _ = tokio::signal::ctrl_c().await;
        quit_now();
    });
    Ok(rx)
}

fn quit_now() -> ! {
    ui::emit(ui::RESTORE);
    ui::emit("\nforced stop: the output may be incomplete\n");
    std::process::exit(130)
}

/// Completes once a stop is asked for; never, if the signal watcher is gone.
async fn stopped(mut stop: watch::Receiver<bool>) {
    if stop.wait_for(|&s| s).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// The banner on stderr, then a blank line.
fn banner(k: &Paint, command: &str, facts: &str, place: &str) {
    for line in ui::banner(k, command, facts, place) {
        ui::emit(&format!("{line}\n"));
    }
    ui::emit("\n");
}

/// Lines to stdout, ignoring errors (a closed pipe mustn't panic).
fn print(lines: &[String]) {
    let mut out = std::io::stdout().lock();
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
}

/// I/O errors don't say which file they were about; this adds the output directory.
fn explain(e: BoxError, out: &Path) -> BoxError {
    match e.downcast_ref::<std::io::Error>() {
        Some(io) if io.kind() == std::io::ErrorKind::AlreadyExists => format!(
            "{} already holds crawl output; pick another --out",
            tilde(out)
        )
        .into(),
        Some(io) => format!("{io} (output in {})", tilde(out)).into(),
        None => e,
    }
}

fn default_out(kind: &str) -> Result<PathBuf, BoxError> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or("HOME is not set; pass --out")?;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    Ok(PathBuf::from(home)
        .join("mccrawl-data")
        .join(format!("{kind}-{secs}")))
}

/// `path` with the home directory written as `~`.
fn tilde(path: &Path) -> String {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => Path::new("~").join(rest).display().to_string(),
        None => path.display().to_string(),
    }
}

/// "20..200" or "50", in ms, at most a minute.
fn parse_latency(s: &str) -> Result<(u64, u64), String> {
    let (lo, hi) = s.split_once("..").unwrap_or((s, s));
    let ms = |v: &str| {
        v.trim()
            .parse::<u64>()
            .map_err(|_| format!("{v:?} is not a whole number of milliseconds"))
    };
    let (lo, hi) = (ms(lo)?, ms(hi)?);
    if lo > hi {
        return Err(format!("{lo} is above {hi}"));
    }
    if hi > 60_000 {
        return Err("at most 60000 ms".into());
    }
    Ok((lo, hi))
}

/// Every fetch slot holds a socket, so lift the soft open-file limit to the hard one (often
/// 1,024 → 500k+), instead of failing at high concurrency.
fn raise_fd_limit() {
    #[cfg(unix)]
    // SAFETY: getrlimit and setrlimit only read and write the rlimit they are given.
    unsafe {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 && lim.rlim_cur < lim.rlim_max {
            lim.rlim_cur = lim.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
        }
    }
}
