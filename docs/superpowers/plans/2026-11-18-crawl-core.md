# McCrawl Crawl Core — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task by task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build sub-project 1 of McCrawl: a single-process async Rust crawler with a two-heap politeness frontier, robots.txt compliance, streaming link extraction, URL and near-duplicate dedup, and WARC + JSONL output. It comes with a mock web, a politeness proof test, and benchmarks.

**Architecture:** A pure, synchronous `Frontier` data structure is owned by one scheduler task (an actor, no locks). The scheduler spawns async fetch tasks, up to `concurrency` at a time. HTML goes through a bounded channel to a CPU-bound parse pool (`spawn_blocking`, at most one task per core). Parsed records go to a single store thread that writes WARC and `pages.jsonl`. Discovered links flow back to the scheduler.

**Tech stack:** Rust 2024, tokio, reqwest 0.13 (rustls, hickory-dns), lol_html 3, texting_robots, xxhash-rust, flate2, serde, clap, axum (mock web). Python (uv + warcio) for the WARC check.

**Spec:** `docs/superpowers/specs/2026-11-18-crawl-core-design.md`

## Context

The spec is approved. This plan turns it into 15 tasks, built one file at a time and one function at a time. In each task the tests come first, then each function is implemented and explained as it's written. Execution is inline (not subagents), with a short checkpoint after each task.

**Refinements to the spec** (Task 1 syncs the spec with these):
1. **The frontier is owned by a scheduler task instead of sitting behind a `Mutex`.** The `Frontier` is a plain `&mut self` struct that takes `now: Instant` as an argument. That gives no lock contention, no thundering herd of 512 tasks waking on one `Notify`, and fully deterministic unit tests that need no sleeping. The ceiling is the same as the spec's (one core runs the scheduler). Sub-project 2 shards it by `hash(host)`.
2. **`src/lib.rs` and `src/crawler.rs` are added**, so integration tests can run a real crawl. `main.rs` only handles the CLI.
3. **DNS and TLS failures are grouped with connect errors** (retried up to 2×). reqwest doesn't expose a clean way to tell them apart. The 5-consecutive-failures rule still kills dead hosts quickly.
4. **WARC HTTP headers are corrected.** reqwest returns decoded bodies, so the stored header block has no `Content-Encoding` for a decoded body (reqwest removes it itself; one it didn't decode stays, since it is true of the stored bytes), drops `Transfer-Encoding`, and sets `Content-Length` to the stored body length.
5. **robots.txt 5xx or unreachable:** the host's URLs stay queued and are not fetched. robots is retried after `robots_retry` × 2^(attempt−1). After 3 failed attempts the host is dropped.

## Global constraints

- **Commands run in WSL at the project root:** `wsl -d Ubuntu -e bash -lc "cd /mnt/c/Users/iidab/OneDrive/Desktop/McCrawl && <cmd>"`. Below, only `<cmd>` is shown.
- Build artifacts go to `$HOME/.cargo-target/mccrawl`, via a gitignored `.cargo/config.toml`. Crawl output goes under `$HOME/mccrawl-data/`, never inside the repo.
- **Crate pins:**
  - `tokio 1.54.1`
  - `reqwest 0.13.5` (`default-features = false`, features `rustls, http2, gzip, brotli, zstd, hickory-dns`)
  - `url 2.5.8`
  - `texting_robots 0.2.2`
  - `lol_html 3.0.3`
  - `xxhash-rust 0.8.21` (`xxh3`)
  - `serde 1.0.229` (`derive`)
  - `serde_json 1.0.154`
  - `flate2 1.1.11`
  - `clap 4.6.8` (`derive`)
  - `axum 0.8.9`
- **Limits:**
  - Timeouts: connect 10 s, total 30 s.
  - URLs: length ≤ 2048, path-segment repeats ≤ 3, redirect hops ≤ 5, retries ≤ 2.
  - Body cap 2 MiB.
  - Backoff: doubles up to a 10 min cap. 5 consecutive host failures → host dead.
  - robots: TTL 24 h, 3 attempts.
  - Delay defaults: `min_delay` 1 s, `delay_factor` k = 5.
  - Dedup: near-duplicate if Hamming distance ≤ 3.
  - WARC files rotate at 1 GiB. Shutdown grace period 10 s.
- **Identity:** User-Agent is `McCrawl/0.1 (+<contact>)`. The contact is a required CLI flag for real crawls. The robots agent token is `mccrawl`.
- **Errors:** `FetchError` is a std-only enum. Binaries and `run` return `Result<_, Box<dyn Error (+ Send + Sync)>>`. A per-URL error never panics the crawl.
- **Comments and tests:** Known ceilings are marked with `ponytail:` comments (ceiling plus upgrade path). Tests live in `tests/<module>.rs`, one integration-test file per module, using the public API only; `src/` holds no test code. The test blocks below are written as `mod tests { use super::*; … }`: when copying one, drop the wrapper and import from `mccrawl::<module>`. "Run `cargo test X`" means `cargo test --test X`. The listed tests are the minimum: each task adds edge-case tests (hostile input, limits, overflow, failure and recovery), and core rules get a mutation check (break the code, confirm a test fails, restore). No test frameworks beyond `cargo test` and tokio's test macros.
- **TDD loop per task:** write signatures with `todo!()` bodies plus the tests → run (FAIL: `not yet implemented`) → implement function by function → run (PASS) → update `STATE.md` and the README (status line + mark the task's box `:::done` in the build-order diagram) → commit.
- **Commits:** every commit message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Nothing is pushed.

## File map

| File | Responsibility | Task |
|---|---|---|
| `src/lib.rs` | `pub mod` declarations only | 1 |
| `src/urlnorm.rs` | URL canonicalization, host key | 1 |
| `src/dedup.rs` | Seen-set, body hash, SimHash, Hamming distance, `NearDupIndex` | 2 |
| `src/robots.rs` | `RobotsRules` (RFC 9309 status semantics), `robots_url` | 3 |
| `src/frontier.rs` | Two-heap frontier, host state machine, politeness timing | 4–5 |
| `src/parse.rs` | Streaming lol_html extraction | 6 |
| `src/mockweb.rs` | Deterministic synthetic web and request log | 7 |
| `src/fetch.rs` | reqwest clients, classified fetch results | 8 |
| `src/store.rs` | WARC writer, JSONL, near-dup marking, store thread | 9 |
| `src/metrics.rs` | Tally, percentiles, `/proc` readers, `Report` | 10 |
| `src/crawler.rs` | `CrawlConfig`, `run()`: scheduler actor, fetch tasks, parse pool, wiring | 11 |
| `src/main.rs` | CLI: `crawl`, `mockweb`, `bench` | 12 |
| `tests/politeness.rs` | Proof of zero politeness violations | 13 |
| `tests/warc_check.py` | Independent WARC validation (warcio) | 13 |
| `seeds/technical.txt` | Curated technical seed list | 15 |
| `README.md` | Diagrams (pipeline, host state machine, roadmap, build order), rationale, benchmarks, usage | created early; progress per task; finished in 15 |

---

### Task 1: Environment, crate layout, `urlnorm.rs`

**Files:** Create `src/lib.rs`, `src/urlnorm.rs`. Modify `Cargo.toml`, the spec, `STATE.md`, `.gitignore`. Create gitignored `.cargo/config.toml` (`target-dir`). Delete the Windows-built `target/` in the repo folder (build artifacts only).

**Produces:**
- `pub const MAX_URL_LEN: usize = 2048;`
- `pub fn normalize(base: &Url, href: &str) -> Option<Url>`
- `pub fn host_key(url: &Url) -> Option<&str>`

- [ ] **Step 1: Environment.**
  - Create `.cargo/config.toml` with `[build] target-dir = "/home/<user>/.cargo-target/mccrawl"` and gitignore `/.cargo/`. (The WSL dotfiles are managed by Nix home-manager and are read-only, so a project-local config is used instead of `~/.profile`.) Verify that `cargo build` leaves no `./target`.
  - Remove `./target`.
  - Run `cargo add url@2.5.8`.
- [ ] **Step 2: Sync the spec** with Refinements 1–5 above (§5 table, §6 Mutex note, §8 errors). Then grep the repo for personal or company references before committing.
- [ ] **Step 3: Create `src/lib.rs`** containing `pub mod urlnorm;` (later tasks add one line each). Add signatures with `todo!()` and these tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn n(base: &str, href: &str) -> Option<String> {
        normalize(&Url::parse(base).unwrap(), href).map(|u| u.to_string())
    }
    #[test]
    fn canonicalizes() {
        let b = "https://Example.com/a/b.html";
        assert_eq!(n(b, "c.html").as_deref(), Some("https://example.com/a/c.html"));
        assert_eq!(n(b, "../x?q=1#frag").as_deref(), Some("https://example.com/x?q=1"));
        assert_eq!(n(b, "HTTP://EXAMPLE.com:80/").as_deref(), Some("http://example.com/"));
        assert_eq!(n(b, "https://example.com:443/p?").as_deref(), Some("https://example.com/p"));
        assert_eq!(n(b, "  /trim  ").as_deref(), Some("https://example.com/trim"));
    }
    #[test]
    fn rejects() {
        let b = "https://example.com/";
        for bad in ["mailto:a@b.c", "javascript:void(0)", "ftp://x.com/", "", "#top", "   "] {
            assert_eq!(n(b, bad), None, "{bad:?}");
        }
        assert_eq!(n(b, &format!("/{}", "a".repeat(2100))), None);
        assert_eq!(n(b, "/a/b/a/b/a/b/a/b"), None); // "a" appears 4 times
        assert!(n(b, "/a/b/a/b/a/b").is_some()); // 3 times is fine
    }
    #[test]
    fn host_key_is_lowercase_hostname_without_port() {
        let u = Url::parse("https://Docs.Rust-lang.org:8443/x").unwrap();
        assert_eq!(host_key(&u), Some("docs.rust-lang.org"));
    }
}
```

- [ ] **Step 4:** Run `cargo test urlnorm`. Expect FAIL (`not yet implemented`).
- [ ] **Step 5: Implement `normalize`**:
  - trim; empty or starting with `#` → `None`;
  - `base.join(href).ok()?`; the scheme must be `http` or `https`;
  - `set_fragment(None)`; if the query is `Some("")`, `set_query(None)`;
  - length ≤ `MAX_URL_LEN`;
  - count path segments in a small `HashMap<&str, u8>`; any count > 3 → `None`.

  `url` already lowercases the host, strips default ports and resolves dot segments; that's why the WHATWG parser was chosen. `host_key` = `url.host_str()`.
- [ ] **Step 6:** Run `cargo test urlnorm`. Expect PASS. Run `cargo clippy --all-targets -- -D warnings`.
- [ ] **Step 7:** Update `STATE.md` (`lib.rs`, `urlnorm.rs`). Commit: `feat(urlnorm): URL canonicalization and host key`.

### Task 2: `dedup.rs`

**Files:** Create `src/dedup.rs`; add `pub mod dedup;`. Run `cargo add xxhash-rust@0.8.21 --features xxh3`.

**Consumes:** `url::Url`.

**Produces:**
- `pub fn url_hash(url: &Url) -> u64`
- `pub fn body_hash(body: &[u8]) -> u64`
- `pub fn simhash(text: &str) -> u64`
- `pub fn hamming(a: u64, b: u64) -> u32`
- `#[derive(Default)] pub struct SeenSet` with `insert(&mut self, &Url) -> bool` (true if new) and `len(&self) -> usize`
- `#[derive(Default)] pub struct NearDupIndex` with `find(&self, fp: u64) -> Option<u32>` and `insert(&mut self, fp: u64, id: u32)`
- `pub const NEAR_DUP_BITS: u32 = 3;`

- [ ] **Step 1: Signatures, `todo!()` bodies, and tests:**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn words(n: usize, prefix: &str) -> Vec<String> {
        (0..n).map(|i| format!("{prefix}{}", (i * 7919) % 1009)).collect()
    }
    #[test]
    fn seen_set_dedups() {
        let mut s = SeenSet::default();
        let u = Url::parse("https://a.com/x").unwrap();
        assert!(s.insert(&u));
        assert!(!s.insert(&u));
        assert_eq!(s.len(), 1);
    }
    #[test]
    fn simhash_near_and_far() {
        let w = words(2000, "w");
        let a = w.join(" ");
        let mut w2 = w.clone();
        w2[1000] = "changed".into();
        let b = w2.join(" ");
        let c = words(2000, "z").join(" ");
        assert_eq!(simhash(&a), simhash(&a));
        assert!(hamming(simhash(&a), simhash(&b)) <= NEAR_DUP_BITS);
        assert!(hamming(simhash(&a), simhash(&c)) > 10);
        assert_eq!(simhash(""), 0);
    }
    #[test]
    fn near_dup_index_uses_blocks_then_verifies_distance() {
        let mut idx = NearDupIndex::default();
        let fp = 0xDEAD_BEEF_1234_5678u64;
        idx.insert(fp, 7);
        assert_eq!(idx.find(fp), Some(7));
        assert_eq!(idx.find(fp ^ (1 | 1 << 20 | 1 << 40)), Some(7)); // 3 bits, 3 different blocks
        assert_eq!(idx.find(fp ^ 0b1111), None); // 4 bits: candidate found, then rejected by distance
        assert_eq!(idx.find(!fp), None);
    }
}
```

- [ ] **Step 2:** Run `cargo test dedup`. Expect FAIL.
- [ ] **Step 3: Implement.**
  - `url_hash`/`body_hash` = `xxh3_64`.
  - `SeenSet` wraps `HashSet<u64>`. `// ponytail: 8 B/URL in RAM; Bloom filter or disk set in SP2.`
  - `hamming` = `(a ^ b).count_ones()`.
  - `simhash`, 64-bit, over word 3-shingles:

```rust
pub fn simhash(text: &str) -> u64 {
    let words: Vec<u64> = text.split_whitespace().map(|w| xxh3_64(w.to_lowercase().as_bytes())).collect();
    if words.is_empty() { return 0; }
    let mut votes = [0i32; 64];
    for win in words.windows(3.min(words.len())) {
        let mut buf = [0u8; 24];
        for (i, h) in win.iter().enumerate() { buf[i * 8..i * 8 + 8].copy_from_slice(&h.to_le_bytes()); }
        let h = xxh3_64(&buf[..win.len() * 8]);
        for (bit, v) in votes.iter_mut().enumerate() { *v += if h >> bit & 1 == 1 { 1 } else { -1 }; }
    }
    votes.iter().enumerate().fold(0, |fp, (bit, &v)| if v > 0 { fp | 1 << bit } else { fp })
}
```

  - `NearDupIndex` (Manku et al., 2007). With 4 × 16-bit blocks, by pigeonhole any fingerprint within distance 3 matches at least one block exactly:

```rust
#[derive(Default)]
pub struct NearDupIndex { tables: [HashMap<u16, Vec<(u64, u32)>>; 4] }
impl NearDupIndex {
    fn block(fp: u64, i: usize) -> u16 { (fp >> (i * 16)) as u16 }
    pub fn find(&self, fp: u64) -> Option<u32> {
        (0..4).find_map(|i| {
            self.tables[i].get(&Self::block(fp, i))?
                .iter().find(|(c, _)| hamming(*c, fp) <= NEAR_DUP_BITS).map(|&(_, id)| id)
        })
    }
    pub fn insert(&mut self, fp: u64, id: u32) {
        for i in 0..4 { self.tables[i].entry(Self::block(fp, i)).or_default().push((fp, id)); }
    }
}
```

- [ ] **Step 4:** Run `cargo test dedup`. Expect PASS. The fixture is deterministic: if the near-pair distance lands above 3, the fixture is the problem (use a longer text), not the algorithm.
- [ ] **Step 5:** Update `STATE.md`. Commit: `feat(dedup): seen-set, SimHash and Manku block index`.

### Task 3: `robots.rs`

**Files:** Create `src/robots.rs`; add `pub mod robots;`. Run `cargo add texting_robots@0.2.2`.

**Produces:**
- `pub enum RobotsRules { AllowAll, Parsed(texting_robots::Robot), Unreachable }`
- `RobotsRules::from_response(status: u16, body: &[u8], agent: &str) -> Self`
- `allowed(&self, url: &Url) -> bool`
- `crawl_delay(&self) -> Option<Duration>`
- `pub fn robots_url(url: &Url) -> Url`

- [ ] **Step 1: Tests:**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn u(s: &str) -> Url { Url::parse(s).unwrap() }
    #[test]
    fn rfc9309_status_semantics() {
        let txt = b"User-agent: *\nDisallow: /private\nCrawl-delay: 2\n";
        let r = RobotsRules::from_response(200, txt, "mccrawl");
        assert!(r.allowed(&u("https://a.com/ok")));
        assert!(!r.allowed(&u("https://a.com/private/x")));
        assert_eq!(r.crawl_delay(), Some(Duration::from_secs(2)));
        assert!(RobotsRules::from_response(404, b"", "mccrawl").allowed(&u("https://a.com/private")));
        let down = RobotsRules::from_response(503, b"", "mccrawl");
        assert!(matches!(down, RobotsRules::Unreachable));
        assert!(!down.allowed(&u("https://a.com/")));
    }
    #[test]
    fn agent_specific_group_wins() {
        let txt = b"User-agent: mccrawl\nDisallow: /\n\nUser-agent: *\nAllow: /\n";
        assert!(!RobotsRules::from_response(200, txt, "mccrawl").allowed(&u("https://a.com/x")));
    }
    #[test]
    fn robots_url_keeps_scheme_host_port() {
        assert_eq!(robots_url(&u("http://h1.mock:8080/p/1?x=1")).as_str(), "http://h1.mock:8080/robots.txt");
    }
}
```

- [ ] **Step 2:** Run `cargo test robots`. Expect FAIL.
- [ ] **Step 3: Implement.**
  - **`from_response`:** 2xx → `Robot::new(agent, body)`, falling back to `AllowAll` on a parse error. 4xx except 429 → `AllowAll`. 429, 5xx and anything else → `Unreachable`.
  - **`allowed`:** `Parsed(r)` → `r.allowed(url.as_str())`; `AllowAll` → true; `Unreachable` → false.
  - **`crawl_delay`:** `Duration::try_from_secs_f32` (texting_robots accepts `inf`/`1e30`, which would make `from_secs_f32` panic), capped at `MAX_CRAWL_DELAY` = 24 h. Covered by the test `hostile_crawl_delay_never_panics_and_is_capped`.
  - **`robots_url`:** clone, `set_path("/robots.txt")`, `set_query(None)`, `set_fragment(None)`.
- [ ] **Step 4:** Run `cargo test robots`. Expect PASS.
- [ ] **Step 5:** Update `STATE.md`. Commit: `feat(robots): RFC 9309 rules and robots URL`.

### Task 4: `frontier.rs` — two heaps, priority, politeness timing

**Files:** Create `src/frontier.rs`; add `pub mod frontier;`.

**Consumes:** `urlnorm::host_key`, `dedup::SeenSet`, `robots::{RobotsRules, robots_url}`.

**Produces** (all pure: time is passed in, never read inside):

```rust
pub type HostId = u32;
#[derive(Clone, Debug)]
pub struct UrlEntry { pub url: Url, pub depth: u16, pub priority: i32, pub parent: Option<Arc<str>>, pub retries: u8, pub redirect_hops: u8 }
impl UrlEntry {
    pub fn seed(url: Url) -> Self;                                   // depth 0, priority 0
    pub fn child(url: Url, parent: &UrlEntry, parent_url: Arc<str>) -> Self; // depth+1, priority = -(depth as i32)
}
#[derive(Debug)] pub enum Job { Robots { host: HostId, url: Url }, Page { host: HostId, entry: UrlEntry } }
#[derive(Debug)] pub enum Next { Job(Job), WaitUntil(Instant), Idle, Done }
#[derive(Debug, Clone, Copy)]
pub enum Outcome { Ok { elapsed: Duration }, RateLimited { elapsed: Duration, retry_after: Option<Duration> }, ServerError { elapsed: Duration }, NetworkError }
#[derive(Debug, Clone)]
pub struct FrontierConfig { pub min_delay: Duration, pub delay_factor: f64, pub per_host_cap: u32, pub max_backoff: Duration, pub max_host_failures: u32, pub robots_retry: Duration, pub max_robots_attempts: u32 }
#[derive(Debug, Clone, Default)]
pub struct FrontierStats { pub hosts: usize, pub dead_hosts: usize, pub queued: usize, pub dropped_disallowed: u64, pub dropped_cap: u64, pub dropped_dead: u64 }
impl Frontier {
    pub fn new(cfg: FrontierConfig) -> Self;
    pub fn push(&mut self, entry: UrlEntry, now: Instant) -> bool;    // false = already seen / no host
    pub fn requeue(&mut self, entry: UrlEntry, now: Instant);         // retries: bypass the seen-set
    pub fn next(&mut self, now: Instant) -> Next;
    pub fn is_done(&self) -> bool;                                    // nothing scheduled and nothing in flight (pure check)
    pub fn complete_page(&mut self, host: HostId, outcome: Outcome, now: Instant);
    pub fn complete_robots(&mut self, host: HostId, rules: RobotsRules, elapsed: Duration, now: Instant);
    pub fn host_name(&self, host: HostId) -> &str;
    pub fn stats(&self) -> FrontierStats;
}
```

- [ ] **Step 1: Tests**, all using a fixed `t0` and explicit offsets, with no sleeps:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn s(n: u64) -> Duration { Duration::from_secs(n) }
    fn ms(n: u64) -> Duration { Duration::from_millis(n) }
    fn cfg() -> FrontierConfig {
        FrontierConfig { min_delay: s(1), delay_factor: 0.0, per_host_cap: 100, max_backoff: s(600),
                         max_host_failures: 5, robots_retry: s(60), max_robots_attempts: 3 }
    }
    fn e(u: &str, depth: u16) -> UrlEntry {
        UrlEntry { depth, priority: -(depth as i32), ..UrlEntry::seed(Url::parse(u).unwrap()) }
    }
    fn job(f: &mut Frontier, now: Instant) -> Job {
        match f.next(now) { Next::Job(j) => j, other => panic!("expected job, got {other:?}") }
    }
    fn page_url(j: Job) -> String { match j { Job::Page { entry, .. } => entry.url.to_string(), j => panic!("{j:?}") } }
    /// Take the next robots job, complete it with AllowAll at time `t`, return its host id.
    fn allow(f: &mut Frontier, t: Instant) -> HostId {
        let Job::Robots { host, .. } = job(f, t) else { panic!() };
        f.complete_robots(host, RobotsRules::AllowAll, ms(0), t);
        host
    }

    #[test]
    fn robots_first_then_page_after_delay() {
        let t0 = Instant::now();
        let mut f = Frontier::new(cfg());
        assert!(f.push(e("https://a.com/1", 0), t0));
        let Job::Robots { host, url } = job(&mut f, t0) else { panic!() };
        assert_eq!(url.as_str(), "https://a.com/robots.txt");
        assert!(matches!(f.next(t0), Next::Idle));
        f.complete_robots(host, RobotsRules::AllowAll, ms(10), t0);
        assert!(matches!(f.next(t0), Next::WaitUntil(t) if t == t0 + s(1)));
        assert_eq!(page_url(job(&mut f, t0 + s(1))), "https://a.com/1");
    }
    #[test]
    fn one_in_flight_per_host_and_delay_between_requests() {
        let t0 = Instant::now();
        let mut f = Frontier::new(cfg());
        f.push(e("https://a.com/1", 0), t0);
        f.push(e("https://a.com/2", 0), t0);
        let h = allow(&mut f, t0);
        let t1 = t0 + s(1);
        job(&mut f, t1);
        assert!(matches!(f.next(t1), Next::Idle));
        f.complete_page(h, Outcome::Ok { elapsed: ms(10) }, t1);
        assert!(matches!(f.next(t1 + ms(999)), Next::WaitUntil(t) if t == t1 + s(1)));
        assert_eq!(page_url(job(&mut f, t1 + s(1))), "https://a.com/2");
    }
    #[test]
    fn ready_hosts_are_served_best_priority_first() {
        let t0 = Instant::now();
        let mut f = Frontier::new(cfg());
        f.push(e("https://a.com/deep", 3), t0);
        f.push(e("https://b.com/root", 0), t0);
        allow(&mut f, t0); // b.com (better priority) gets its robots fetched first
        allow(&mut f, t0);
        assert_eq!(page_url(job(&mut f, t0 + s(1))), "https://b.com/root");
        assert_eq!(page_url(job(&mut f, t0 + s(1))), "https://a.com/deep");
    }
    #[test]
    fn dedups_but_requeue_bypasses() {
        let t0 = Instant::now();
        let mut f = Frontier::new(cfg());
        assert!(f.push(e("https://a.com/1", 0), t0));
        assert!(!f.push(e("https://a.com/1", 0), t0));
        f.requeue(e("https://a.com/1", 0), t0);
        assert_eq!(f.stats().queued, 2);
    }
    #[test]
    fn adaptive_delay_follows_response_time() {
        let t0 = Instant::now();
        let mut f = Frontier::new(FrontierConfig { delay_factor: 5.0, ..cfg() });
        f.push(e("https://a.com/1", 0), t0);
        f.push(e("https://a.com/2", 0), t0);
        let h = allow(&mut f, t0);
        job(&mut f, t0 + s(1));
        f.complete_page(h, Outcome::Ok { elapsed: ms(400) }, t0 + s(2));
        assert!(matches!(f.next(t0 + s(2)), Next::WaitUntil(t) if t == t0 + s(4))); // 5 × 400 ms = 2 s > min 1 s
    }
    #[test]
    fn rate_limit_backoff_doubles_halves_and_honors_retry_after() {
        let t0 = Instant::now();
        let mut f = Frontier::new(cfg());
        for i in 0..5 { f.push(e(&format!("https://a.com/{i}"), 0), t0); }
        let h = allow(&mut f, t0);
        let mut t = t0 + s(1);
        job(&mut f, t);
        f.complete_page(h, Outcome::RateLimited { elapsed: ms(0), retry_after: Some(s(30)) }, t);
        assert!(matches!(f.next(t), Next::WaitUntil(x) if x == t + s(30))); // backoff 2 → 2 s, Retry-After wins
        t += s(30);
        job(&mut f, t);
        f.complete_page(h, Outcome::RateLimited { elapsed: ms(0), retry_after: None }, t);
        assert!(matches!(f.next(t), Next::WaitUntil(x) if x == t + s(4))); // backoff 4
        t += s(4);
        job(&mut f, t);
        f.complete_page(h, Outcome::Ok { elapsed: ms(0) }, t);
        assert!(matches!(f.next(t), Next::WaitUntil(x) if x == t + s(2))); // backoff halves to 2
    }
    #[test]
    fn empty_frontier_is_done() {
        let mut f = Frontier::new(cfg());
        assert!(f.is_done());
        assert!(matches!(f.next(Instant::now()), Next::Done));
        f.push(e("https://a.com/1", 0), Instant::now());
        assert!(!f.is_done());
    }
}
```

- [ ] **Step 2:** Run `cargo test frontier`. Expect FAIL.
- [ ] **Step 3: Implement**, with the following internals.

  **Internals:**
  - `Queued { priority, seq, entry }` is ordered by `(priority, Reverse(seq))`: highest priority first, FIFO among equals.
  - `Host { name, robots_url, queue: BinaryHeap<Queued>, robots: Option<(RobotsRules, Instant)>, robots_attempts, next_allowed, backoff: u32, in_flight, scheduled, failures, fetched, dead }` (`None` = not fetched yet, or unreachable). The in-flight flag already covers "fetching".
  - `Frontier { cfg, hosts: Vec<Host>, ids: HashMap<String, HostId>, waiting: BinaryHeap<Reverse<(Instant, HostId)>>, ready: BinaryHeap<(i32, Reverse<u64>, HostId)>, seen: SeenSet, seq: u64, in_flight: usize, stats }`
  - **Invariant:** each host is in at most one of {waiting, ready, in flight}. The `scheduled` flag tracks heap membership.

  **Algorithm:**
  - `enqueue`: look up or create the `Host`. A new host starts with `next_allowed = now`, `backoff = 1`, and `robots_url(&entry.url)`. Dead host → `stats.dropped_dead += 1`. Otherwise push a `Queued` with `seq += 1`, then call `schedule(id)`.
  - `schedule(id)`: if the host is not dead, not in flight, not scheduled and has a non-empty queue → set `scheduled = true` and push `Reverse((next_allowed, id))` onto waiting.
  - `next(now)`:
    1. Promote every waiting host with `t ≤ now` to ready, keyed by its queue's top priority. `seq += 1` breaks ties.
    2. Pop from ready and call `dispatch(id, now)` until one returns a job; then `in_flight += 1` and return `Next::Job`.
    3. Otherwise return `WaitUntil(earliest waiting)`, else `Idle` if `in_flight > 0`, else `Done`.
  - `dispatch(id, now)`:
    1. Set `scheduled = false`; a dead host → `None`.
    2. If robots is `None` or older than `ROBOTS_TTL` (24 h): set `in_flight = true`, and return a `Job::Robots`.
    3. Otherwise pop entries. If `fetched ≥ per_host_cap`, drop the whole queue (`dropped_cap`) and return `None`. Skip and count URLs that robots disallows. On the first allowed URL: `in_flight = true`, `fetched += 1`, return a `Job::Page`.
  - `complete_page` (this is the politeness formula):

```rust
pub fn complete_page(&mut self, id: HostId, outcome: Outcome, now: Instant) {
    self.in_flight -= 1;
    let cfg = &self.cfg;
    let h = &mut self.hosts[id as usize];
    h.in_flight = false;
    let crawl_delay = h.robots.as_ref().and_then(|(r, _)| r.crawl_delay()).unwrap_or_default();
    let (elapsed, retry_after) = match outcome {
        Outcome::Ok { elapsed } => { h.failures = 0; h.backoff = (h.backoff / 2).max(1); (elapsed, None) }
        Outcome::RateLimited { elapsed, retry_after } => { h.backoff = (h.backoff * 2).min(1024); (elapsed, retry_after) }
        Outcome::ServerError { elapsed } => { h.failures += 1; (elapsed, None) }
        Outcome::NetworkError => { h.failures += 1; (Duration::ZERO, None) }
    };
    if h.failures >= cfg.max_host_failures && !h.dead {
        h.dead = true;
        self.stats.dead_hosts += 1;
        self.stats.dropped_dead += h.queue.len() as u64;
        h.queue.clear();
    }
    let base = crawl_delay.max(cfg.min_delay).max(elapsed.mul_f64(cfg.delay_factor));
    let delay = base.max((base * h.backoff).min(cfg.max_backoff)); // cap only the backoff's extra wait, never the site's Crawl-delay
    h.next_allowed = now + delay.max(retry_after.unwrap_or_default());
    self.schedule(id);
}
```

  - `complete_robots`:
    - Decrement `in_flight` and clear the host's `in_flight`.
    - **`Unreachable`:** `robots_attempts += 1`, robots back to `None`. If `attempts ≥ max_robots_attempts`, mark the host dead and drop its queue. Otherwise set `next_allowed = now + robots_retry × 2^(attempts−1)`.
    - **Anything else:** reset attempts, set `Ready(rules, now)`, set `next_allowed = now + max(crawl_delay, min_delay, k × elapsed)`.
    - Then `schedule(id)`.
  - Add `// ponytail:` comments:
    - The ready-heap priority is a snapshot taken at promotion time. A higher-priority URL pushed while the host sits in ready doesn't reorder it.
    - One scheduler core is the ceiling; SP2 shards by `hash(host)`.
- [ ] **Step 4:** Run `cargo test frontier`. Expect PASS.
- [ ] **Step 5:** Update `STATE.md`. Commit: `feat(frontier): two-heap politeness frontier`.

### Task 5: `frontier.rs` — robots, caps, failures

**Files:** Modify `src/frontier.rs` (tests, plus fixes the new tests expose).

- [ ] **Step 1: Add tests:**

```rust
#[test]
fn crawl_delay_from_robots() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    f.push(e("https://a.com/1", 0), t0);
    let Job::Robots { host, .. } = job(&mut f, t0) else { panic!() };
    f.complete_robots(host, RobotsRules::from_response(200, b"User-agent: *\nCrawl-delay: 3\n", "mccrawl"), ms(0), t0);
    assert!(matches!(f.next(t0), Next::WaitUntil(t) if t == t0 + s(3)));
}
#[test]
fn disallowed_urls_are_dropped() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    f.push(e("https://a.com/private/x", 0), t0);
    f.push(e("https://a.com/ok", 1), t0);
    let Job::Robots { host, .. } = job(&mut f, t0) else { panic!() };
    f.complete_robots(host, RobotsRules::from_response(200, b"User-agent: *\nDisallow: /private\n", "mccrawl"), ms(0), t0);
    assert_eq!(page_url(job(&mut f, t0 + s(1))), "https://a.com/ok");
    assert_eq!(f.stats().dropped_disallowed, 1);
}
#[test]
fn per_host_cap_drops_the_rest() {
    let t0 = Instant::now();
    let mut f = Frontier::new(FrontierConfig { per_host_cap: 2, ..cfg() });
    for i in 0..3 { f.push(e(&format!("https://a.com/{i}"), 0), t0); }
    let h = allow(&mut f, t0);
    let mut t = t0;
    for _ in 0..2 {
        t += s(1);
        job(&mut f, t);
        f.complete_page(h, Outcome::Ok { elapsed: ms(0) }, t);
    }
    assert!(matches!(f.next(t + s(1)), Next::Done));
    assert_eq!(f.stats().dropped_cap, 1);
}
#[test]
fn host_dies_after_consecutive_failures() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    for i in 0..10 { f.push(e(&format!("https://a.com/{i}"), 0), t0); }
    let h = allow(&mut f, t0);
    let mut t = t0;
    for _ in 0..5 {
        t += s(1);
        job(&mut f, t);
        f.complete_page(h, Outcome::NetworkError, t);
    }
    assert!(matches!(f.next(t + s(1)), Next::Done));
    assert_eq!(f.stats().dead_hosts, 1);
    assert_eq!(f.stats().dropped_dead, 5);
}
#[test]
fn robots_unavailable_retries_with_backoff_then_gives_up() {
    let t0 = Instant::now();
    let mut f = Frontier::new(cfg());
    f.push(e("https://a.com/1", 0), t0);
    let mut t = t0;
    for wait in [60, 120] {
        let Job::Robots { host, .. } = job(&mut f, t) else { panic!("expected robots") };
        f.complete_robots(host, RobotsRules::Unreachable, ms(0), t);
        assert!(matches!(f.next(t), Next::WaitUntil(x) if x == t + s(wait)));
        t += s(wait);
    }
    let Job::Robots { host, .. } = job(&mut f, t) else { panic!() };
    f.complete_robots(host, RobotsRules::Unreachable, ms(0), t);
    assert!(matches!(f.next(t), Next::Done)); // no Page job was ever issued
}
```

- [ ] **Step 2:** Run `cargo test frontier`. Fix whatever fails. These tests target the Task 4 logic, so expect small fixes only.
- **Review of Tasks 1–3 (done after Task 5):** edge-case tests and mutation checks. Fixes:
  - `urlnorm` also strips credentials and trailing host dots, and applies RFC 3986 percent normalization (idempotent).
  - `robots` retries a file texting_robots rejects (an oversized `$` rule) with the `$` anchors stripped, instead of allowing everything.
  - `dedup` gets golden values, which a Task 14 optimization must keep.
- **Built beyond the plan:**
  - 11 edge-case tests: a URL pushed mid-download, priority and FIFO within a host, one slow host not blocking others, requeue, failure-count reset, dead-host pushes, robots recovery, robots refetch at 24 h, the Retry-After cap, hostless URLs, child entries.
  - The randomized model check `random_operations_never_break_politeness_or_lose_urls` (25 seeds × 5,000 operations; URL conservation checked).
  - 12 mutations, all caught.
- [ ] **Step 3:** Run `cargo test` and `cargo clippy --all-targets -- -D warnings`.
- [ ] **Step 4:** Commit: `test(frontier): robots, caps and dead-host behaviour`.

### Task 6: `parse.rs` — streaming extraction

**Files:** Create `src/parse.rs`; add `pub mod parse;`. Run `cargo add lol_html@3.0.3`.

**Consumes:** `urlnorm::normalize`.

**Produces:**
- `pub struct Link { pub url: Url, pub anchor: String, pub nofollow: bool }`
- `pub struct Parsed { pub title: String, pub text: String, pub canonical: Option<Url>, pub noindex: bool, pub nofollow: bool, pub links: Vec<Link> }`
- `pub fn parse(page_url: &Url, html: &[u8]) -> Parsed`

- [ ] **Step 1: Tests:**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    const HTML: &str = r#"<html><head><title>Rust &amp; Crawlers</title>
<base href="https://example.com/docs/">
<link rel="canonical" href="/docs/page">
<meta name="robots" content="noindex">
<script>var x = "<a href='/nope'>";</script><style>.a{}</style></head>
<body><h1>Hello</h1><p>World of <b>crawlers</b></p>
<a href="intro.html">Intro <em>guide</em></a>
<a href="/q?a=1&amp;b=2" rel="nofollow ugc">Q</a>
<a href="mailto:x@y.z">mail</a>
<a href="https://other.org/x">Other</a></body></html>"#;

    #[test]
    fn extracts_links_anchors_meta_and_text() {
        let p = parse(&Url::parse("https://example.com/start").unwrap(), HTML.as_bytes());
        assert_eq!(p.title, "Rust & Crawlers");
        assert_eq!(p.canonical.unwrap().as_str(), "https://example.com/docs/page");
        assert!(p.noindex && !p.nofollow);
        let links: Vec<_> = p.links.iter().map(|l| (l.url.as_str(), l.anchor.as_str(), l.nofollow)).collect();
        assert_eq!(links, vec![
            ("https://example.com/docs/intro.html", "Intro guide", false),
            ("https://example.com/q?a=1&b=2", "Q", true),
            ("https://other.org/x", "Other", false),
        ]);
        assert!(p.text.contains("Hello World of crawlers"), "{}", p.text);
        assert!(!p.text.contains("var x") && !p.text.contains(".a{}"));
    }
    #[test]
    fn meta_robots_none_means_noindex_and_nofollow() {
        let p = parse(&Url::parse("https://a.com/").unwrap(), br#"<meta name="ROBOTS" content="None">"#);
        assert!(p.noindex && p.nofollow);
    }
    #[test]
    fn decodes_entities_including_numeric() {
        assert_eq!(decode_entities("a &lt;b&gt; &#65;&#x42; &amp;amp; &bogus; &"), "a <b> AB &amp; &bogus; &");
    }
}
```

- [ ] **Step 2:** Run `cargo test parse`. Expect FAIL.
- [ ] **Step 3: Implement** a single streaming pass with no DOM. Handlers borrow local `RefCell`s (lol_html's `Settings::new()` allows non-`'static` closures).
  - `element!("base[href]")` updates the base URL.
  - `element!("a[href]")`:
    - `normalize(base, decode_entities(href))`. If valid, push a `Link` with an empty anchor and set `current = Some(idx)`; otherwise `current = None`. Because of this, a bad href's text never ends up in the previous link's anchor.
    - `nofollow` = the `rel` tokens include `nofollow`.
  - `text!("a[href]")` appends the chunk to `links[current]`, plus a space when `last_in_text_node()`.
  - `text!("title")` appends to the title.
  - `element!("link[rel][href]")` with the `canonical` rel token sets canonical.
  - `element!("meta[name][content]")`, where the name equals `robots` ignoring case: `noindex`/`nofollow`/`none` set the flags.
  - `doc_text!` appends chunks whose `text_type() == TextType::Data` (this excludes script, style and title), plus a space on `last_in_text_node()`.
  - Run `HtmlRewriter::new(settings, |_: &[u8]| {})`, then `write` and `end`. Errors are ignored and partial results kept.
  - **Finalize:** `decode_entities` on the title, text and anchors (raw text is accumulated first, so an entity split across chunks still decodes). Collapse whitespace runs to a single space and trim.
  - `fn decode_entities(s: &str) -> Cow<'_, str>` handles `amp lt gt quot apos nbsp` and `&#N;`/`&#xH;`. Unknown entities are left untouched.
  - If the test shows `get_attribute` already decodes `&amp;`, drop the decode on hrefs.
  - `// ponytail: no charset sniffing (lol_html defaults to UTF-8); SP5 re-parses raw HTML from WARC.`

  **As built** (the tests live in `tests/parse.rs`, so `decode_entities` is `pub`):
  - **lol_html settings:**
    - `with_strict(false)`. Strict mode (the default) stops reading at any script/style/title/textarea/iframe/xmp/noscript tag inside `<select>` or after `<frameset>`, and `<select><script>` is valid HTML.
    - `with_adjust_charset_on_meta_tag(!bom)`. A leading UTF-8 BOM is stripped and outranks `<meta charset>`, as the HTML spec says.
  - **Per-element rules:**
    - `get_attribute` returns raw values, so hrefs are decoded.
    - Only the first `<base href>` counts, even when it's unusable.
    - The title takes only RCData chunks, which skips SVG `<title>` tooltips, and only from the first title.
    - Anchors also take `alt` text from `a[href] img[alt]` and skip non-Data text (script inside `<a>`).
  - **`decode_entities`:**
    - Every numeric reference, following HTML's rules: NUL, surrogates and out-of-range values → U+FFFD, and 0x80–0x9F → Windows-1252.
    - 144 named references: the Latin-1 table plus HTML 4's special characters, cross-checked against the HTML5 table. `;` is required.
    - The name scan stops before the next `&`, so the decode is linear time.
  - **Known limits (`ponytail:`):** links before `<base>`, links inside `<noscript>` (raw text to lol_html), and no HTTP-header charset.
  - **Checks:** 17 tests in `tests/parse.rs` (the plan's 3 plus edge cases) and 34 mutations, all caught.
- [ ] **Step 4:** Run `cargo test parse`. Expect PASS.
- [ ] **Step 5:** Update `STATE.md`. Commit: `feat(parse): streaming link/text extraction with lol_html`.

### Task 7: `mockweb.rs` — synthetic web

**Files:** Create `src/mockweb.rs`; add `pub mod mockweb;`. Run `cargo add tokio@1.54.1 --features rt-multi-thread,macros,net,time,sync,signal` and `cargo add axum@0.8.9`.

**Produces:**

```rust
#[derive(Clone, Debug)]
pub struct MockConfig { pub hosts: u32, pub pages_per_host: u32, pub links_per_page: u32, pub page_words: usize, pub latency_ms: (u64, u64), pub cross_host_pct: u32 }
// Default: 1000 hosts, 500 pages/host, 20 links, 4000 words (~30 KB), 20..200 ms, 30% cross-host
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Behavior { Normal, DisallowPrivate, CrawlDelay2, RobotsDown, RateLimited }
pub fn behavior(host: u32) -> Behavior;            // by host % 10: 0, 1, 2, 3, otherwise Normal
pub fn host_name(i: u32) -> String;                // "h{i}.mock"
pub fn host_index(name: &str) -> Option<u32>;
pub fn robots_txt(host: u32) -> (u16, String);     // 404 / Disallow: /private / Crawl-delay: 2 / 503
pub fn page_html(cfg: &MockConfig, port: u16, host: u32, page: u32) -> String; // deterministic (splitmix64 seeded by xxh3)
pub fn politeness_ceiling(cfg: &MockConfig, min_delay: Duration) -> f64;      // Σ 1/max(min_delay, crawl_delay) over hosts with robots up
#[derive(Clone, Debug)] pub struct Hit { pub host: String, pub path: String, pub at: std::time::Instant, pub status: u16 }
pub struct MockWeb { pub addr: SocketAddr /* + private shared state */ }
pub async fn start(cfg: MockConfig, port: u16) -> std::io::Result<MockWeb>; // 127.0.0.1:port (0 = any free port)
impl MockWeb {
    pub fn resolve_overrides(&self) -> Vec<(String, SocketAddr)>;
    pub fn seeds(&self, n: u32) -> Vec<Url>;       // http://h{i}.mock:{port}/
    pub fn hits(&self) -> Vec<Hit>;
}
```

**Routes:** one axum `fallback` handler reads the `Host` header and the path.
- `/robots.txt` → `robots_txt`.
- `/` and `/p/{n}` → `page_html` after a latency drawn deterministically from `(host, path)`. On `RateLimited` hosts, every 3rd page request (robots.txt not counted) returns `429` with `Retry-After: 2`.
- `/private/{n}` → a page.
- `/big` → 3 MiB of HTML.
- `/redirect` → `301` with `Location: /p/0`.
- `/file.pdf` → `application/pdf`.
- Pages on `DisallowPrivate` hosts link to `/private/…`.

Every request is logged as a `Hit`.

- [ ] **Step 1: Tests** (pure; the HTTP side is exercised in Task 8):

```rust
#[test]
fn pages_are_deterministic_and_linked() {
    let cfg = MockConfig::default();
    let a = page_html(&cfg, 8080, 4, 7);
    assert_eq!(a, page_html(&cfg, 8080, 4, 7));
    assert_ne!(a, page_html(&cfg, 8080, 4, 8));
    assert_eq!(a.matches("<a href=").count(), cfg.links_per_page as usize);
    assert!(a.len() > 20_000);
    assert!(a.contains(".mock:8080/p/")); // cross-host links carry the port
}
#[test]
fn behaviors_and_robots() {
    assert_eq!(behavior(10), Behavior::DisallowPrivate);
    assert_eq!(robots_txt(12).0, 503);
    assert!(robots_txt(10).1.contains("Disallow: /private"));
    assert!(robots_txt(11).1.contains("Crawl-delay: 2"));
    assert_eq!(robots_txt(4).0, 404);
    assert_eq!(host_index(&host_name(42)), Some(42));
}
#[test]
fn ceiling_excludes_robots_down_and_respects_crawl_delay() {
    let cfg = MockConfig { hosts: 10, ..Default::default() };
    // 8 hosts at 1/s, 1 host at 0.5/s (crawl-delay 2), 1 host down
    assert!((politeness_ceiling(&cfg, Duration::from_secs(1)) - 8.5).abs() < 1e-9);
}
```

- [ ] **Step 2:** Run `cargo test mockweb`. Expect FAIL. Then implement. Run `cargo test mockweb`. Expect PASS.

  **As built** (tests in `tests/mockweb.rs`):
  - **Dependencies:**
    - axum with default features off (`tokio`, `http1` only).
    - tokio also gets `io-util`, used by the raw-TCP HTTP tests. Those tests don't need reqwest, so the server is checked on its own, before Task 8.
  - **Additions to the design:**
    - Every page with `page % 10 == 9` copies the page before it except for the title. This plants near-duplicates (same SimHash, different body hash), so later crawl tests can check dedup end to end.
    - `host_index` accepts only the exact `host_name` spelling.
    - Host names in requests are case-insensitive.
    - `at` is logged on arrival, before the latency, because politeness is judged by arrival times.
    - The rate-limit counter is atomic per host, so it stays exact under parallel requests.
  - **Checks:** 15 tests (the plan's 3 plus behavior/robots agreement checked through `RobotsRules`, mock pages parsed with `parse`, config edge cases, every HTTP route, 429 cadence interleaved and under 300 parallel requests, the request log, latency) and 30 mutations, all caught.
- [ ] **Step 3:** Update `STATE.md`. Commit: `feat(mockweb): deterministic synthetic web with behaviours`.

### Task 8: `fetch.rs`

**Files:** Create `src/fetch.rs`; add `pub mod fetch;`. Run `cargo add reqwest@0.13.5 --no-default-features --features rustls,http2,gzip,brotli,zstd,hickory-dns`. If `aws-lc-sys` fails to build, `sudo apt install cmake` (one-time, run by the user).

**Produces:**

```rust
#[derive(Clone, Debug)]
pub struct FetchConfig { pub user_agent: String, pub connect_timeout: Duration, pub timeout: Duration, pub max_body: usize, pub resolve: Vec<(String, SocketAddr)> }
#[derive(Debug)]
pub struct FetchResponse { pub status: u16, pub http_head: Vec<u8>, pub is_html: bool, pub content_type: Option<String>,
                           pub location: Option<String>, pub retry_after: Option<Duration>, pub body: Vec<u8>, pub truncated: bool, pub elapsed: Duration }
#[derive(Debug)] pub enum FetchError { Timeout, Connect, Other(String) }   // + Display + Error
impl FetchError { pub fn retryable(&self) -> bool; pub fn kind(&self) -> &'static str; }
pub struct Fetcher { /* page client: redirect::Policy::none(); robots client: Policy::limited(5) */ }
impl Fetcher {
    pub fn new(cfg: &FetchConfig) -> Result<Self, reqwest::Error>;
    pub async fn fetch_page(&self, url: &Url) -> Result<FetchResponse, FetchError>;   // HTML only: body skipped otherwise
    pub async fn fetch_robots(&self, url: &Url) -> Result<FetchResponse, FetchError>; // any content type
}
pub fn parse_retry_after(v: &str) -> Option<Duration>;   // delta-seconds only; HTTP-date → None (ponytail)
```

- [ ] **Step 1: Tests:**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::mockweb::{self, MockConfig};
    fn cfg(resolve: Vec<(String, std::net::SocketAddr)>) -> FetchConfig {
        FetchConfig { user_agent: "McCrawl/0.1 (+test)".into(), connect_timeout: Duration::from_secs(2),
                      timeout: Duration::from_secs(5), max_body: 2 << 20, resolve }
    }
    #[tokio::test]
    async fn classifies_mock_responses() {
        let web = mockweb::start(MockConfig { hosts: 10, latency_ms: (0, 0), ..Default::default() }, 0).await.unwrap();
        let f = Fetcher::new(&cfg(web.resolve_overrides())).unwrap();
        let u = |h: u32, p: &str| Url::parse(&format!("http://{}:{}{p}", mockweb::host_name(h), web.addr.port())).unwrap();
        let r = f.fetch_page(&u(4, "/p/1")).await.unwrap();
        assert_eq!(r.status, 200);
        assert!(r.is_html && !r.body.is_empty() && !r.truncated);
        let head = String::from_utf8(r.http_head).unwrap();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n") && !head.to_ascii_lowercase().contains("content-encoding"));
        let r = f.fetch_page(&u(4, "/redirect")).await.unwrap();
        assert_eq!((r.status, r.location.as_deref()), (301, Some("/p/0")));
        let r = f.fetch_page(&u(4, "/file.pdf")).await.unwrap();
        assert!(!r.is_html && r.body.is_empty());
        let r = f.fetch_page(&u(4, "/big")).await.unwrap();
        assert!(r.truncated && r.body.len() == 2 << 20);
        let mut saw_429 = false;
        for i in 0..3 {
            let r = f.fetch_page(&u(3, &format!("/p/{i}"))).await.unwrap();
            if r.status == 429 { saw_429 = true; assert_eq!(r.retry_after, Some(Duration::from_secs(2))); }
        }
        assert!(saw_429);
        assert_eq!(f.fetch_robots(&u(2, "/robots.txt")).await.unwrap().status, 503);
    }
    #[tokio::test]
    async fn connection_refused_is_retryable() {
        let f = Fetcher::new(&cfg(vec![])).unwrap();
        let err = f.fetch_page(&Url::parse("http://127.0.0.1:1/").unwrap()).await.unwrap_err();
        assert!(err.retryable(), "{err:?}");
    }
    #[test]
    fn retry_after_forms() {
        assert_eq!(parse_retry_after(" 120 "), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
    }
}
```

- [ ] **Step 2:** Run `cargo test fetch`. Expect FAIL.
- [ ] **Step 3: Implement.**
  - **Clients:** `reqwest::Client::builder()` with `user_agent`, `connect_timeout`, `timeout`, `gzip/brotli/zstd(true)`, `pool_max_idle_per_host(1)`, `pool_idle_timeout(30 s)`, and `.resolve(name, addr)` for each override. The page client uses `redirect(Policy::none())`; the robots client uses `Policy::limited(5)`.
  - **Request:** `get(url).send()`, measuring `elapsed` from send until the body is finished.
  - **Content type:** `is_html` = `text/html` or `application/xhtml+xml`. For pages that aren't HTML, return without reading the body.
  - **Body:** read with a `resp.chunk().await` loop up to `max_body`; stop and set `truncated` when it's reached.
  - **`http_head`:** `HTTP/1.1 {code} {canonical_reason}\r\n` plus headers, minus `content-encoding`, `transfer-encoding` and `content-length`, plus `Content-Length: {body.len()}`, plus `\r\n`.
  - **Error mapping:** `is_timeout()` → `Timeout`, `is_connect()` → `Connect`, otherwise `Other(e.to_string())`.
- [ ] **Step 4:** Run `cargo test fetch`. Expect PASS.

  **As built** (tests in `tests/fetch.rs`):
  - **Build:** aws-lc-sys built without cmake. `flate2` is added now as a dev-dependency (gzip test bodies); Task 9 moves it to `[dependencies]`.
  - **Changes from the sketch above:**
    - `FetchError::Connect` carries the cause (`Connect(String)`), so a report can tell DNS, refused and TLS apart. Messages are `kind: cause chain`, without the URL.
    - `Content-Encoding` is not stripped by us: reqwest (tower-http) removes it, with `Content-Length`, only when it decodes, so one still present (an encoding we don't decode) is true of the stored bytes. `Transfer-Encoding` and the server's `Content-Length` are always dropped.
    - Each client sends an `Accept` header (`text/html,…` for pages, `text/plain,*/*` for robots.txt). Compression is on by default with the features; reqwest advertises `zstd,gzip,br`.
    - Header values are read lossily (a raw UTF-8 `Location` works) and written to `http_head` as raw bytes.
    - `parse_retry_after` takes digits only (`+5` and `1.5` are rejected); a value too big for u64 saturates, and the frontier caps it at 24 h.
    - The buffer reservation from `Content-Length` is capped at `max_body`, and the read stops as soon as the cap is reached.
  - **Checks:** 25 tests (the plan's 3 plus hand-written servers for compression, bombs, cap boundaries, content types, raw headers, redirect limits, timeouts including a separate connect timeout, TLS and broken responses) and 48 mutations, all caught.
- [ ] **Step 5:** Update `STATE.md`. Commit: `feat(fetch): reqwest fetcher with body cap and classified errors`.

### Task 9: `store.rs`

**Files:** Create `src/store.rs`; add `pub mod store;`. Run `cargo remove --dev flate2`, `cargo add flate2@1.1.11`, `cargo add serde@1.0.229 --features derive` and `cargo add serde_json@1.0.154`. (The form `--features serde/derive` with two versioned crates is rejected by cargo.)

**Produces:**

```rust
#[derive(Serialize, Default, Debug)]
pub struct PageRecord { pub url: String, pub host: String, pub depth: u16, pub parent_url: Option<String>, pub priority: i32,
    pub fetched_at: u64 /* unix ms */, pub status: Option<u16>, pub error: Option<String>, pub content_type: Option<String>,
    pub bytes: u64, pub truncated: bool, pub title: String, pub text: String, pub canonical: Option<String>, pub noindex: bool,
    pub links: Vec<LinkOut>, pub simhash: Option<String>, pub body_hash: Option<String>, pub near_dup: Option<String> }
#[derive(Serialize, Debug)] pub struct LinkOut { pub url: String, pub anchor: String, pub nofollow: bool }
pub struct WarcPayload { pub http_head: Vec<u8>, pub body: Vec<u8>, pub truncated: bool }
pub struct StoreMsg { pub record: PageRecord, pub simhash: Option<u64>, pub body_hash: Option<u64>, pub warc: Option<WarcPayload> }
#[derive(Serialize, Default, Debug, Clone)]
pub struct StoreSummary { pub records: u64, pub warc_records: u64, pub warc_files: u32, pub exact_dups: u64, pub near_dups: u64 }
pub fn iso8601(unix_secs: u64) -> String;   // Hinnant's civil-from-days, no date crate
pub fn warc_record(kind: &str, target: Option<&str>, date: &str, content_type: &str, extra: &[(&str, String)], payload: &[u8], id: &str) -> Vec<u8>;
pub struct WarcWriter;  impl WarcWriter { pub fn create(dir: &Path, rotate_at: u64) -> io::Result<Self>; pub fn write_response(&mut self, url: &str, unix_secs: u64, p: &WarcPayload) -> io::Result<()>; pub fn flush(&mut self) -> io::Result<()>; }
pub struct Store;  impl Store { pub fn open(dir: &Path, rotate_at: u64) -> io::Result<Self>; pub fn write(&mut self, m: StoreMsg) -> io::Result<()>; pub fn finish(self) -> io::Result<StoreSummary>; }
pub fn spawn(store: Store, rx: tokio::sync::mpsc::Receiver<StoreMsg>) -> std::thread::JoinHandle<io::Result<StoreSummary>>;
```

- [ ] **Step 1: Tests:**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mccrawl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }
    #[test]
    fn iso8601_known_dates() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601(1_763_424_000 + 3661), "2025-11-18T01:01:01Z");
    }
    #[test]
    fn warc_record_framing() {
        let rec = warc_record("response", Some("http://a.com/"), "2025-11-18T00:00:00Z",
                              "application/http;msgtype=response", &[], b"HTTP/1.1 200 OK\r\n\r\nhi", "<urn:uuid:x>");
        let s = String::from_utf8(rec).unwrap();
        assert!(s.starts_with("WARC/1.1\r\nWARC-Type: response\r\n"));
        assert!(s.contains("WARC-Target-URI: http://a.com/\r\n") && s.contains("Content-Length: 21\r\n"));
        assert!(s.ends_with("\r\n\r\nHTTP/1.1 200 OK\r\n\r\nhi\r\n\r\n"));
    }
    #[test]
    fn rotates_files_and_each_record_is_a_gzip_member() {
        let dir = tmp("warc");
        std::fs::create_dir_all(&dir).unwrap();
        let mut w = WarcWriter::create(&dir, 1).unwrap(); // rotate after every record
        let p = WarcPayload { http_head: b"HTTP/1.1 200 OK\r\n\r\n".to_vec(), body: b"x".to_vec(), truncated: false };
        w.write_response("http://a.com/", 0, &p).unwrap();
        w.write_response("http://a.com/2", 0, &p).unwrap();
        w.flush().unwrap();
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert!(files.len() >= 2);
        let mut s = String::new();
        flate2::read::MultiGzDecoder::new(std::fs::File::open(dir.join("crawl-00000.warc.gz")).unwrap())
            .read_to_string(&mut s).unwrap();
        assert!(s.starts_with("WARC/1.1\r\nWARC-Type: warcinfo"));
    }
    #[test]
    fn marks_near_duplicates_and_writes_jsonl() {
        let dir = tmp("store");
        let mut st = Store::open(&dir, 1 << 30).unwrap();
        for u in ["http://a.com/1", "http://b.com/1"] {
            st.write(StoreMsg { record: PageRecord { url: u.into(), ..Default::default() },
                                simhash: Some(0xABCD), body_hash: Some(1), warc: None }).unwrap();
        }
        let sum = st.finish().unwrap();
        assert_eq!((sum.records, sum.near_dups, sum.exact_dups), (2, 1, 1));
        let jsonl = std::fs::read_to_string(dir.join("pages.jsonl")).unwrap();
        let second: serde_json::Value = serde_json::from_str(jsonl.lines().nth(1).unwrap()).unwrap();
        assert_eq!(second["near_dup"], "http://a.com/1");
        assert_eq!(second["simhash"], "000000000000abcd");
    }
}
```

- [ ] **Step 2:** Run `cargo test store`. Expect FAIL.
- [ ] **Step 3: Implement.**
  - **`warc_record`** writes, in order:
    - `WARC/1.1`
    - `WARC-Type`
    - `WARC-Record-ID`
    - `WARC-Date`
    - optional `WARC-Target-URI`
    - `extra` headers (e.g. `WARC-Truncated: length`)
    - `Content-Type`
    - `Content-Length`
    - blank line, payload, `\r\n\r\n`
  - **Record ID:** `<urn:uuid:…>`, formatted 8-4-4-4-12 from `xxh3_128(url, counter, nanos)`. `// ponytail: not RFC 4122 random, but unique per record`.
  - **`WarcWriter`:** each record is compressed with its own `GzEncoder` and appended to `crawl-NNNNN.warc.gz`. Every new file starts with a `warcinfo` record (`software: McCrawl/0.1`). The writer rotates once the file size reaches `rotate_at`.
  - **`Store::write`:**
    - If `body_hash` is in the exact set, `exact_dups += 1`.
    - A nonzero `simhash` is looked up in `NearDupIndex`; on a hit, `near_dup = urls[id]` and `near_dups += 1`. The fingerprint is then inserted with `id = urls.len()`.
    - The hashes are written as 16-digit hex into the record.
    - The record goes to `pages.jsonl` through a `BufWriter`; the WARC response is written if a payload is present.
  - **`spawn`:** a std thread loop. `try_recv` → write. `Empty` → flush, then `blocking_recv`. `None` or `Disconnected` → `finish`. This flushes whenever the pipeline goes idle, so a crash leaves valid files behind.
- [ ] **Step 4:** Run `cargo test store`. Expect PASS.

  **As built** (tests in `tests/store.rs`):
  - **Changes from the sketch above:**
    - Files are opened with `create_new`, so a reused output directory fails instead of overwriting an earlier crawl (the first WARC file, `pages.jsonl`, and any file a rotation would land on).
    - Rotation is checked before each response, and only once something has been archived. So no file is only a `warcinfo`, except the single file of a crawl that archived nothing. `rotate_at` counts compressed bytes.
    - The writer compresses the header, `http_head`, body and trailer straight into one reused buffer, so the body is not copied into a record first. `warc_record` stays for framing tests and `warcinfo`.
    - Record IDs are RFC 9562 version-8 UUIDs (the vendor-defined version, honest about not being random), from xxh3_128(clock, pid, a process-wide counter). The `warcinfo` record also carries `WARC-Filename`.
    - The store sets `simhash`, `body_hash` and `near_dup` itself. Every nonzero fingerprint is indexed, near duplicates included, so a chain (C near B, B near A) still marks C. `near_dup` is the earliest page the index finds. A body hash of 0 is an ordinary value; only a SimHash of 0 (no text) is skipped.
    - `StoreSummary.warc_records` counts responses; each file's `warcinfo` is implied by `warc_files`.
  - **Checks:** 24 tests (the plan's 4, plus a 450-year calendar check, exact framing, a per-member WARC reader, rotation timing, hostile URLs, overwrite refusal, duplicate-marking rules, JSONL escaping and schema, and the store thread's ordering, idle flush and empty finish). 43 of 44 mutations caught; the survivor is the `>=` vs `>` rotation boundary, which random record IDs make untestable and which only shifts one record between files. An early warcio read of files written from real mock-web fetches passed (6 files, 121 responses, 1 truncated).
- [ ] **Step 5:** Update `STATE.md`. Commit: `feat(store): WARC writer, JSONL and near-dup marking`.

### Task 10: `metrics.rs`

**Files:** Create `src/metrics.rs`; add `pub mod metrics;`. Add `#[derive(Serialize)]` to `FrontierStats`.

**Produces:**

```rust
#[derive(Default, Debug)]
pub struct Tally { pub pages: u64, pub bytes: u64, pub status: BTreeMap<u16, u64>, pub errors: BTreeMap<&'static str, u64>,
    pub retries: u64, pub redirects: u64, pub non_html: u64, pub robots_fetches: u64, pub latencies_ms: Vec<u32>, pub idle_slot: Duration }
#[derive(Serialize, Debug)]
pub struct Report { pub wall_secs: f64, pub pages: u64, pub pages_per_sec: f64, pub bytes: u64, pub status: BTreeMap<u16, u64>,
    pub errors: BTreeMap<String, u64>, pub retries: u64, pub redirects: u64, pub non_html: u64, pub robots_fetches: u64,
    pub idle_slot_secs: f64, pub latency_p50_ms: u32, pub latency_p99_ms: u32, pub cpu_secs: Option<f64>, pub peak_rss_mb: Option<f64>,
    pub frontier: FrontierStats, pub store: StoreSummary }
pub fn percentile(sorted: &[u32], p: f64) -> u32;   // nearest-rank, 0 for empty
pub fn parse_vmhwm_kb(status: &str) -> Option<u64>;
pub fn parse_cpu_secs(stat: &str) -> Option<f64>;   // utime+stime (fields 14,15) / 100; split after the last ')'
pub fn peak_rss_mb() -> Option<f64>;
pub fn cpu_secs() -> Option<f64>;
impl Tally { pub fn report(self, wall: Duration, frontier: FrontierStats, store: StoreSummary) -> Report; pub fn progress_line(&self, wall: Duration, f: &FrontierStats) -> String; }
```

The scheduler is the only owner of `Tally`, so the counters are plain fields; atomics aren't needed.

- [ ] **Step 1: Tests:**
  - `percentile(&[1..=100], 0.5) == 50` and `0.99 → 99`; an empty slice → 0.
  - `parse_vmhwm_kb("Name:\tx\nVmHWM:\t  12345 kB\n") == Some(12345)`.
  - `parse_cpu_secs("1 (my (weird) comm) S 0 0 0 0 0 0 0 0 0 0 250 150 0 0") == Some(4.0)`.
  - `peak_rss_mb().is_some()` (runs on Linux/WSL).
- [ ] **Step 2:** Run `cargo test metrics`. Expect FAIL. Implement. Run `cargo test metrics`. Expect PASS.

  **As built** (tests in `tests/metrics.rs`):
  - **Changes from the sketch above:**
    - `Tally::page(status, bytes, elapsed)` counts a response in one place, so the millisecond conversion (saturating at u32::MAX) isn't repeated in the crawler.
    - `Report.errors` keeps `&'static str` keys; it is only serialized.
    - `pages_per_sec` is 0 for zero wall time, because serde_json writes infinity as null.
    - `percentile` shaves the rank product by 1e-12, so float error (0.07 × 100 = 7.000000000000001) can't move it up a rank. p outside 0..=1 needs no clamp: the cast saturates and the rank is clamped.
    - `parse_cpu_secs` adds the two tick counts as f64, so hostile input can't overflow.
  - **Checks:** 9 tests (the plan's 4 groups, plus an exact integer-rank check over 10 sizes, real-format `/proc` lines, live readers checked against the raw VmHWM and against 64 MiB / 300 ms of real work, the report's JSON key sets, and the exact progress line) and 24 mutations, all caught (the float shave is caught only by the exact-rank check, which shows the float error is real).
- [ ] **Step 3:** Update `STATE.md`. Commit: `feat(metrics): tally, percentiles and /proc readers`.

### Task 11: `crawler.rs` — the scheduler actor and pipeline

**Files:** Create `src/crawler.rs`; add `pub mod crawler;`.

**Consumes:** everything from Tasks 1–10.

**Produces:**

```rust
#[derive(Clone, Debug)]
pub struct CrawlConfig { pub seeds: Vec<Url>, pub out_dir: PathBuf, pub budget: u64, pub concurrency: usize, pub per_host_cap: u32,
    pub min_delay: Duration, pub delay_factor: f64, pub user_agent: String, pub robots_agent: String,
    pub resolve: Vec<(String, SocketAddr)>, pub max_body: usize, pub robots_retry: Duration, pub progress_every: Option<Duration> }
impl CrawlConfig { pub fn new(seeds: Vec<Url>, out_dir: PathBuf) -> Self; } // spec defaults: budget 100k, conc 512, cap 500, 1 s, k=5, 2 MiB, robots_retry 60 s, progress 5 s
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub async fn run(cfg: CrawlConfig, shutdown: impl Future<Output = ()>) -> Result<Report, BoxError>;
```

**Pipeline in `run`:**
1. **Start the stages.**
   - Create `out_dir` and run `Store::open`. Start `store::spawn` with a `store_tx` channel (capacity 1024).
   - Run `Fetcher::new`. Run `Frontier::new` and push the seeds.
   - Create `parse_tx`/`parse_rx` (capacity = `concurrency`) and `links_tx`/`links_rx` (capacity 1024).
2. **Parse dispatcher task.** For each `ParseJob { entry, resp }`:
   - Acquire a `Semaphore` permit (`available_parallelism`), then run in `spawn_blocking`:
     - `parse`, then `simhash(&parsed.text)` and `body_hash`.
     - Build a `PageRecord`. Then `store_tx.blocking_send(StoreMsg { …, warc: Some(..) })`.
     - Build children from the links, skipping all of them when the page is meta-`nofollow` and skipping `rel=nofollow` links. Each child is `UrlEntry::child(url, &entry, Arc::from(entry.url.as_str()))`.
     - `links_tx.blocking_send(children)`, ignoring send errors.
   - Keep the blocking handles in a `JoinSet` and await them all before exiting.
3. **Fetch task** (`JoinSet<Done>`):
   - **`Job::Robots`:** `fetch_robots` → `RobotsRules::from_response` (an `Err` becomes `Unreachable`). Return `Done::Robots { host, rules, elapsed }`.
   - **`Job::Page`:** `fetch_page`.
     - 200 and HTML → `parse_tx.send(ParseJob).await`. A full channel is the backpressure: the fetch task keeps holding its slot until it gets in.
     - Any other response or an error → `store_tx.send(StoreMsg)` with the status or error and no WARC.
     - Return `Done::Page { host, entry, result: Result<Meta{status, elapsed, retry_after, location, bytes, is_html}, FetchError>, sent_to_parse }`.
4. **Scheduler loop** (owns `Frontier` and `Tally`; no locks):

```rust
loop {
    // fill free slots
    while !stopping && inflight.len() < cfg.concurrency && dispatched < cfg.budget {
        match frontier.next(Instant::now()) {
            Next::Job(job) => { if matches!(job, Job::Page { .. }) { dispatched += 1; } inflight.spawn(fetch_job(ctx.clone(), job)); }
            Next::WaitUntil(t) => { wake = Some(t); break; }
            Next::Idle | Next::Done => { wake = None; break; }
        }
    }
    if inflight.is_empty() && pending_parse == 0 && (stopping || dispatched >= cfg.budget || frontier.is_done()) { break; }
    tokio::select! {
        Some(done) = inflight.join_next() => handle_done(done?, &mut frontier, &mut tally, &mut pending_parse),
        Some(children) = links_rx.recv() => { pending_parse -= 1; for c in children { frontier.push(c, Instant::now()); } }
        _ = sleep_until(wake), if wake.is_some() => {}
        _ = &mut shutdown, if !stopping => { stopping = true; grace_deadline = Instant::now() + 10 s; }
        _ = progress.tick(), if cfg.progress_every.is_some() => eprintln!("{}", tally.progress_line(..)),
    }
    // after the grace deadline: inflight.abort_all(); break
}
```

   Notes on the sketch:
   - **Errors:** a `JoinError` from a panicking fetch task is a bug, so `run` returns `Err`. Tasks aborted after the grace deadline are never joined.
   - **Parse messages:** every `ParseJob` produces exactly one links message (possibly an empty `Vec`), which keeps `pending_parse` balanced.
   - **`handle_done` for pages:**
     - Map the result to an `Outcome`: 429/503 → `RateLimited`, other 5xx → `ServerError`, `Err` → `NetworkError`, everything else → `Ok`. Then `frontier.complete_page`.
     - **Retries:** a retryable error or any 5xx with `retries < 2` → `requeue(retries + 1)`.
     - **Redirects:** 3xx + `Location` → `normalize(&entry.url, loc)`, and if `redirect_hops < 5`, `push` with `redirect_hops + 1`, the same depth, priority and parent.
     - Update the tally (status, bytes, latency, errors by `kind()`); `pending_parse += sent_to_parse as isize`.
   - **Idle slots:** `idle_slot` accumulates the time with free slots but no job allowed (start a timer on `WaitUntil`/`Idle`, stop it on the next dispatch).
5. **Teardown, in this order:**
   1. Drop `links_rx` (late parse sends then fail fast).
   2. Drop `parse_tx` and await the dispatcher.
   3. Drop all `store_tx` clones and join the store thread via `spawn_blocking(|| h.join())`.
   4. Build the `Report` and write `out_dir/report.json`.

- [ ] **Step 1: Tests** (in `crawler.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::mockweb::{self, MockConfig};
    fn cfg_for(web: &mockweb::MockWeb, name: &str, budget: u64) -> CrawlConfig {
        let out = std::env::temp_dir().join(format!("mccrawl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        CrawlConfig { budget, concurrency: 64, min_delay: Duration::from_millis(50), user_agent: "McCrawl/0.1 (+test)".into(),
                      resolve: web.resolve_overrides(), progress_every: None, ..CrawlConfig::new(web.seeds(50), out) }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn small_crawl_respects_budget_and_writes_outputs() {
        let web = mockweb::start(MockConfig { hosts: 50, latency_ms: (1, 3), ..Default::default() }, 0).await.unwrap();
        let cfg = cfg_for(&web, "small", 300);
        let out = cfg.out_dir.clone();
        let r = run(cfg, std::future::pending()).await.unwrap();
        assert!(r.pages <= 300 && r.pages >= 250, "pages = {}", r.pages);
        let lines = std::fs::read_to_string(out.join("pages.jsonl")).unwrap().lines().count() as u64;
        assert_eq!(lines, r.store.records);
        assert!(out.join("report.json").exists() && out.join("crawl-00000.warc.gz").exists());
        assert!(r.store.warc_records > 0);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_signal_stops_promptly() {
        let web = mockweb::start(MockConfig { hosts: 50, ..Default::default() }, 0).await.unwrap();
        let cfg = cfg_for(&web, "stop", 1_000_000);
        let t = std::time::Instant::now();
        let r = run(cfg, tokio::time::sleep(Duration::from_millis(300))).await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(12));
        assert!(r.pages > 0);
    }
}
```

- [ ] **Step 2:** Run `cargo test crawler`. Expect FAIL. Implement the stages in the order fetch task → parse dispatcher → scheduler loop → teardown, compiling after each. Run `cargo test crawler`. Expect PASS.
- [ ] **Step 3:** Run `cargo test` (everything) and `cargo clippy --all-targets -- -D warnings`.
- [ ] **Step 4:** Update `STATE.md`. Commit: `feat(crawler): scheduler actor, fetch/parse/store pipeline`.

### Task 12: `main.rs` — CLI

**Files:** Modify `src/main.rs`. Run `cargo add clap@4.6.8 --features derive`.

**Subcommands:**
- **`crawl`**
  - Flags: `--seeds <file>` (one URL per line, `#` comments, bad lines warned and skipped), `--out` (default `$HOME/mccrawl-data/run-<unix_secs>`), `--budget 100000`, `--concurrency 512`, `--per-host-cap 500`, `--min-delay-ms 1000`, `--contact <url>` (required).
  - UA = `McCrawl/0.1 (+{contact})`. Ctrl-C is the shutdown future.
  - Prints the `report.json` path and a summary.
- **`mockweb`**
  - Flags: `--hosts 10000 --port 8080 --latency-ms 20..200`.
  - Serves until Ctrl-C and prints a `curl --resolve` example.
- **`bench`**
  - Flags: `--hosts 10000 --pages 200000 --mode polite|raw --concurrency 64,256,1024 --out`.
  - Starts one in-process mock web.
  - For each concurrency it runs a crawl with all host roots as seeds. Polite mode uses `min_delay` 1 s and k = 5. Raw mode uses `min_delay` 0, k = 0 and mock latency 0.
  - Prints a markdown table (concurrency, pages/s, % of `politeness_ceiling` in polite mode, p50, p99, CPU s, peak RSS MB) and writes `bench.json`.
  - `main` returns `Result<(), Box<dyn Error>>`, on a `#[tokio::main]` multi-thread runtime.

- [ ] **Step 1:** Implement, and add `pub fn parse_seeds(text: &str) -> Vec<Url>` in `src/crawler.rs` (so `tests/` can reach it) with a test (comments, blanks, an invalid line).
- [ ] **Step 2: Smoke test.** Run `ulimit -n 65536 && cargo run --release -- bench --hosts 2000 --pages 20000 --mode polite --concurrency 256`. It should finish and print the table.
- [ ] **Step 3:** Update `STATE.md`. Commit: `feat(cli): crawl, mockweb and bench subcommands`.

### Task 13: Politeness proof and WARC validation

**Files:** Create `tests/politeness.rs`, `tests/warc_check.py`.

- [ ] **Step 1: `tests/politeness.rs`:**

```rust
use mccrawl::{crawler::{self, CrawlConfig}, mockweb::{self, Behavior, MockConfig}};
use std::{collections::HashMap, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn crawl_never_violates_politeness() {
    let web = mockweb::start(MockConfig { hosts: 1000, pages_per_host: 50, latency_ms: (1, 5), ..Default::default() }, 0).await.unwrap();
    let out = std::env::temp_dir().join(format!("mccrawl-polite-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let min_delay = Duration::from_millis(200);
    let cfg = CrawlConfig { budget: 4000, concurrency: 256, min_delay, user_agent: "McCrawl/0.1 (+test)".into(),
                            resolve: web.resolve_overrides(), progress_every: None, ..CrawlConfig::new(web.seeds(1000), out.clone()) };
    let report = crawler::run(cfg, std::future::pending()).await.unwrap();
    assert!(report.pages >= 3000, "only {} pages", report.pages);

    let mut by_host: HashMap<String, Vec<mockweb::Hit>> = HashMap::new();
    for h in web.hits() { by_host.entry(h.host.clone()).or_default().push(h); }
    let slack = Duration::from_millis(5);
    for (host, mut hits) in by_host {
        hits.sort_by_key(|h| h.at);
        let b = mockweb::behavior(mockweb::host_index(&host).unwrap());
        let min_gap = if b == Behavior::CrawlDelay2 { Duration::from_secs(2) } else { min_delay };
        for w in hits.windows(2) {
            let gap = w[1].at - w[0].at;
            assert!(gap + slack >= min_gap, "{host}: gap {gap:?} < {min_gap:?}");
            if w[0].status == 429 { assert!(gap + slack >= Duration::from_secs(2), "{host}: ignored Retry-After"); }
        }
        match b {
            Behavior::DisallowPrivate => assert!(hits.iter().all(|h| !h.path.starts_with("/private")), "{host} fetched /private"),
            Behavior::RobotsDown => assert!(hits.iter().all(|h| h.path == "/robots.txt"), "{host}: page fetched while robots down"),
            _ => {}
        }
    }
    std::fs::remove_dir_all(out).ok();
}
```

- [ ] **Step 2: `tests/warc_check.py`:**

```python
"""Independent WARC validation for McCrawl output, using warcio."""
import sys
from warcio.archiveiterator import ArchiveIterator

def check(path):
    counts = {}
    with open(path, "rb") as f:
        for rec in ArchiveIterator(f):
            counts[rec.rec_type] = counts.get(rec.rec_type, 0) + 1
            if rec.rec_type == "response":
                assert rec.rec_headers.get_header("WARC-Target-URI"), path
                assert rec.http_headers and rec.http_headers.get_statuscode(), path
                rec.content_stream().read()
    return counts

if __name__ == "__main__":
    files = sys.argv[1:]
    assert files, "usage: warc_check.py FILE.warc.gz..."
    total = {}
    for p in files:
        for k, v in check(p).items():
            total[k] = total.get(k, 0) + v
    assert total.get("warcinfo") == len(files) and total.get("response", 0) > 0, total
    print("OK", total)
```

- [ ] **Step 3: Run.**
  - `cargo test --test politeness` → PASS.
  - Run a short bench into `$HOME/mccrawl-data/check`, then `uv run --with warcio python tests/warc_check.py $HOME/mccrawl-data/check/*/crawl-*.warc.gz` → `OK {...}`.
- [ ] **Step 4:** Update `STATE.md`. Commit: `test: politeness proof and warcio validation`.

### Task 14: Benchmarks and profiling

**Files:** Modify `Cargo.toml` (`[profile.release] lto = "fat"`, `codegen-units = 1`, `debug = "line-tables-only"`), plus any hot-path files the profile points to.

- [ ] **Step 1: Baseline.** With `ulimit -n 65536`:
  - `cargo run --release -- bench --mode polite` (10k hosts, 200k pages, concurrency 64, 256, 1024).
  - `bench --mode raw`.
  - Save both `bench.json` files.
  - Expected shape: Little's law (throughput ≈ concurrency ÷ latency) limits low concurrency; at 1024 the crawl approaches the politeness ceiling.
- [ ] **Step 2: Profile.**
  - `cargo install --locked samply`.
  - The user runs `sudo sysctl kernel.perf_event_paranoid=1` once.
  - `samply record --save-only -o $HOME/mccrawl-data/raw.json.gz target/release/mccrawl bench --mode raw --concurrency 1024`, then `samply load`.
- [ ] **Step 3: Optimize** at most the top 2 hot spots from the profile. Likely candidates are per-word `to_lowercase` allocation in `simhash`, the parse output sink and URL re-serialization. Re-run the bench after each and record before/after pages/s. Keep `cargo test` green.
- [ ] **Step 4:** Update `STATE.md`. Commit: `perf: <hot spot> (+X% raw pages/s)`, one commit per optimization.

### Task 15: Real crawl, README, wrap-up

**Files:** Create `seeds/technical.txt`. Modify `README.md` (add benchmarks, profiling, real-crawl stats; keep the existing diagrams), `STATE.md`.

- [ ] **Step 1: Seed list.** Draft `seeds/technical.txt` with about 150 curated technical sources: language and framework docs, engineering blogs, systems and database write-ups, and individual expert blogs. Comments group the sections. The user reviews and edits the list before any crawl.
- [ ] **Step 2: Real crawl.**
  - `ulimit -n 65536 && cargo run --release -- crawl --seeds seeds/technical.txt --budget 30000 --contact <user-supplied URL>`.
  - Watch the progress lines. Then run `warc_check.py` on the output.
- [ ] **Step 3: `README.md`:**
  - What McCrawl is.
  - Architecture diagram (from the spec).
  - Design rationale: two heaps, adaptive delay and the politeness formula, RFC 9309 semantics, SimHash and the Manku index, the actor-owned frontier, Little's law.
  - Benchmark table and profiling results with before/after numbers.
  - Real-crawl stats from `report.json`.
  - How to run (`crawl`, `bench`, tests).
  - Roadmap (sub-projects 2–5).
- [ ] **Step 4:** Final checks: `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`. Mark sub-project 1 done in `STATE.md` and save this plan to `docs/superpowers/plans/2026-11-18-crawl-core.md`. Commit: `docs: README with benchmarks; crawl core complete`.

---

## Verification (end to end)

1. `cargo test` passes everything: unit tests in all 10 modules, crawler tests, and `tests/politeness.rs` with zero violations across 1,000 hosts. The test covers delay gaps, crawl-delay, Retry-After, robots disallow, and robots-down hosts.
2. `uv run --with warcio python tests/warc_check.py <run>/crawl-*.warc.gz` prints `OK`.
3. `cargo run --release -- bench --mode polite` prints a table with pages/s as % of the politeness ceiling; `--mode raw` gives the engine ceiling. Both are recorded in the README.
4. A real crawl of `seeds/technical.txt` with a budget of about 30k finishes, writes `report.json`, and its WARC passes check 2.
5. `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` are clean.

## Spec coverage check

| Spec | Task(s) |
|---|---|
| §5 components | 1–12 |
| §6 frontier and politeness | 4, 5 |
| §7 libraries | pinned per task |
| §8 errors and robots semantics | 3, 5, 8, 11 |
| §9 CLI, lifecycle and outputs | 9, 11, 12 |
| §9 environment | 1 |
| §10 tests | every task + 13 |
| §11 benchmarks and profiling | 12, 14 |
| §12 done criteria | 13–15 |
