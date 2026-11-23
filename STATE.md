# STATE.md — knowledge graph

**Read this before touching code. Update it before finishing.** This replaces scanning the directory.

## Status

- **2026-11-18:** McCrawl is a high-performance web crawler: Rust crawl engine now, Python ML later. Sub-project 1 (crawl core) is being built on branch `feat/crawl-core`, following `docs/superpowers/plans/2026-11-18-crawl-core.md` (15 tasks). **Done:** Task 1 `urlnorm.rs`, Task 2 `dedup.rs`, Task 3 `robots.rs`, Task 4 `frontier.rs` core (18 tests pass, clippy and fmt clean). **Next: Task 5, frontier tests for robots, caps and dead hosts.**
- **Toolchain (WSL):** Rust 1.97.1 from Nix (home-manager; dotfiles are read-only Nix symlinks). Python 3.14, uv.
  - Stale rustup proxies in `~/.cargo/bin` shadow `cargo clippy` / `cargo fmt`. Run `cargo-clippy clippy --all-targets -- -D warnings` and `cargo-fmt` directly instead.
  - Build artifacts go to `~/.cargo-target/mccrawl`, set by the gitignored `.cargo/config.toml`.

## Files

- `Cargo.toml`: crate `mccrawl`, edition 2024. Deps: `url`, `xxhash-rust` (xxh3), `texting_robots`. Used by: `cargo`.
- `Cargo.lock`: generated lockfile. Depends on: `Cargo.toml`.
- `.cargo/config.toml` (gitignored, local only): `target-dir` outside the repo/OneDrive.
- `src/lib.rs`: `pub mod` declarations (`dedup`, `frontier`, `robots`, `urlnorm`). Used by: `src/main.rs` (later), `tests/`.
- `src/main.rs`: binary entry point; still the placeholder "Hello, world!" (CLI comes in Task 12).
- `src/urlnorm.rs`: `normalize(base, href) -> Option<Url>` (resolve, drop fragment and empty query, http(s) only, ≤ 2048 chars, no path segment repeated > 3 times) and `host_key(&Url) -> Option<&str>` (lowercase hostname, the politeness key). Depends on: `url`. Used by: `frontier` (`host_key`); planned: `parse`, `crawler`. Tested by: `tests/urlnorm.rs`.
- `src/dedup.rs`: `url_hash`/`body_hash` (xxh3_64), `hamming`, `simhash` (64-bit SimHash over lowercase word 3-shingles; empty → 0), `SeenSet` (HashSet<u64> of URL hashes; `insert` → true if new), `NearDupIndex` (Manku 4×16-bit block tables; `find`/`insert`), `NEAR_DUP_BITS = 3`. Depends on: `url`, `xxhash-rust`. Used by: `frontier` (SeenSet); planned: `store` (NearDupIndex, hashes), `crawler`. Tested by: `tests/dedup.rs`.
- `src/robots.rs`: `RobotsRules { AllowAll, Parsed(Robot), Unreachable }`. `from_response(status, body, agent)` follows RFC 9309: 2xx parse (a parse error means AllowAll), 4xx AllowAll, 429/5xx/other Unreachable. `allowed(&Url)` (Unreachable → false). `crawl_delay()` never panics and is capped at `MAX_CRAWL_DELAY` = 24 h. `robots_url(&Url)`. Depends on: `texting_robots`, `url`. Used by: `frontier`; planned: `crawler`. Tested by: `tests/robots.rs`.
- `src/frontier.rs`: the pure two-heap frontier (time passed in as `now`). Per-host `BinaryHeap` queues; `waiting` min-heap by `next_allowed`; `ready` max-heap by best priority; each host in at most one of {waiting, ready, in flight}. API: `push` (seen-set dedup), `requeue` (retries), `next(now) -> Next {Job, WaitUntil, Idle, Done}`, `complete_page(host, Outcome, now)`, `complete_robots(host, RobotsRules, elapsed, now)`, `is_done`, `host_name`, `stats`. Types: `UrlEntry` (seed/child, priority = -depth), `Job {Robots, Page}`, `Outcome`, `FrontierConfig`, `FrontierStats`. Politeness: `delay = max(base, min(base × backoff, max_backoff))`, at least Retry-After (≤ 24 h), where `base = max(crawl_delay, min_delay, k × elapsed)`. Backoff ×2 on 429/503, ÷2 on success. A host dies after `max_host_failures` in a row, or after `max_robots_attempts` unreachable robots fetches (retried at robots_retry × 2^(n−1)). Robots TTL 24 h. Per-host cap drops the rest of the queue. Depends on: `urlnorm`, `dedup`, `robots`, `url`. Used by (planned): `crawler`, `metrics` (FrontierStats). Tested by: `tests/frontier.rs`.
- `tests/`: one integration-test file per module, against the public API (no test code in `src/`). Run one with `cargo test --test <module>`.
  - `tests/urlnorm.rs`: canonicalization table, trap/scheme rejections, host key.
  - `tests/dedup.rs`: seen-set dedup, SimHash near/far on a 2000-word fixture, block index hit/reject.
  - `tests/robots.rs`: status semantics (200/404/500/503/429), agent-specific group, hostile Crawl-delay, robots URL.
  - `tests/frontier.rs`: robots first, one in flight per host, priority order, dedup vs requeue, adaptive delay, backoff and Retry-After, a site Crawl-delay above the backoff cap, empty → Done. Deterministic: fixed `t0` plus offsets, no sleeps.
- `README.md`: project overview with Mermaid diagrams: crawl pipeline, per-host frontier state machine, politeness formula, dedup layers, roadmap, sub-project 1 build order. **After each task:** bump the status line and mark the task's box `:::done`. Mirrors: spec §2 and §4–§6, plan task list.
- `.gitignore`: ignores `/target`, `/.cargo/`, local envs, secrets, crawl output (`data/`, `output/`) and `*.log`.
- `CLAUDE.md`: workflow rules for Claude Code (WSL terminal, STATE.md-first workflow).
- `AGENTS.md`: same rules for non-Claude agents (Codex etc.); mirror rule changes into `CLAUDE.md`.
- `STATE.md`: this file, the knowledge graph.
- `docs/superpowers/specs/2026-11-18-crawl-core-design.md`: design for sub-project 1 (crawl core) plus the 5-sub-project roadmap, synced with the plan's refinements.
- `docs/superpowers/plans/2026-11-18-crawl-core.md`: the 15-task implementation plan, with tests and interfaces per task. The source of truth for build order.
