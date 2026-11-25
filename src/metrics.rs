//! Crawl statistics. The scheduler is the only owner of a `Tally`, so the counters are plain
//! fields with no atomics; `progress` snapshots it for the live display, and `report` turns it
//! into the `report.json` summary at the end.

use std::{collections::BTreeMap, time::Duration};

use serde::Serialize;

use crate::{frontier::FrontierStats, store::StoreSummary};

#[derive(Default, Debug)]
pub struct Tally {
    /// Page fetches that got a response, whatever the status.
    pub pages: u64,
    /// Body bytes read (decoded, capped).
    pub bytes: u64,
    pub status: BTreeMap<u16, u64>,
    /// Failed fetches by `FetchError::kind`.
    pub errors: BTreeMap<&'static str, u64>,
    pub retries: u64,
    pub redirects: u64,
    pub non_html: u64,
    pub robots_fetches: u64,
    /// One entry per page response.
    // ponytail: 4 B per page; swap for a fixed-size histogram if crawls reach 100M+ pages.
    pub latencies_ms: Vec<u32>,
    /// Time with a free fetch slot but no host allowed yet: the cost of politeness.
    pub idle_slot: Duration,
}

#[derive(Serialize, Debug)]
pub struct Report {
    pub wall_secs: f64,
    pub pages: u64,
    pub pages_per_sec: f64,
    pub bytes: u64,
    pub status: BTreeMap<u16, u64>,
    pub errors: BTreeMap<&'static str, u64>,
    pub retries: u64,
    pub redirects: u64,
    pub non_html: u64,
    pub robots_fetches: u64,
    pub idle_slot_secs: f64,
    pub latency_p50_ms: u32,
    pub latency_p99_ms: u32,
    /// User + system CPU of the whole process. `None` off Linux.
    pub cpu_secs: Option<f64>,
    /// Peak resident memory in MiB. `None` off Linux.
    pub peak_rss_mb: Option<f64>,
    pub frontier: FrontierStats,
    pub store: StoreSummary,
}

impl Tally {
    /// Counts one page response.
    pub fn page(&mut self, status: u16, bytes: u64, elapsed: Duration) {
        self.pages += 1;
        self.bytes += bytes;
        *self.status.entry(status).or_default() += 1;
        self.latencies_ms
            .push(u32::try_from(elapsed.as_millis()).unwrap_or(u32::MAX));
    }

    pub fn report(
        mut self,
        wall: Duration,
        frontier: FrontierStats,
        store: StoreSummary,
    ) -> Report {
        self.latencies_ms.sort_unstable();
        Report {
            wall_secs: wall.as_secs_f64(),
            pages: self.pages,
            pages_per_sec: per_sec(self.pages, wall),
            bytes: self.bytes,
            status: self.status,
            errors: self.errors,
            retries: self.retries,
            redirects: self.redirects,
            non_html: self.non_html,
            robots_fetches: self.robots_fetches,
            idle_slot_secs: self.idle_slot.as_secs_f64(),
            latency_p50_ms: percentile(&self.latencies_ms, 0.5),
            latency_p99_ms: percentile(&self.latencies_ms, 0.99),
            cpu_secs: cpu_secs(),
            peak_rss_mb: peak_rss_mb(),
            frontier,
            store,
        }
    }

    /// A snapshot for the live display. Latency percentiles are over the last `RECENT`
    /// responses, so they follow what the crawl is doing now and cost little to compute.
    pub fn progress(
        &self,
        elapsed: Duration,
        in_flight: usize,
        frontier: FrontierStats,
        stopping: bool,
    ) -> Progress {
        let from = self.latencies_ms.len().saturating_sub(RECENT);
        let mut recent = self.latencies_ms[from..].to_vec();
        recent.sort_unstable();
        Progress {
            elapsed,
            pages: self.pages,
            errors: self.errors.values().sum(),
            bytes: self.bytes,
            status: self.status.clone(),
            retries: self.retries,
            in_flight,
            recent_p50_ms: percentile(&recent, 0.5),
            recent_p99_ms: percentile(&recent, 0.99),
            frontier,
            stopping,
            saving: false,
        }
    }
}

/// Responses the live latency percentiles are taken over.
pub const RECENT: usize = 1024;

/// A running crawl as the live display sees it, sent a few times a second.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub elapsed: Duration,
    /// Page fetches that got a response, whatever the status.
    pub pages: u64,
    /// Page fetches that failed without a response.
    pub errors: u64,
    pub bytes: u64,
    pub status: BTreeMap<u16, u64>,
    pub retries: u64,
    /// Fetches in flight, robots.txt included.
    pub in_flight: usize,
    pub recent_p50_ms: u32,
    pub recent_p99_ms: u32,
    pub frontier: FrontierStats,
    /// The shutdown signal fired: nothing new starts, and the fetches in flight are finishing.
    pub stopping: bool,
    /// The last snapshot: fetching is over, and the parse pool and the store are writing what
    /// is left, which can take seconds.
    pub saving: bool,
}

/// 0 for a zero duration rather than infinity, which JSON can't hold (serde_json writes null).
fn per_sec(n: u64, wall: Duration) -> f64 {
    let secs = wall.as_secs_f64();
    if secs > 0.0 { n as f64 / secs } else { 0.0 }
}

/// Nearest-rank percentile of ascending `sorted`: the smallest value with at least a fraction
/// `p` of the data at or below it. `p` outside 0..=1 acts as the nearest end (NaN as 0); empty
/// gives 0.
pub fn percentile(sorted: &[u32], p: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    // Shaved by one part in 10^12, so float error (0.07 × 100 = 7.000000000000001) can't push
    // the rank up by one. The cast saturates (negative and NaN give 0), and the clamp does the rest.
    let rank = (p * sorted.len() as f64 * (1.0 - 1e-12)).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// The `VmHWM` (peak resident set) line of `/proc/<pid>/status`, in kB.
pub fn parse_vmhwm_kb(status: &str) -> Option<u64> {
    let value = status.lines().find_map(|l| l.strip_prefix("VmHWM:"))?;
    value.trim().strip_suffix("kB")?.trim_end().parse().ok()
}

/// User + system CPU seconds from `/proc/<pid>/stat`: fields 14 and 15, in ticks of 1/100 s
/// (USER_HZ, 100 on every mainstream Linux). Fields are counted after the last `)`, since the
/// command name in parentheses may itself hold spaces and `)`.
pub fn parse_cpu_secs(stat: &str) -> Option<f64> {
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace().skip(11);
    let utime: u64 = fields.next()?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some((utime as f64 + stime as f64) / 100.0)
}

pub fn peak_rss_mb() -> Option<f64> {
    let kb = parse_vmhwm_kb(&std::fs::read_to_string("/proc/self/status").ok()?)?;
    Some(kb as f64 / 1024.0)
}

pub fn cpu_secs() -> Option<f64> {
    parse_cpu_secs(&std::fs::read_to_string("/proc/self/stat").ok()?)
}
