use std::{
    collections::{BTreeMap, BTreeSet},
    hint::black_box,
    time::{Duration, Instant},
};

use mccrawl::{
    frontier::FrontierStats,
    metrics::{Tally, cpu_secs, parse_cpu_secs, parse_vmhwm_kb, peak_rss_mb, percentile},
    store::StoreSummary,
};
use serde_json::Value;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn keys(v: &Value) -> BTreeSet<&str> {
    v.as_object().unwrap().keys().map(String::as_str).collect()
}

// ---- percentile ----

#[test]
fn percentile_nearest_rank() {
    let v: Vec<u32> = (1..=100).collect();
    assert_eq!(percentile(&v, 0.5), 50);
    assert_eq!(percentile(&v, 0.99), 99);
    assert_eq!(percentile(&[], 0.5), 0);
    // The ends, and p outside 0..=1.
    assert_eq!(percentile(&v, 0.0), 1);
    assert_eq!(percentile(&v, 1.0), 100);
    assert_eq!(percentile(&v, 1.5), 100);
    assert_eq!(percentile(&v, f64::INFINITY), 100);
    assert_eq!(percentile(&v, -0.5), 1);
    assert_eq!(percentile(&v, f64::NAN), 1);
    assert_eq!((percentile(&[7], 0.0), percentile(&[7], 0.99)), (7, 7));
    // Just past a rank boundary moves up one.
    assert_eq!(percentile(&v, 0.501), 51);
    assert_eq!(
        (percentile(&[10, 20], 0.5), percentile(&[10, 20], 0.51)),
        (10, 20)
    );
    assert_eq!(
        (
            percentile(&[5, 5, 5, 9], 0.75),
            percentile(&[5, 5, 5, 9], 0.76)
        ),
        (5, 9)
    );
}

#[test]
fn percentile_matches_exact_integer_ranks() {
    // Nearest rank is ceil(p·n), at least 1. With p = k/100 or k/1000 that is exact integer math,
    // so float error (0.07 × 100 = 7.000000000000001) must never shift the answer.
    for n in [1usize, 2, 3, 7, 10, 99, 100, 101, 1000, 12_345] {
        let v: Vec<u32> = (0..n as u32).map(|i| i * 3).collect(); // distinct, so an off-by-one shows
        for (k, scale) in (0..=100)
            .map(|k| (k, 100))
            .chain((0..=1000).map(|k| (k, 1000)))
        {
            let rank = (k * n).div_ceil(scale).max(1);
            let p = k as f64 / scale as f64;
            assert_eq!(percentile(&v, p), v[rank - 1], "n={n} p={k}/{scale}");
        }
    }
}

// ---- /proc parsing ----

#[test]
fn vmhwm_parsing() {
    assert_eq!(
        parse_vmhwm_kb("Name:\tx\nVmHWM:\t  12345 kB\n"),
        Some(12345)
    );
    let real = "Name:\tmccrawl\nVmPeak:\t  900000 kB\nVmSize:\t  800000 kB\n\
                VmHWM:\t   54321 kB\nVmRSS:\t   40000 kB\n";
    assert_eq!(
        parse_vmhwm_kb(real),
        Some(54321),
        "the peak, not VmPeak or VmRSS"
    );
    assert_eq!(parse_vmhwm_kb("VmHWM:\t0 kB"), Some(0));
    assert_eq!(
        parse_vmhwm_kb("VmHWM:\t18446744073709551615 kB"),
        Some(u64::MAX)
    );
    for bad in [
        "",
        "Name:\tx\n",
        "VmRSS:\t5 kB\n",
        "VmHWM:\t12345\n",
        "VmHWM:\t12345 MB\n",
        "VmHWM:\tlots kB\n",
        "VmHWM:\t-5 kB\n",
        "VmHWM:\t18446744073709551616 kB\n",
        " VmHWM:\t5 kB\n",
    ] {
        assert_eq!(parse_vmhwm_kb(bad), None, "{bad:?}");
    }
}

#[test]
fn cpu_time_parsing() {
    assert_eq!(
        parse_cpu_secs("1 (my (weird) comm) S 0 0 0 0 0 0 0 0 0 0 250 150 0 0"),
        Some(4.0)
    );
    // A full line as Linux writes it: utime 1234, stime 56 ticks.
    let real = "4242 (mccrawl) R 1 4242 4242 34816 4242 4194304 1520 0 0 0 1234 56 0 0 20 0 9 0 \
                123456 1234567 890 18446744073709551615 1 1 0 0 0 0 0 4096 17920 0 0 0 17 3 0 0 0 0 0";
    assert_eq!(parse_cpu_secs(real), Some(12.9));
    // A command name holding ") " and numbers, which splitting at the first ')' would miscount.
    assert_eq!(
        parse_cpu_secs("7 (a) 1 2 3 4 5 6 7 8 9 10 11) R 0 0 0 0 0 0 0 0 0 0 100 0"),
        Some(1.0)
    );
    // Huge tick counts don't overflow.
    let huge = format!("1 (x) S{} {} {}", " 0".repeat(10), u64::MAX, u64::MAX);
    assert!(parse_cpu_secs(&huge).unwrap() > 3e17);
    for bad in [
        "",
        "1 (x) S 0 0 0",
        "1 x S 0 0 0 0 0 0 0 0 0 0 1 2",
        "1 (x) S 0 0 0 0 0 0 0 0 0 0 1",
        "1 (x) S 0 0 0 0 0 0 0 0 0 0 1 two",
        "1 (x) S 0 0 0 0 0 0 0 0 0 0 -1 2",
    ] {
        assert_eq!(parse_cpu_secs(bad), None, "{bad:?}");
    }
}

#[test]
fn live_readers_track_this_process() {
    let kb = || parse_vmhwm_kb(&std::fs::read_to_string("/proc/self/status").unwrap()).unwrap();
    let (kb0, rss0, kb1) = (kb(), peak_rss_mb().expect("VmHWM on Linux"), kb());
    assert!(
        kb0 as f64 / 1024.0 <= rss0 && rss0 <= kb1 as f64 / 1024.0,
        "MiB of VmHWM: {rss0}"
    );
    assert!(rss0 > 0.0 && rss0 < 100_000.0, "{rss0}");
    // 64 MiB actually written to (vec! of a nonzero byte fills every page)...
    let big = black_box(vec![1u8; 64 << 20]);
    let rss1 = peak_rss_mb().unwrap();
    assert!(rss1 >= rss0 + 60.0, "{rss0} -> {rss1}");
    drop(big);
    assert!(
        peak_rss_mb().unwrap() >= rss1,
        "a high-water mark never goes down"
    );

    // ...and 300 ms of CPU work.
    let cpu0 = cpu_secs().expect("/proc/self/stat on Linux");
    let start = Instant::now();
    let mut x = 0u64;
    while start.elapsed() < ms(300) {
        x = black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
    }
    let cpu1 = cpu_secs().unwrap();
    assert!(cpu1 - cpu0 >= 0.2, "{cpu0} -> {cpu1}");
}

// ---- Tally ----

#[test]
fn page_counts_and_saturates_latency() {
    let mut t = Tally::default();
    t.page(200, 1000, Duration::from_micros(1500)); // whole milliseconds, rounded down
    t.page(200, 0, Duration::from_micros(999));
    t.page(404, 10, Duration::MAX);
    t.page(200, 5, ms(u32::MAX as u64)); // exactly fits
    t.page(200, 5, ms(u32::MAX as u64 + 1));
    assert_eq!((t.pages, t.bytes), (5, 1020));
    assert_eq!(t.status, BTreeMap::from([(200, 4), (404, 1)]));
    assert_eq!(t.latencies_ms, [1, 0, u32::MAX, u32::MAX, u32::MAX]);
}

#[test]
fn report_carries_every_count() {
    let mut t = Tally::default();
    for l in [30, 10, 20, 40, 1000, 50, 60, 70, 80, 90] {
        t.page(200, 100, ms(l)); // out of order on purpose
    }
    t.page(503, 0, ms(5));
    t.errors.insert("timeout", 2);
    t.errors.insert("connect", 1);
    (t.retries, t.redirects, t.non_html, t.robots_fetches) = (3, 4, 5, 6);
    t.idle_slot = ms(1500);
    let f = FrontierStats {
        hosts: 7,
        dead_hosts: 1,
        queued: 8,
        dropped_disallowed: 9,
        dropped_cap: 10,
        dropped_dead: 11,
    };
    let s = StoreSummary {
        records: 12,
        warc_records: 13,
        warc_files: 1,
        exact_dups: 2,
        near_dups: 3,
    };
    let r = t.report(Duration::from_secs(2), f, s);

    assert_eq!(
        (r.wall_secs, r.pages, r.pages_per_sec, r.bytes),
        (2.0, 11, 5.5, 1000)
    );
    // Sorted: 5 10 20 30 40 50 60 70 80 90 1000. Ranks ceil(5.5) = 6 and ceil(10.89) = 11.
    assert_eq!((r.latency_p50_ms, r.latency_p99_ms), (50, 1000));
    assert_eq!(r.status, BTreeMap::from([(200, 10), (503, 1)]));
    assert_eq!(r.errors, BTreeMap::from([("connect", 1), ("timeout", 2)]));
    assert_eq!(
        (r.retries, r.redirects, r.non_html, r.robots_fetches),
        (3, 4, 5, 6)
    );
    assert_eq!(r.idle_slot_secs, 1.5);
    assert!(r.cpu_secs.is_some() && r.peak_rss_mb.is_some());

    let j = serde_json::to_value(&r).unwrap();
    let want = [
        "bytes",
        "cpu_secs",
        "errors",
        "frontier",
        "idle_slot_secs",
        "latency_p50_ms",
        "latency_p99_ms",
        "non_html",
        "pages",
        "pages_per_sec",
        "peak_rss_mb",
        "redirects",
        "retries",
        "robots_fetches",
        "status",
        "store",
        "wall_secs",
    ];
    assert_eq!(keys(&j), BTreeSet::from(want));
    let frontier = [
        "dead_hosts",
        "dropped_cap",
        "dropped_dead",
        "dropped_disallowed",
        "hosts",
        "queued",
    ];
    assert_eq!(keys(&j["frontier"]), BTreeSet::from(frontier));
    let store = [
        "exact_dups",
        "near_dups",
        "records",
        "warc_files",
        "warc_records",
    ];
    assert_eq!(keys(&j["store"]), BTreeSet::from(store));
    assert_eq!(
        (&j["status"]["200"], &j["status"]["503"]),
        (&Value::from(10), &Value::from(1))
    );
    assert_eq!(j["errors"]["timeout"], 2);
    assert_eq!(
        (&j["frontier"]["hosts"], &j["frontier"]["dropped_dead"]),
        (&Value::from(7), &Value::from(11))
    );
    assert_eq!(
        (&j["store"]["records"], &j["store"]["near_dups"]),
        (&Value::from(12), &Value::from(3))
    );
}

#[test]
fn an_empty_or_instant_crawl_reports_zeros_not_nulls() {
    let r = Tally::default().report(
        Duration::ZERO,
        FrontierStats::default(),
        StoreSummary::default(),
    );
    assert_eq!(
        (r.pages, r.pages_per_sec, r.latency_p50_ms, r.latency_p99_ms),
        (0, 0.0, 0, 0)
    );
    let mut t = Tally::default();
    t.page(200, 1, ms(1));
    let r = t.report(
        Duration::ZERO,
        FrontierStats::default(),
        StoreSummary::default(),
    );
    assert_eq!(r.pages_per_sec, 0.0, "not infinity");
    let j = serde_json::to_value(&r).unwrap();
    assert_eq!(
        j["pages_per_sec"], 0.0,
        "JSON has no infinity; serde_json would write null"
    );
}

#[test]
fn progress_line_is_one_line_with_the_key_numbers() {
    let mut t = Tally::default();
    for _ in 0..30 {
        t.page(200, 1 << 20, ms(1));
    }
    t.errors.insert("timeout", 2);
    t.errors.insert("other", 1);
    t.retries = 4;
    let f = FrontierStats {
        hosts: 12,
        dead_hosts: 3,
        queued: 456,
        ..Default::default()
    };
    assert_eq!(
        t.progress_line(Duration::from_secs(10), &f),
        "   10.0s  pages 30 (3.0/s)  30.0 MiB  errors 3  retries 4  queued 456  hosts 12 (3 dead)"
    );
    let instant = t.progress_line(Duration::ZERO, &f);
    assert!(
        instant.contains("(0.0/s)") && !instant.contains('\n'),
        "{instant}"
    );
}
