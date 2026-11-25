# McCrawl — Sub-project 1: Crawl Core — Design

**Date:** 2026-11-18 · **Status:** approved in brainstorming, awaiting spec review

## 1. Context

McCrawl is a high-performance web crawler, with Rust for the crawl engine and Python for the ML. It targets the hard problems of web-scale crawling: throughput, politeness across thousands of hosts, JS rendering and anti-bot handling, and intelligent crawl prioritization. It is backend only; there is no frontend.

**Constraints**
- Timeline is 3–6 weeks.
- Development and runs happen on a laptop under WSL Ubuntu. Engine throughput is measured against a local mock web; real-web crawls are small and polite.
- The **engine is general**, but the **real crawl, ML and search are specialized** on high-quality *technical knowledge* (developer docs, engineering blogs, technical write-ups).

**Headline results to measure** (every number is measured, never claimed):
- Engine: X pages/s on the synthetic benchmark, at Y% of the politeness ceiling.
- Scheduling: the harvest rate of high-quality technical pages, A% with BFS vs B% with the ML scheduler, under an equal page budget.
- Search: N pages indexed with embeddings for semantic search.

## 2. Project roadmap

Each sub-project gets its own spec → plan → build cycle, in this order:

| # | Sub-project | Lang | Proves |
|---|---|---|---|
| **1** | **Crawl core** (this spec) | Rust | Efficiency, politeness |
| 2 | Distribution: host-hash sharding across workers, disk-backed frontier, checkpoint/resume | Rust | Path to 100M+ pages/day |
| 3 | Focused-crawl ML: score URLs *before* fetch (URL, anchor text, link context) → frontier priority; recrawl scheduling | Python train, Rust serve | Harvest rate vs BFS |
| 4 | Dynamic content: JS-need detection, headless Chrome via CDP, anti-bot signals | Rust | JS rendering |
| 5 | Knowledge layer: quality classifier, embeddings, vector index, search CLI | Python | Search over the crawl |

## 3. Sub-project 1 scope

**In scope:**
- A single-process async crawler with a two-heap frontier and per-host politeness.
- robots.txt handling, redirect handling, link extraction with anchor text, URL and content dedup.
- WARC + JSONL output, metrics, a mock web, benchmarks, tests and a README.

**Out of scope:**
- Distribution and checkpoint/resume (SP2).
- ML priority and recrawl (SP3).
- JS rendering (SP4).
- Per-IP or registered-domain politeness grouping, sitemaps, and a disk-backed frontier (SP2/SP3).

## 4. Architecture

```
 seeds ──► Frontier ───────────────────────────────────────────────────┐
           │ per-host queue (BinaryHeap by priority)                   │
           │ waiting heap: hosts by next_allowed time (politeness gate)│
           │ ready heap:   ready hosts by best URL priority (value)    │
           └──► next() ──► Fetch task ×N (async) ──► complete(host) ──┘ (reschedule: now + delay)
                                │
                                ▼ bounded channel (backpressure)
                          Parse pool (one thread per CPU core)
                          links+anchors, text, title, canonical, meta robots, SimHash
                            │                         │
          new URLs ◄─ normalize ◄─ seen-set ◄─────────┤
          (frontier.push)                             ▼
                                         Store task (single writer): WARC + pages.jsonl
```

There is one process, one tokio multi-threaded runtime and one binary crate.

## 5. Components

| File | Responsibility | Depends on |
|---|---|---|
| `src/main.rs` | CLI (`crawl`, `mockweb`, `bench`), stop signals | `crawler`, `mockweb`, `ui` |
| `src/ui.rs` | Terminal output: Monokai Pro (default) or Catppuccin Mocha; the start banner; a borderless live panel redrawn in place, which becomes a plain line every 5 s when the output isn't a terminal or `NO_COLOR` is set; the end summary; the Markdown bench table | `metrics`, `clap`, `libc` |
| `src/lib.rs` | `pub mod` declarations, so integration tests can run real crawls | all modules |
| `src/crawler.rs` | `CrawlConfig` + `run()`: the scheduler task that owns the frontier, the fetch tasks, the parse pool, the store wiring, graceful shutdown, `report.json` | all modules |
| `src/urlnorm.rs` | `normalize(base, href) -> Option<Url>`: resolve relative links, lowercase scheme/host, drop fragment and default port, reject non-http(s), enforce max length 2048, and reject paths where any segment appears more than 3 times (a trap signature like `/a/b/a/b/a/b/a/`). `host_key(&Url) -> String` (hostname) | `url` |
| `src/frontier.rs` | Pure two-heap frontier and per-host state (see §6): a `&mut self` struct, with time passed in as `now`. `push`, `next(now)`, `complete_page`, `complete_robots` | `urlnorm`, `dedup` (seen-set), `robots` |
| `src/robots.rs` | Fetch, parse and cache robots.txt per host with RFC 9309 status semantics; `allowed(url)`, `crawl_delay()` | `texting_robots`, `fetch` |
| `src/fetch.rs` | Two `reqwest::Client`s (pages: redirects off; robots.txt: ≤ 5 redirects, per RFC 9309): a fresh connection per fetch (reused idle connections hung in real crawls), timeouts (10 s connect / 30 s total), 2 MB body cap, gzip/brotli, `hickory-dns`, **redirects disabled** (the `Location` header becomes a new frontier URL, hop count ≤ 5), Content-Type checked before reading the body. Returns a `FetchResult` or `FetchError` | `reqwest` |
| `src/parse.rs` | `parse(url, body) -> Parsed { title, text, canonical, noindex, nofollow, links: Vec<Link{url, anchor, nofollow}> }`. Single-pass **streaming** parse with `lol_html` element/text handlers (`a[href]` + anchor text, `title`, `link[rel=canonical]`, `meta[name=robots]`, visible text with `script`/`style` skipped). No DOM is built. Boilerplate removal and main-content extraction are left to SP5, which reads the raw HTML from WARC | `lol_html`, `urlnorm` |
| `src/dedup.rs` | Seen-URL `HashSet<u64>` (xxh3 of canonical URL); exact body hash (xxh3); 64-bit SimHash over word 3-shingles; `hamming(a, b)`; near-duplicate if distance ≤ 3. `NearDupIndex` uses Manku et al.'s (Google, 2007) block trick, not a linear scan: split the fingerprint into 4 × 16-bit blocks and keep 4 hash maps from block value to fingerprints. By pigeonhole, any fingerprint within distance ≤ 3 matches one block exactly, so only those buckets are checked | `xxhash-rust` |
| `src/store.rs` | Single writer task. WARC (`warcinfo` + `response` records, each record its own gzip member, compressed by the parse pool so the writer only appends, files rotate at 1 GB) and `pages.jsonl`; flushed whenever its queue runs empty | `flate2`, `serde_json` |
| `src/metrics.rs` | Counters owned by the scheduler (plain fields, no atomics), latency percentiles, CPU time and peak RSS from `/proc`, a `Progress` snapshot for the live display, the final report | `serde` |
| `src/mockweb.rs` | Synthetic web. Hosts `h{i}.mock`, deterministic pages from `hash(host, path)`, configurable latency, robots variants (disallow paths, crawl-delay, 5xx robots), injected 429s, request log `(host, path, time)` | `axum`, `tokio` |

Mock hostnames resolve to 127.0.0.1 through a custom `reqwest` DNS resolver that handles only `*.mock` names.

## 6. Frontier and politeness algorithm

**Per-host state:**
- `queue`: BinaryHeap of (priority, seq, url)
- `next_allowed`: Instant
- `delay`
- `backoff`
- `in_flight`: bool, so at most one request per host at a time
- `consecutive_fails`
- `pages_fetched`
- `robots`: Unknown / Fetching / Ready

**Two heaps, each host present at most once overall:**
- **waiting**: min-heap by `next_allowed`. These hosts have queued URLs but must still wait.
- **ready**: max-heap by the priority of the host's best queued URL. These hosts can be fetched now.

**`next(now)`:**
1. Move every waiting host whose `next_allowed ≤ now` into ready.
2. Pop the best ready host. Take its top URL and mark the host in flight.
3. If ready is empty, return `WaitUntil(earliest next_allowed)`, or `Idle` (requests in flight), or `Done`. The scheduler task sleeps until that time, or until a fetch completes or new links arrive.

**`complete(host, outcome, response_time)`:**
1. Clear `in_flight`.
2. Compute the delay: `delay = max(robots_crawl_delay, min_delay, k × response_time) × backoff`. Defaults: `min_delay` = 1 s, `k` = 5, both configurable.
3. Set `next_allowed = now + delay`, raised further to honor `Retry-After`.
4. If the host's queue is not empty, re-insert it into waiting.

**Outcome rules:**
- **429 / 503:** `backoff` doubles, up to a 10-minute cap.
- **Success:** `backoff` halves, down to 1.
- **5 consecutive failures:** the host is marked dead and its queue is dropped.
- **Per-host page cap reached:** the queue is dropped.

**robots.txt:** A newly seen host's first job is fetching its robots.txt. That request goes through the same politeness gate, and the host's URLs aren't served until the rules are Ready. Disallowed URLs are dropped when popped. If robots.txt is unavailable (5xx or unreachable), the host's URLs stay queued but unfetched, and robots.txt is retried after 60 s and then 120 s. After 3 failed attempts the host is dropped.

**SP1 priority:** `priority = −depth`, so seeds come first and the crawl is breadth-first. This is the baseline sub-project 3 will be compared against.

**Why two heaps:** The waiting heap enforces *when* a host may be contacted (politeness). The ready heap picks *which* of the allowed hosts is most valuable (prioritization). Both operations are O(log hosts). It's a deterministic variant of Mercator's front/back queue design, and an SP3 score replaces `−depth` without other changes.

`ponytail:` The frontier is owned by a single scheduler task (an actor: no locks, no thundering herd), so one core is its ceiling. If the benchmark shows the scheduler saturating, SP2 shards it by `hash(host)`. The frontier is also in memory: about 8 bytes per seen URL plus the queued URLs, which is fine for tens of millions; disk-backed in SP2. Politeness is per hostname, not per IP or registered domain; that comes in SP2.

## 7. Libraries

- **Hand-written** because they're the heart of the design: the frontier and politeness logic, SimHash with the near-dup index, the WARC writer.
- **Crates:** `tokio`, `reqwest` (rustls, gzip, brotli, hickory-dns), `url`, `texting_robots`, `lol_html` (streaming HTML rewriter: one pass, no DOM, low memory), `xxhash-rust` (xxh3, hashes stable across runs and in Python), `serde`, `serde_json`, `flate2` (zlib-rs backend), `clap` (derive), `axum`.
- **Std only:** the seen-set, the parse pool (one thread per core sharing a job queue), logging (`eprintln!`), memory stats (`/proc/self/status`).
- Versions are pinned to current releases when each crate is added; docs are checked at that time.

## 8. Error handling

| Outcome | Action |
|---|---|
| Timeout, connection error (including DNS and TLS, which reqwest doesn't distinguish cleanly), 5xx | Retry ≤ 2× through the frontier (politeness applies); 5 consecutive failures kill the host |
| 429 / 503 | Per-host backoff, honor `Retry-After`; the URL is retried ≤ 2× like other failures, but rate limiting never counts toward killing the host |
| 4xx (not 429) | Permanent, recorded, no retry |
| Non-HTML Content-Type | Skip before downloading the body |
| Body > 2 MB | Truncate, set `truncated: true`, add a `WARC-Truncated` header |
| 5 consecutive host failures | Host dead, queue dropped |

**robots.txt status codes (RFC 9309):**
- 2xx: parse and obey.
- 4xx: allow all.
- 5xx or unreachable: disallow all, and retry robots after a backoff.
- Rules are cached for 24 h.

**Trap defenses:**
- URL length ≤ 2048.
- Repeated path segments are limited.
- At most 5 redirect hops.
- A per-host page cap.

**Rust error types:**
- A std-only `FetchError` enum classifies failures.
- `main` prints `error: …` and exits with 1 (2 for usage errors, 130 for a forced quit).
- A per-URL error is recorded and the crawl continues; it never panics the crawl.

## 9. Lifecycle, CLI and output

```
mccrawl crawl   --seeds seeds/technical.txt --contact <url or email> [--out ~/mccrawl-data/run-001] \
                [--budget 100000] [--concurrency 512] [--per-host-cap 500] [--min-delay-ms 1000]
mccrawl mockweb [--hosts 10000] [--port 8080] [--latency-ms 20..200]
mccrawl bench   [--hosts 10000] [--pages 200000] [--mode polite|raw] [--concurrency 64,256,1024]
every command:  [--theme monokai|mocha]
```

**Stopping:**
- The crawl stops when the budget is reached, the frontier is empty with nothing in flight, or Ctrl-C is pressed.
- On Ctrl-C (or SIGTERM) it stops handing out URLs, waits up to 10 s for in-flight fetches, flushes, then writes `report.json`. A second signal quits at once (exit code 130).

**Crash safety:** Output is append-only. Each WARC record is its own gzip member and each JSONL row is one line, and both are flushed whenever the store's queue runs empty, so a crash leaves all earlier output readable. Output files are created new, never overwritten.

**User agent:** `McCrawl/0.1 (+<contact>)`, where the contact is the required `--contact` flag: an http(s) URL or an email address.

**Output files:**
- `crawl-NNNNN.warc.gz`. Response records store the decoded body, so the stored HTTP header block has no `Content-Encoding` for a decoded body (reqwest removes it when it decodes; one still present is true of the stored bytes and is kept), drops `Transfer-Encoding`, and sets `Content-Length` to the stored body length.
- `pages.jsonl`, one row per fetch: `url, host, depth, parent_url, priority, fetched_at, status, error, content_type, bytes, truncated, title, text, canonical, noindex, links[{url, anchor, nofollow}], simhash (hex), body_hash (hex), near_dup` (`near_dup` is the URL of an earlier page within SimHash distance ≤ 3, or `null`)
- `report.json`: config, wall time, pages/s, bytes, status and error histograms, politeness wait, exact and near-duplicate counts, p50/p99 fetch latency, CPU seconds, peak RSS

**Environment (WSL):**
- Build artifacts go to `~/.cargo-target/mccrawl`, set by a gitignored local `.cargo/config.toml`.
- Crawl output goes to `~/mccrawl-data/`.
- Both keep gigabytes of build artifacts and crawl data out of OneDrive sync and off the slow `/mnt/c` filesystem.

## 10. Testing

No test frameworks: `cargo test` plus one Python check.

- **Per-module tests** (`tests/<module>.rs`, against the public API; `src/` holds no test code):
  - urlnorm: an input → canonical table.
  - frontier: never pops a host before `next_allowed`, at most one request in flight per host, priority order among ready hosts.
  - robots: the RFC 9309 status mapping.
  - dedup: SimHash distance is small for near-identical texts and large for different ones; seen-set behavior.
  - parse: links, anchors and nofollow/noindex from fixture HTML.
  - store: WARC record framing.
- **`tests/politeness.rs`:** Starts an in-process mock web with 1,000 hosts and crawls it. From the request log, with no slack (one process, one clock), it asserts:
  - (a) each request to a host arrives at least the previous response's time, plus the whole delay it earned: max(minimum, Crawl-delay, k × response time), at least Retry-After after a 429, and the robots retry × 2^(n-1) after the n-th robots.txt failure;
  - (b) robots.txt comes first and once per host, and no disallowed path is requested;
  - (c) a host with robots.txt down gets nothing else, and is given up after 3 tries;
  - (d) the server saw exactly the requests the crawler reports, and each rule above decided some gaps, so none passed untested.
- **`tests/warc_check.py`:** Run with `uv run --with warcio`. Reads every WARC record of a crawl with warcio, an independent check that the files are valid. It also checks that the records match `pages.jsonl` and `report.json`.

## 11. Benchmarks

`mccrawl bench` runs the crawler against the in-process mock web: 10k hosts, 20–200 ms latency, pages around 30 KB.

- **Polite mode:** reports pages/s as a **percentage of the politeness ceiling** (`hosts ÷ min_delay`, e.g. 10k pages/s).
- **Raw mode:** delay 0, the engine's own ceiling.
- **Concurrency sweep** at 64, 256 and 1024, which tests the single-lock ceiling.
- **Every run reports** pages/s, p50/p99 latency, CPU and peak RSS.
- **Profiling:** CPU profiles of raw mode with `perf record -e cpu-clock:u` (user time, so no `perf_event_paranoid` change is needed; `samply` works too once it is 1), plus per-thread CPU from `/proc`. Hot paths found there drive the optimizations, and each optimization gets a before/after number in the README.

## 12. Done criteria

1. All tests in `tests/` pass, including `politeness.rs`. `warc_check.py` passes.
2. Benchmark numbers are measured and recorded in a README table.
3. A real crawl of 10–50k pages from `seeds/technical.txt` (committed, curated technical sites) completes politely, and its WARC passes `warc_check.py`.
4. `README.md` covers the architecture diagram, design rationale, benchmarks and how to run it.
