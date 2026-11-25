//! Terminal output rendered to strings: number formats, the live panel (exact text, and every
//! line within `WIDTH` whatever the numbers), the banner, summary and table, and the redraw
//! sequence. Color is checked by stripping it: colored output must read exactly like plain.

use std::{collections::BTreeMap, ffi::OsStr, time::Duration};

use mccrawl::{
    frontier::FrontierStats,
    metrics::{Progress, Tally},
    store::StoreSummary,
    ui::{self, Dashboard, Paint, Theme, WIDTH, bytes, clock, count, ms},
};

/// Removes the color codes (`ESC [ ... m`).
fn strip(s: &str) -> String {
    let mut out = String::new();
    let mut in_code = false;
    for c in s.chars() {
        if in_code {
            in_code = c != 'm';
        } else if c == '\x1b' {
            in_code = true;
        } else {
            out.push(c);
        }
    }
    out
}

fn plain() -> Paint {
    Paint::new(Theme::Monokai, false)
}

fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s)
}

/// A row of the panel: label in 8 columns, value in 17, two spaces, then the visual.
fn row(label: &str, value: &str, visual: &str) -> String {
    format!("  {label:<8}{value:<17}  {visual}")
}

/// `left` and `right` across `WIDTH` columns.
fn spread(left: &str, right: &str) -> String {
    let gap = WIDTH - 2 - left.chars().count() - right.chars().count();
    format!("  {left}{}{right}", " ".repeat(gap))
}

/// 12,408 fetches done of 30,000 after 252 s.
fn sample() -> Progress {
    Progress {
        elapsed: secs(252.0),
        pages: 12_386,
        errors: 22,
        bytes: 432_013_312,
        status: BTreeMap::from([(200, 11_880), (203, 22), (301, 310), (404, 151), (503, 23)]),
        retries: 91,
        in_flight: 431,
        recent_p50_ms: 180,
        recent_p99_ms: 2400,
        frontier: FrontierStats {
            hosts: 1_241,
            dead_hosts: 37,
            queued: 88_410,
            dropped_disallowed: 312,
            ..Default::default()
        },
        stopping: false,
        saving: false,
    }
}

/// A dashboard fed 4 snapshots, 2 s apart, ending at `sample()`: rates 52, 100 and 52 pages/s,
/// and CPU use 50%, 50% and 75%.
fn fed(k: Paint) -> (Dashboard, Progress) {
    let mut d = Dashboard::new(k, "crawling", 30_000, 512);
    let at = |s: f64, pages: u64| Progress {
        elapsed: secs(s),
        pages,
        ..sample()
    };
    d.update(&at(246.0, 12_000 - 22), Some(100.0));
    d.update(&at(248.0, 12_104 - 22), Some(101.0));
    d.update(&at(250.0, 12_304 - 22), Some(102.0));
    let p = sample();
    d.update(&p, Some(103.5));
    d.tick();
    d.tick();
    (d, p)
}

#[test]
fn the_panel_shows_every_number_in_its_column() {
    let (d, p) = fed(plain());
    let want = [
        spread("⠹ crawling", "00:04:12"),
        String::new(),
        row(
            "pages",
            "12,408 / 30,000",
            &format!("{}╸{}  41%", "━".repeat(9), "─".repeat(14)),
        ),
        row("rate", "52.0 pages/s", &("▁".repeat(21) + "▅█▅")),
        row("data", "412 MiB", "p50 180 ms  p99 2.4 s"),
        String::new(),
        row("slots", "431 / 512", &("▰".repeat(20) + "▱▱▱▱")),
        row("hosts", "1,204 live", "37 dead · 88,410 queued"),
        String::new(),
        row("2xx", "11,902", &"█".repeat(24)),
        row("3xx", "310", "▋"),
        row("4xx", "151", "▎"),
        row("5xx", "23", "▏"),
        row("errors", "22", "91 retries · 312 disallowed"),
        String::new(),
        spread("cpu 75% · peak mem 212 MiB", "ctrl-c to stop"),
    ];
    assert_eq!(d.panel(&p, Some(212.0)), want);
    // The percent rounds down: 100% only when it's all done.
    let nearly = Progress {
        pages: 29_999,
        errors: 0,
        ..sample()
    };
    assert!(d.panel(&nearly, None)[2].ends_with("━╸  99%"));
    // A title too long for the line still leaves a space before the clock.
    let long = Dashboard::new(plain(), &"x".repeat(70), 1, 1);
    assert!(long.panel(&p, None)[0].ends_with(&format!("{} 00:04:12", "x".repeat(70))));
    // Unknown CPU and memory just leave them out.
    let fresh = Dashboard::new(plain(), "crawling", 30_000, 512);
    assert_eq!(
        fresh.panel(&p, None).last().unwrap(),
        &spread("", "ctrl-c to stop")
    );
}

#[test]
fn colored_output_reads_exactly_like_plain_in_both_themes() {
    let (plain_d, p) = fed(plain());
    let want = plain_d.panel(&p, Some(212.0));
    for theme in [Theme::Monokai, Theme::Mocha] {
        let k = Paint::new(theme, true);
        let (d, _) = fed(k);
        let got = d.panel(&p, Some(212.0));
        assert_eq!(got.iter().map(|l| strip(l)).collect::<Vec<_>>(), want);
        assert!(got[2].contains("\x1b[38;2;"), "no color: {:?}", got[2]);
        for line in &got {
            assert!(
                ui::width(line) <= WIDTH,
                "{} columns: {line:?}",
                ui::width(line)
            );
            assert_eq!(ui::width(line), strip(line).chars().count());
        }
        assert_eq!(strip(&d.line(&p)), plain_d.line(&p));
    }
    // Status bars in the theme's colors: 2xx good, 3xx info, 4xx warning, 5xx bad.
    let (d, _) = fed(Paint::new(Theme::Monokai, true));
    let got = d.panel(&p, None);
    for (line, rgb) in [
        (9, "169;220;118"),
        (10, "120;220;232"),
        (11, "252;152;103"),
        (12, "255;97;136"),
    ] {
        assert!(
            got[line].contains(&format!("38;2;{rgb}m")),
            "{:?}",
            got[line]
        );
    }
    // The two themes differ.
    let (m, _) = fed(Paint::new(Theme::Monokai, true));
    let (c, _) = fed(Paint::new(Theme::Mocha, true));
    assert_ne!(m.panel(&p, None), c.panel(&p, None));
}

#[test]
fn stopping_and_saving_say_so() {
    let d = Dashboard::new(plain(), "crawling", 30_000, 512);
    let mut p = Progress {
        stopping: true,
        in_flight: 1,
        ..sample()
    };
    let panel = d.panel(&p, None);
    assert_eq!(panel[0], spread("⠋ stopping", "finishing 1 download"));
    assert_eq!(
        panel.last().unwrap(),
        &spread("", "ctrl-c again to quit now")
    );
    assert!(d.line(&p).starts_with("⠋ stopping 41% "), "{}", d.line(&p));
    assert!(d.plain(&p).ends_with("  stopping"));
    p.in_flight = 3;
    assert_eq!(
        d.panel(&p, None)[0],
        spread("⠋ stopping", "finishing 3 downloads")
    );
    p.saving = true;
    let panel = d.panel(&p, None);
    assert_eq!(panel[0], spread("⠋ saving", "writing the last pages"));
    assert_eq!(panel.last().unwrap(), &spread("", ""));
    assert!(d.line(&p).starts_with("⠋ saving 41% "));
    assert!(d.plain(&p).ends_with("  saving"));
}

#[test]
fn the_rate_graph_takes_a_sample_every_2_s_and_keeps_the_last_24() {
    let mut d = Dashboard::new(plain(), "crawling", 1 << 40, 8);
    let at = |s: f64, pages: u64| Progress {
        elapsed: secs(s),
        pages,
        ..Default::default()
    };
    let rate = |d: &Dashboard, p: &Progress| {
        let panel = d.panel(p, None);
        let row = panel.iter().find(|l| l.starts_with("  rate")).unwrap();
        let (value, graph) = row[10..].split_at(19);
        (value.trim().to_owned(), graph.to_owned())
    };
    let footer = |d: &Dashboard, p: &Progress| d.panel(p, None).last().unwrap().clone();
    let empty = "▁".repeat(24);
    // The first snapshot sets the baseline; until a sample, the average so far is shown.
    let p = at(0.25, 50);
    d.update(&p, Some(1.0));
    assert_eq!(rate(&d, &p), ("200.0 pages/s".into(), empty.clone()));
    assert!(!footer(&d, &p).contains("cpu"));
    let p = at(2.0, 450); // 1.75 s after the baseline: still no sample
    d.update(&p, Some(1.5));
    assert_eq!(rate(&d, &p), ("225.0 pages/s".into(), empty.clone()));
    let p = at(2.25, 500); // 2 s: (500 - 50) / 2
    d.update(&p, Some(2.0));
    assert_eq!(rate(&d, &p), ("225.0 pages/s".into(), "▁".repeat(23) + "█"));
    assert!(
        footer(&d, &p).starts_with("  cpu 50% "),
        "{}",
        footer(&d, &p)
    );
    let p = at(4.25, 600); // 50/s, scaled against 225/s
    d.update(&p, Some(1.0)); // CPU going backwards counts as 0
    assert_eq!(rate(&d, &p), ("50.0 pages/s".into(), "▁".repeat(22) + "█▃"));
    assert!(footer(&d, &p).starts_with("  cpu 0% "));
    // 30 more samples of 100 pages: the old ones are gone, and the graph stays 24 wide.
    let (mut t, mut n) = (4.25, 600);
    for _ in 0..30 {
        (t, n) = (t + 2.0, n + 200);
        d.update(&at(t, n), None);
    }
    assert_eq!(
        rate(&d, &at(t, n)),
        ("100.0 pages/s".into(), "█".repeat(24))
    );
    assert!(!footer(&d, &at(t, n)).contains("cpu"), "unknown CPU");
}

#[test]
fn no_numbers_break_the_panel_width() {
    let max = Progress {
        elapsed: Duration::from_secs(1_000_000_000),
        pages: u64::MAX,
        errors: u64::MAX,
        bytes: u64::MAX,
        status: BTreeMap::from([(100, u64::MAX), (200, u64::MAX), (999, 1)]),
        retries: u64::MAX,
        in_flight: usize::MAX,
        recent_p50_ms: u32::MAX,
        recent_p99_ms: u32::MAX,
        frontier: FrontierStats {
            hosts: usize::MAX,
            dead_hosts: usize::MAX,
            queued: usize::MAX,
            dropped_disallowed: u64::MAX,
            dropped_cap: u64::MAX,
            dropped_dead: u64::MAX,
        },
        stopping: false,
        saving: false,
    };
    let start = Progress {
        elapsed: secs(1.0),
        ..Default::default()
    };
    // 3 s gives the highest rate; 10^9 s the longest clock.
    let cases = [
        (false, false, 3.0),
        (true, false, 3.0),
        (true, true, 3.0),
        (false, false, 1e9),
    ];
    for (budget, slots) in [(0, 0), (1, 1), (u64::MAX, usize::MAX)] {
        for (stopping, saving, at) in cases {
            let p = Progress {
                stopping,
                saving,
                elapsed: secs(at),
                ..max.clone()
            };
            let mut d = Dashboard::new(Paint::new(Theme::Mocha, true), "crawling", budget, slots);
            d.update(&start, Some(0.0));
            d.update(&p, Some(f64::MAX));
            let panel = d.panel(&p, Some(f64::MAX));
            for line in panel.iter().chain([&d.line(&p)]) {
                assert!(
                    ui::width(line) <= WIDTH,
                    "{} columns: {:?}",
                    ui::width(line),
                    strip(line)
                );
                let text = strip(line);
                assert!(!text.contains("NaN") && !text.contains("inf"), "{text:?}");
            }
            // Every status class seen gets a row, the rare ones too.
            let text: Vec<String> = panel.iter().map(|l| strip(l)).collect();
            for class in ["1xx", "2xx", "3xx", "4xx", "5xx", "9xx"] {
                assert!(
                    text.iter().any(|l| l.starts_with(&format!("  {class}"))),
                    "{class}"
                );
            }
        }
    }
    // An empty crawl: zeros, no NaN.
    let d = Dashboard::new(plain(), "crawling", 0, 0);
    let p = Progress::default();
    assert!(d.panel(&p, None).iter().all(|l| !l.contains("NaN")));
    let bar = format!("╸{}   0%", "─".repeat(23));
    assert_eq!(d.panel(&p, None)[2], row("pages", "0 / 0", &bar));
}

#[test]
fn the_plain_line_is_one_line_with_the_key_numbers() {
    let d = Dashboard::new(plain(), "crawling", 30_000, 512);
    let p = sample();
    assert_eq!(
        d.plain(&p),
        "  252.0s  pages 12408/30000 (49.2/s)  412 MiB  errors 22  retries 91  in flight 431  \
         queued 88410  hosts 1241 (37 dead)"
    );
    let zero = d.plain(&Progress::default());
    assert!(
        zero.starts_with("    0.0s  pages 0/30000 (0.0/s)"),
        "{zero}"
    );
}

#[test]
fn numbers_are_short_and_never_wider_than_their_column() {
    for (n, want) in [
        (0, "0"),
        (999, "999"),
        (1_000, "1,000"),
        (12_345, "12,345"),
        (999_999, "999,999"),
        (1_000_000, "1.0M"),
        (1_234_567, "1.2M"),
        (999_999_999, "999.9M"),
        (1_000_000_000, "1.0B"),
        (1_000_000_000_000, "1.0T"),
        (1_000_000_000_000_000, "1.0P"),
        (1_000_000_000_000_000_000, "1.0E"),
        (u64::MAX, "18.4E"),
    ] {
        assert_eq!(count(n), want);
    }
    for e in 0..20 {
        let n = 10u64.pow(e);
        for v in [n - 1, n, n + 1, n.saturating_mul(9)] {
            assert!(count(v).chars().count() <= 7, "{v} → {}", count(v));
        }
    }
    for (n, want) in [
        (0, "0 B"),
        (1023, "1023 B"),
        (1024, "1.00 KiB"),
        (10 << 10, "10.0 KiB"),
        (100 << 10, "100 KiB"),
        (432_013_312, "412 MiB"),
        (3 << 29, "1.50 GiB"),
        (u64::MAX, "16.0 EiB"),
    ] {
        assert_eq!(bytes(n), want);
    }
    for (v, want) in [
        (0, "0 ms"),
        (999, "999 ms"),
        (1000, "1.0 s"),
        (2400, "2.4 s"),
        (u32::MAX, "4294967.3 s"),
    ] {
        assert_eq!(ms(v), want);
    }
    for (s, want) in [
        (0, "00:00:00"),
        (59, "00:00:59"),
        (3661, "01:01:01"),
        (360_000, "100:00:00"),
    ] {
        assert_eq!(clock(Duration::from_secs(s)), want);
    }
}

#[test]
fn color_only_for_a_terminal_that_wants_it() {
    let os = |s: &'static str| Some(OsStr::new(s));
    assert!(ui::color_wanted(true, None, os("xterm-256color")));
    assert!(ui::color_wanted(true, None, None));
    assert!(
        ui::color_wanted(true, os(""), None),
        "an empty NO_COLOR doesn't count"
    );
    assert!(!ui::color_wanted(false, None, None), "not a terminal");
    assert!(!ui::color_wanted(true, os("1"), None), "NO_COLOR");
    assert!(!ui::color_wanted(true, None, os("dumb")));
    // Without color nothing has a code in it.
    let k = plain();
    for s in [
        k.label("a"),
        k.value("b"),
        k.accent("c"),
        k.good("d"),
        k.warn("e"),
        k.bad("f"),
    ] {
        assert!(!s.contains('\x1b'), "{s:?}");
    }
    assert_eq!(ui::width("\x1b[1;38;2;1;2;3mab\x1b[0m⠹━╸▰▱█▏"), 9);
}

#[test]
fn the_banner_is_the_logo_in_a_gradient_or_one_plain_line() {
    let version = env!("CARGO_PKG_VERSION");
    assert_eq!(
        ui::banner(&plain(), "crawl", "3 seeds", "out ~/x"),
        [format!("McCrawl {version} · crawl · 3 seeds · out ~/x")]
    );
    let k = Paint::new(Theme::Monokai, true);
    let lines = ui::banner(&k, "crawl", "3 seeds", "out ~/x");
    let text: Vec<String> = lines.iter().map(|l| strip(l)).collect();
    assert_eq!(
        text,
        [
            format!("  ┏┳┓┏━╸┏━╸┏━┓┏━┓╻ ╻╻     McCrawl {version} · crawl"),
            "  ┃┃┃┃  ┃  ┣┳┛┣━┫┃╻┃┃     3 seeds".into(),
            "  ╹ ╹┗━╸┗━╸╹┗╸╹ ╹┗┻┛┗━╸   out ~/x".into(),
        ]
    );
    // The gradient runs from the theme's first color to its last, across the logo.
    assert!(
        lines[2].contains("\x1b[1;38;2;255;97;136m╹"),
        "{:?}",
        lines[2]
    );
    assert!(
        lines[2].contains("\x1b[1;38;2;171;157;242m╸"),
        "{:?}",
        lines[2]
    );
    // In between, the colors blend, rounded: a quarter of the way from #ff6188 to #fc9867.
    assert!(
        lines[0].contains("\x1b[1;38;2;254;111;128m┳"),
        "{:?}",
        lines[0]
    );
    let mocha = ui::banner(&Paint::new(Theme::Mocha, true), "crawl", "", "");
    assert!(mocha[2].contains("\x1b[1;38;2;203;166;247m╹"));
    assert!(mocha[2].contains("\x1b[1;38;2;137;180;250m╸"));
}

#[test]
fn the_summary_says_what_was_fetched_and_where_it_went() {
    let mut t = Tally::default();
    t.page(200, 1, Duration::from_millis(1));
    t.errors.insert("timeout", 2);
    t.errors.insert("connect", 1);
    let frontier = FrontierStats {
        hosts: 1_241,
        dead_hosts: 37,
        ..Default::default()
    };
    let store = StoreSummary {
        warc_files: 3,
        near_dups: 412,
        ..Default::default()
    };
    let mut r = t.report(Duration::from_secs(603), frontier, store);
    (r.pages, r.pages_per_sec, r.bytes) = (30_000, 49.76, 1_095_216_660);
    (r.latency_p50_ms, r.latency_p99_ms) = (175, 2200);
    let want = [
        spread("✓ done", "00:10:03"),
        String::new(),
        "  30,000 pages   49.8/s   1.02 GiB   p50 175 ms  p99 2.2 s".into(),
        "  1,241 hosts · 37 dead · 412 near-duplicates · 3 errors".into(),
        String::new(),
        "  out     ~/mccrawl-data/run-1".into(),
        "          pages.jsonl · report.json · 3 WARC files".into(),
    ];
    assert_eq!(
        ui::summary(&plain(), &r, false, "~/mccrawl-data/run-1"),
        want
    );
    let colored = ui::summary(
        &Paint::new(Theme::Mocha, true),
        &r,
        false,
        "~/mccrawl-data/run-1",
    );
    assert_eq!(colored.iter().map(|l| strip(l)).collect::<Vec<_>>(), want);
    r.store.warc_files = 1;
    r.wall_secs = f64::NAN; // never panics
    let early = ui::summary(&plain(), &r, true, "/data");
    assert_eq!(early[0], spread("✓ stopped early", "00:00:00"));
    assert_eq!(
        early[6],
        "          pages.jsonl · report.json · 1 WARC file"
    );
}

#[test]
fn the_table_is_markdown_with_right_aligned_cells() {
    let rows = vec![
        vec!["64".to_owned(), "580.2".to_owned()],
        vec!["1024".to_owned(), "8100.0".to_owned()],
        vec!["7".to_owned()], // a short row gets an empty cell
    ];
    let want = [
        "| slots | pages/s |",
        "|------:|--------:|",
        "|    64 |   580.2 |",
        "|  1024 |  8100.0 |",
        "|     7 |         |",
    ];
    assert_eq!(ui::table(&plain(), &["slots", "pages/s"], &rows), want);
    let colored = ui::table(
        &Paint::new(Theme::Monokai, true),
        &["slots", "pages/s"],
        &rows,
    );
    assert_eq!(colored.iter().map(|l| strip(l)).collect::<Vec<_>>(), want);
    assert_eq!(ui::table(&plain(), &["µs"], &[]), ["| µs |", "|---:|"]);
}

#[test]
fn errors_and_warnings_are_labelled() {
    assert_eq!(ui::error(&plain(), "boom"), "error: boom");
    assert_eq!(ui::warning(&plain(), "hm"), "warning: hm");
    let k = Paint::new(Theme::Monokai, true);
    assert_eq!(
        ui::error(&k, "boom"),
        "\x1b[1;38;2;255;97;136merror:\x1b[0m boom"
    );
    assert_eq!(strip(&ui::warning(&k, "hm")), "warning: hm");
}

#[test]
fn a_redraw_goes_back_over_exactly_the_last_frame() {
    let lines = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        ui::redraw(0, &lines(&["a", "b"])),
        "\x1b[?2026h\x1b[Ja\nb\n\x1b[?2026l"
    );
    assert_eq!(
        ui::redraw(2, &lines(&["c"])),
        "\x1b[?2026h\x1b[2F\x1b[Jc\n\x1b[?2026l"
    );
    assert_eq!(ui::redraw(3, &[]), "\x1b[?2026h\x1b[3F\x1b[J\x1b[?2026l");
    assert_eq!(ui::RESTORE, "\x1b[?7h\x1b[?25h");
}
