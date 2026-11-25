//! Terminal output: two color themes (Monokai Pro, the default, and Catppuccin Mocha), the
//! start banner, the live crawl panel, the end summary and the bench table. Everything renders
//! to strings, with or without color, so it is tested without a terminal; `Live` draws the
//! panel in place on stderr.

use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsStr,
    fmt::Write as _,
    io::Write as _,
    time::Duration,
};

use clap::builder::styling::{Color, RgbColor, Style, Styles};

use crate::metrics::{Progress, Report};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Theme {
    #[default]
    Monokai,
    Mocha,
}

/// Colors as 0xRRGGBB.
struct Palette {
    title: u32,
    text: u32,
    dim: u32,
    faint: u32,
    /// Left to right across the logo and the progress bar; at least two stops.
    grad: &'static [u32],
    ok: u32,
    info: u32,
    warn: u32,
    bad: u32,
}

const MONOKAI: Palette = Palette {
    title: 0xffd866,
    text: 0xfcfcfa,
    dim: 0x939293,
    faint: 0x5b595c,
    grad: &[0xff6188, 0xfc9867, 0xffd866, 0xa9dc76, 0x78dce8, 0xab9df2],
    ok: 0xa9dc76,
    info: 0x78dce8,
    warn: 0xfc9867,
    bad: 0xff6188,
};

const MOCHA: Palette = Palette {
    title: 0xcba6f7,
    text: 0xcdd6f4,
    dim: 0x7f849c,
    faint: 0x45475a,
    grad: &[0xcba6f7, 0x89b4fa],
    ok: 0xa6e3a1,
    info: 0x89b4fa,
    warn: 0xf9e2af,
    bad: 0xf38ba8,
};

/// Width of the live panel; with nothing else on the line it fits an 80-column window.
pub const WIDTH: usize = 64;
/// Undoes what `Live` changes: line wrapping and the hidden cursor.
pub const RESTORE: &str = "\x1b[?7h\x1b[?25h";

const CELLS: usize = 24;
/// Rate samples the graph shows, one per `SAMPLE`.
const HISTORY: usize = 24;
const SAMPLE: Duration = Duration::from_secs(2);
const SPIN: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const EIGHTHS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
const LOGO: [&str; 3] = [
    "┏┳┓┏━╸┏━╸┏━┓┏━┓╻ ╻╻  ",
    "┃┃┃┃  ┃  ┣┳┛┣━┫┃╻┃┃  ",
    "╹ ╹┗━╸┗━╸╹┗╸╹ ╹┗┻┛┗━╸",
];

/// Color on for a terminal, unless `NO_COLOR` is set to anything but "" (no-color.org) or
/// `TERM` is `dumb`.
pub fn color_wanted(is_terminal: bool, no_color: Option<&OsStr>, term: Option<&OsStr>) -> bool {
    is_terminal && no_color.is_none_or(OsStr::is_empty) && term.is_none_or(|t| t != "dumb")
}

/// A theme, with color on or off. Off, every function here gives the same text uncolored.
#[derive(Clone, Copy)]
pub struct Paint {
    pal: &'static Palette,
    color: bool,
}

impl Paint {
    pub fn new(theme: Theme, color: bool) -> Self {
        let pal = match theme {
            Theme::Monokai => &MONOKAI,
            Theme::Mocha => &MOCHA,
        };
        Self { pal, color }
    }

    /// For a stream: color if it is a terminal and the environment doesn't say otherwise.
    pub fn detect(theme: Theme, is_terminal: bool) -> Self {
        let (no_color, term) = (std::env::var_os("NO_COLOR"), std::env::var_os("TERM"));
        Self::new(
            theme,
            color_wanted(is_terminal, no_color.as_deref(), term.as_deref()),
        )
    }

    pub fn color(&self) -> bool {
        self.color
    }

    // ponytail: 24-bit color only; add a 256-color fallback if a terminal without it turns up.
    fn fg(&self, rgb: u32, bold: bool, s: &str) -> String {
        if !self.color || s.is_empty() {
            return s.to_owned();
        }
        let b = if bold { "1;" } else { "" };
        let (r, g, bl) = (rgb >> 16, (rgb >> 8) & 0xff, rgb & 0xff);
        format!("\x1b[{b}38;2;{r};{g};{bl}m{s}\x1b[0m")
    }

    pub fn label(&self, s: &str) -> String {
        self.fg(self.pal.dim, false, s)
    }
    pub fn value(&self, s: &str) -> String {
        self.fg(self.pal.text, true, s)
    }
    pub fn accent(&self, s: &str) -> String {
        self.fg(self.pal.title, true, s)
    }
    pub fn good(&self, s: &str) -> String {
        self.fg(self.pal.ok, true, s)
    }
    pub fn warn(&self, s: &str) -> String {
        self.fg(self.pal.warn, true, s)
    }
    pub fn bad(&self, s: &str) -> String {
        self.fg(self.pal.bad, true, s)
    }

    /// The gradient at `t` in 0..=1.
    fn grad(&self, t: f64) -> u32 {
        let stops = self.pal.grad;
        let x = t.clamp(0.0, 1.0) * (stops.len() - 1) as f64;
        let i = (x as usize).min(stops.len() - 2);
        mix(stops[i], stops[i + 1], x - i as f64)
    }
}

fn mix(a: u32, b: u32, t: f64) -> u32 {
    let channel = |shift: u32| {
        let (x, y) = (((a >> shift) & 0xff) as f64, ((b >> shift) & 0xff) as f64);
        ((x + (y - x) * t).round() as u32) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// `--help` in the default theme. (It is printed before `--theme` is read.)
pub fn help_styles() -> Styles {
    let c = |rgb: u32| {
        let (r, g, b) = ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
        Style::new().fg_color(Some(Color::Rgb(RgbColor(r, g, b))))
    };
    let p = &MONOKAI;
    Styles::styled()
        .header(c(p.title).bold())
        .usage(c(p.title).bold())
        .literal(c(p.info).bold())
        .placeholder(c(p.grad[5]))
        .error(c(p.bad).bold())
        .valid(c(p.ok))
        .invalid(c(p.warn))
}

/// Columns `s` takes: its characters minus color codes. Every symbol used here is one column.
pub fn width(s: &str) -> usize {
    let mut in_code = false;
    s.chars()
        .filter(|&c| {
            if in_code {
                in_code = c != 'm';
                false
            } else if c == '\x1b' {
                in_code = true;
                false
            } else {
                true
            }
        })
        .count()
}

fn pad(s: String, w: usize) -> String {
    let n = width(&s);
    s + &" ".repeat(w.saturating_sub(n))
}

/// `left` and `right` on one line of `WIDTH`, at least one space apart.
fn spread(left: &str, right: &str) -> String {
    let gap = WIDTH.saturating_sub(2 + width(left) + width(right)).max(1);
    format!("  {left}{}{right}", " ".repeat(gap))
}

/// 12,345 below a million, then 1.2M, 3.4B …; at most 7 columns for any u64.
pub fn count(n: u64) -> String {
    const UNITS: [(u64, &str); 5] = [
        (1_000_000_000_000_000_000, "E"),
        (1_000_000_000_000_000, "P"),
        (1_000_000_000_000, "T"),
        (1_000_000_000, "B"),
        (1_000_000, "M"),
    ];
    if let Some(&(div, unit)) = UNITS.iter().find(|(div, _)| n >= *div) {
        let tenths = n / (div / 10); // rounded down, so 999,999,999 never shows as 1000.0M
        return format!("{}.{}{unit}", tenths / 10, tenths % 10);
    }
    let digits = n.to_string();
    let mut out = String::new();
    for (i, d) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(d);
    }
    out
}

/// 412 MiB, 1.02 GiB …
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let (mut v, mut i) = (n as f64, 0);
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    match i {
        0 => format!("{n} B"),
        _ if v < 10.0 => format!("{v:.2} {}", UNITS[i]),
        _ if v < 100.0 => format!("{v:.1} {}", UNITS[i]),
        _ => format!("{v:.0} {}", UNITS[i]),
    }
}

/// 180 ms, 2.4 s
pub fn ms(v: u32) -> String {
    if v < 1000 {
        format!("{v} ms")
    } else {
        format!("{:.1} s", f64::from(v) / 1000.0)
    }
}

/// hh:mm:ss (hours keep growing past 99).
pub fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Pages per second, at most 7 columns.
fn rate_text(r: f64) -> String {
    if r < 100_000.0 {
        format!("{r:.1}")
    } else {
        count(r as u64) // saturates; an absurd rate still fits
    }
}

fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 {
        0.0
    } else {
        (a as f64 / b as f64).min(1.0)
    }
}

/// A bar of `frac` × `CELLS` in eighths of a cell, with at least a sliver when above 0.
fn blocks(frac: f64) -> String {
    let eighths = (frac * (CELLS * 8) as f64).round() as usize;
    let eighths = if frac > 0.0 { eighths.max(1) } else { 0 };
    let mut s = "█".repeat(eighths / 8);
    if eighths % 8 > 0 {
        s.push(EIGHTHS[eighths % 8 - 1]);
    }
    s
}

/// Page fetches finished, failed ones included: what the budget counts down.
fn done(p: &Progress) -> u64 {
    p.pages.saturating_add(p.errors)
}

/// The logo beside three lines: the name and `command`, then `facts` and `place` (both already
/// painted). Without color, one line.
pub fn banner(k: &Paint, command: &str, facts: &str, place: &str) -> Vec<String> {
    let version = env!("CARGO_PKG_VERSION");
    let name = format!(
        "{}{}",
        k.accent("McCrawl"),
        k.label(&format!(" {version} · {command}"))
    );
    if !k.color {
        return vec![format!("{name} · {facts} · {place}")];
    }
    LOGO.iter()
        .zip([name, facts.to_owned(), place.to_owned()])
        .map(|(row, info)| {
            let logo: String = row
                .chars()
                .enumerate()
                .map(|(i, c)| match c {
                    ' ' => " ".to_owned(),
                    _ => k.fg(k.grad(i as f64 / 20.0), true, &c.to_string()),
                })
                .collect();
            format!("  {logo}   {info}")
        })
        .collect()
}

/// The live crawl panel. It keeps what a single snapshot can't show: the rate over time and
/// the CPU use between samples.
pub struct Dashboard {
    k: Paint,
    title: String,
    budget: u64,
    concurrency: usize,
    frame: usize,
    /// Pages per second, one sample per `SAMPLE`, oldest first.
    rates: VecDeque<f64>,
    /// (elapsed, fetches done, CPU seconds) at the last sample.
    last: Option<(Duration, u64, Option<f64>)>,
    cpu_pct: Option<f64>,
}

impl Dashboard {
    pub fn new(k: Paint, title: &str, budget: u64, concurrency: usize) -> Self {
        Self {
            k,
            title: title.to_owned(),
            budget,
            concurrency,
            frame: 0,
            rates: VecDeque::new(),
            last: None,
            cpu_pct: None,
        }
    }

    /// Takes in a snapshot. The first sets the baseline; then, every `SAMPLE`, the rate since
    /// the last sample joins the graph and the CPU use is measured. `cpu_secs` is the process
    /// total.
    pub fn update(&mut self, p: &Progress, cpu_secs: Option<f64>) {
        let here = (p.elapsed, done(p), cpu_secs);
        let Some((t, n, cpu)) = self.last else {
            self.last = Some(here);
            return;
        };
        if p.elapsed < t + SAMPLE {
            return;
        }
        let secs = (p.elapsed - t).as_secs_f64();
        self.rates
            .push_back(done(p).saturating_sub(n) as f64 / secs);
        if self.rates.len() > HISTORY {
            self.rates.pop_front();
        }
        self.cpu_pct = cpu
            .zip(cpu_secs)
            .map(|(a, b)| (b - a).max(0.0) / secs * 100.0)
            .filter(|c| c.is_finite());
        self.last = Some(here);
    }

    /// The last sample, or the average so far until there is one.
    fn rate(&self, p: &Progress) -> f64 {
        match self.rates.back() {
            Some(&r) => r,
            None if !p.elapsed.is_zero() => done(p) as f64 / p.elapsed.as_secs_f64(),
            None => 0.0,
        }
    }

    /// Turns the spinner one step, so the panel shows it's alive between snapshots.
    pub fn tick(&mut self) {
        self.frame += 1;
    }

    fn spin(&self) -> String {
        SPIN[self.frame % SPIN.len()].to_string()
    }

    /// The progress bar: heavy and colored for what's done, light and faint for the rest.
    fn bar(&self, frac: f64) -> String {
        let k = &self.k;
        let step = |i: usize| k.grad(i as f64 / (CELLS - 1) as f64);
        let full = ((frac * CELLS as f64) as usize).min(CELLS);
        let mut s: String = (0..full).map(|i| k.fg(step(i), false, "━")).collect();
        if full < CELLS {
            s += &k.fg(step(full), false, "╸");
            s += &k.fg(k.pal.faint, false, &"─".repeat(CELLS - full - 1));
        }
        s
    }

    /// Rate samples, scaled to the highest; missing ones are faint baselines on the left.
    fn spark(&self) -> String {
        let k = &self.k;
        let hi = self.rates.iter().copied().fold(0.0, f64::max);
        let mut s = k.fg(k.pal.faint, false, &"▁".repeat(HISTORY - self.rates.len()));
        for &r in &self.rates {
            let level = if hi > 0.0 {
                (r / hi * 7.0).round() as usize
            } else {
                0
            };
            let color = mix(k.pal.info, k.pal.title, level as f64 / 7.0);
            s += &k.fg(color, false, &SPARK[level].to_string());
        }
        s
    }

    /// The whole panel, every line at most `WIDTH` columns.
    pub fn panel(&self, p: &Progress, mem_mb: Option<f64>) -> Vec<String> {
        let k = &self.k;
        let row = |label: &str, value: String, visual: String| {
            let label = k.label(&format!("{label:<8}"));
            format!("  {label}{}  {visual}", pad(value, 17))
        };
        let pair = |v: String, l: &str| format!("{}{}", k.value(&v), k.label(l));

        let (left, right) = if p.saving {
            let left = format!("{} {}", k.accent(&self.spin()), k.accent("saving"));
            (left, k.label("writing the last pages"))
        } else if p.stopping {
            let noun = if p.in_flight == 1 {
                "download"
            } else {
                "downloads"
            };
            let left = format!("{} {}", k.warn(&self.spin()), k.warn("stopping"));
            (
                left,
                k.label(&format!("finishing {} {noun}", count(p.in_flight as u64))),
            )
        } else {
            let left = format!("{} {}", k.accent(&self.spin()), k.accent(&self.title));
            (left, k.value(&clock(p.elapsed)))
        };

        let frac = ratio(done(p), self.budget);
        let percent = k.value(&format!("{:>5}", format!("{}%", (frac * 100.0).floor())));
        let total = k.label(&format!(" / {}", count(self.budget)));
        let latency =
            k.label("p50 ") + &pair(ms(p.recent_p50_ms), "  p99 ") + &k.value(&ms(p.recent_p99_ms));
        let used = ((ratio(p.in_flight as u64, self.concurrency as u64) * CELLS as f64).round()
            as usize)
            .min(CELLS);
        let slots = k.fg(k.pal.info, false, &"▰".repeat(used))
            + &k.fg(k.pal.faint, false, &"▱".repeat(CELLS - used));
        let f = &p.frontier;
        let live = f.hosts.saturating_sub(f.dead_hosts) as u64;

        let mut lines = vec![
            spread(&left, &right),
            String::new(),
            row(
                "pages",
                k.value(&count(done(p))) + &total,
                self.bar(frac) + &percent,
            ),
            row(
                "rate",
                pair(rate_text(self.rate(p)), " pages/s"),
                self.spark(),
            ),
            row("data", k.value(&bytes(p.bytes)), latency),
            String::new(),
            row(
                "slots",
                pair(
                    count(p.in_flight as u64),
                    &format!(" / {}", count(self.concurrency as u64)),
                ),
                slots,
            ),
            row(
                "hosts",
                pair(count(live), " live"),
                pair(count(f.dead_hosts as u64), " dead · ")
                    + &pair(count(f.queued as u64), " queued"),
            ),
            String::new(),
        ];
        // 2xx to 5xx always, any other class only when seen.
        let mut groups = BTreeMap::from([(2u16, 0u64), (3, 0), (4, 0), (5, 0)]);
        for (&code, &n) in &p.status {
            let g = groups.entry(code / 100).or_default();
            *g = g.saturating_add(n);
        }
        let top = groups.values().copied().max().unwrap_or(0);
        for (&g, &n) in &groups {
            let color = match g {
                2 => k.pal.ok,
                3 => k.pal.info,
                4 => k.pal.warn,
                5 => k.pal.bad,
                _ => k.pal.dim,
            };
            let bar = k.fg(color, false, &blocks(ratio(n, top)));
            lines.push(row(&format!("{g}xx"), k.value(&count(n)), bar));
        }
        lines.push(row(
            "errors",
            k.value(&count(p.errors)),
            pair(count(p.retries), " retries · ")
                + &pair(count(f.dropped_disallowed), " disallowed"),
        ));
        lines.push(String::new());

        let mut usage = Vec::new();
        if let Some(c) = self.cpu_pct {
            usage.push(k.label("cpu ") + &k.value(&format!("{}%", count(c.round() as u64))));
        }
        if let Some(m) = mem_mb {
            usage.push(k.label("peak mem ") + &k.value(&bytes((m * 1_048_576.0) as u64)));
        }
        let hint = match (p.saving, p.stopping) {
            (true, _) => "",
            (false, true) => "ctrl-c again to quit now",
            (false, false) => "ctrl-c to stop",
        };
        lines.push(spread(&usage.join(&k.label(" · ")), &k.label(hint)));
        lines
    }

    /// One line, for a window too small for the panel.
    pub fn line(&self, p: &Progress) -> String {
        let k = &self.k;
        let frac = ratio(done(p), self.budget);
        let spin = if p.stopping && !p.saving {
            k.warn(&self.spin())
        } else {
            k.accent(&self.spin())
        };
        let state = match (p.saving, p.stopping) {
            (true, _) => k.accent("saving "),
            (false, true) => k.warn("stopping "),
            (false, false) => String::new(),
        };
        format!(
            "{spin} {state}{} {} {} {}",
            k.value(&format!("{}%", (frac * 100.0).floor())),
            k.label(&format!("· {}/{} ·", count(done(p)), count(self.budget))),
            k.value(&rate_text(self.rate(p))),
            k.label(&format!("pages/s · {} errors", count(p.errors))),
        )
    }

    /// One uncolored line for logs, when stderr isn't a terminal.
    pub fn plain(&self, p: &Progress) -> String {
        let secs = p.elapsed.as_secs_f64();
        let avg = if secs > 0.0 {
            done(p) as f64 / secs
        } else {
            0.0
        };
        let f = &p.frontier;
        format!(
            "{secs:7.1}s  pages {}/{} ({avg:.1}/s)  {}  errors {}  retries {}  in flight {}  \
             queued {}  hosts {} ({} dead){}",
            done(p),
            self.budget,
            bytes(p.bytes),
            p.errors,
            p.retries,
            p.in_flight,
            f.queued,
            f.hosts,
            f.dead_hosts,
            match (p.saving, p.stopping) {
                (true, _) => "  saving",
                (false, true) => "  stopping",
                (false, false) => "",
            }
        )
    }
}

/// The end card: what was fetched, then where it went. It isn't redrawn, so a long path may
/// wrap.
pub fn summary(k: &Paint, r: &Report, stopped_early: bool, out: &str) -> Vec<String> {
    let pair = |v: String, l: &str| format!("{}{}", k.value(&v), k.label(l));
    let title = if stopped_early {
        k.warn("✓ stopped early")
    } else {
        k.good("✓ done")
    };
    let wall = Duration::try_from_secs_f64(r.wall_secs).unwrap_or_default();
    let errors = r.errors.values().fold(0u64, |a, &n| a.saturating_add(n));
    let files = r.store.warc_files;
    let noun = if files == 1 { "file" } else { "files" };
    vec![
        spread(&title, &k.value(&clock(wall))),
        String::new(),
        format!(
            "  {}   {}   {}   {}{}  {}{}",
            pair(count(r.pages), " pages"),
            pair(rate_text(r.pages_per_sec), "/s"),
            k.value(&bytes(r.bytes)),
            k.label("p50 "),
            k.value(&ms(r.latency_p50_ms)),
            k.label("p99 "),
            k.value(&ms(r.latency_p99_ms)),
        ),
        format!(
            "  {} · {} · {} · {}",
            pair(count(r.frontier.hosts as u64), " hosts"),
            pair(count(r.frontier.dead_hosts as u64), " dead"),
            pair(count(r.store.near_dups), " near-duplicates"),
            pair(count(errors), " errors"),
        ),
        String::new(),
        format!("  {}{}", k.label("out     "), k.value(out)),
        format!(
            "          {}",
            k.label(&format!("pages.jsonl · report.json · {files} WARC {noun}"))
        ),
    ]
}

/// A Markdown table with right-aligned cells. With color the header is highlighted and the
/// rules dimmed, so it reads the same either way and pastes as Markdown.
pub fn table(k: &Paint, head: &[&str], rows: &[Vec<String>]) -> Vec<String> {
    let widths: Vec<usize> = head
        .iter()
        .enumerate()
        .map(|(c, h)| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|s| s.chars().count())
                .fold(h.chars().count(), usize::max)
        })
        .collect();
    let bar = k.label("|");
    let line = |cells: Vec<String>| format!("{bar} {} {bar}", cells.join(&format!(" {bar} ")));
    let mut out = vec![line(
        head.iter()
            .zip(&widths)
            .map(|(h, &w)| k.accent(&format!("{h:>w$}")))
            .collect(),
    )];
    let rule: Vec<String> = widths
        .iter()
        .map(|&w| format!("{}:", "-".repeat(w + 1)))
        .collect();
    out.push(k.label(&format!("|{}|", rule.join("|"))));
    for r in rows {
        let cells = widths.iter().enumerate();
        let cells = cells.map(|(c, &w)| format!("{:>w$}", r.get(c).map_or("", String::as_str)));
        out.push(line(cells.collect()));
    }
    out
}

pub fn error(k: &Paint, msg: &str) -> String {
    format!("{} {msg}", k.bad("error:"))
}

pub fn warning(k: &Paint, msg: &str) -> String {
    format!("{} {msg}", k.warn("warning:"))
}

/// What replaces the `prev` lines drawn last with `lines`: up `prev` lines, clear to the end of
/// the screen, the new lines. Wrapped in synchronized-output marks, so terminals that know
/// them show the frame at once, without flicker; others ignore them.
pub fn redraw(prev: usize, lines: &[String]) -> String {
    let mut s = String::from("\x1b[?2026h");
    if prev > 0 {
        let _ = write!(s, "\x1b[{prev}F");
    }
    s.push_str("\x1b[J");
    for l in lines {
        s.push_str(l);
        s.push('\n');
    }
    s.push_str("\x1b[?2026l");
    s
}

/// Draws frames in place on stderr. While it lives the cursor is hidden and line wrapping is
/// off, so a window narrower than a line clips it instead of breaking the redraw; dropping it
/// erases the last frame and restores both.
pub struct Live {
    drawn: usize,
}

impl Live {
    pub fn start() -> Self {
        emit("\x1b[?25l\x1b[?7l");
        Self { drawn: 0 }
    }

    pub fn draw(&mut self, lines: &[String]) {
        emit(&redraw(self.drawn, lines));
        self.drawn = lines.len();
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        emit(&redraw(self.drawn, &[]));
        emit(RESTORE);
    }
}

/// Writes to stderr, ignoring errors: a closed stderr mustn't stop a crawl.
pub fn emit(s: &str) {
    let _ = std::io::stderr().write_all(s.as_bytes());
}

/// Whether `lines` of the panel fit the window, keeping a row for the cursor. A taller panel
/// would scroll, and the next redraw couldn't reach its top. True when the size is unknown.
pub fn fits(lines: usize) -> bool {
    window().is_none_or(|(cols, rows)| cols >= WIDTH && rows > lines)
}

/// The (columns, rows) of the terminal on stderr.
fn window() -> Option<(usize, usize)> {
    #[cfg(unix)]
    {
        let mut ws = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCGWINSZ only fills in the winsize it is given.
        let ok = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
        (ok && ws.ws_col > 0 && ws.ws_row > 0).then(|| (ws.ws_col.into(), ws.ws_row.into()))
    }
    #[cfg(not(unix))]
    None
}
