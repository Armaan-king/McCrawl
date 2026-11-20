# STATE.md — knowledge graph

**Read this before touching code. Update it before finishing.** This replaces scanning the directory.

## Status

- **2026-11-18:** McCrawl is a high-performance web crawler: Rust crawl engine now, Python ML later. Sub-project 1 (crawl core) is being built on branch `feat/crawl-core`, following `docs/superpowers/plans/2026-11-18-crawl-core.md` (15 tasks). **Done:** Task 1 `urlnorm.rs`, Task 2 `dedup.rs` (6 tests pass, clippy and fmt clean). **Next: Task 3, `robots.rs`.**
- **Toolchain (WSL):** Rust 1.97.1 from Nix (home-manager; dotfiles are read-only Nix symlinks). Python 3.14, uv.
  - Stale rustup proxies in `~/.cargo/bin` shadow `cargo clippy` / `cargo fmt`. Run `cargo-clippy clippy --all-targets -- -D warnings` and `cargo-fmt` directly instead.
  - Build artifacts go to `~/.cargo-target/mccrawl`, set by the gitignored `.cargo/config.toml`.

## Files

- `Cargo.toml`: crate `mccrawl`, edition 2024. Deps: `url`, `xxhash-rust` (xxh3). Used by: `cargo`.
- `Cargo.lock`: generated lockfile. Depends on: `Cargo.toml`.
- `.cargo/config.toml` (gitignored, local only): `target-dir` outside the repo/OneDrive.
- `src/lib.rs`: `pub mod` declarations (`dedup`, `urlnorm`). Used by: `src/main.rs` (later), `tests/`.
- `src/main.rs`: binary entry point; still the placeholder "Hello, world!" (CLI comes in Task 12).
- `src/urlnorm.rs`: `normalize(base, href) -> Option<Url>` (resolve, drop fragment and empty query, http(s) only, ≤ 2048 chars, no path segment repeated > 3 times) and `host_key(&Url) -> Option<&str>` (lowercase hostname, the politeness key). Depends on: `url`. Used by (planned): `frontier`, `parse`, `crawler`. Tested by: `tests/urlnorm.rs`.
- `src/dedup.rs`: `url_hash`/`body_hash` (xxh3_64), `hamming`, `simhash` (64-bit SimHash over lowercase word 3-shingles; empty → 0), `SeenSet` (HashSet<u64> of URL hashes; `insert` → true if new), `NearDupIndex` (Manku 4×16-bit block tables; `find`/`insert`), `NEAR_DUP_BITS = 3`. Depends on: `url`, `xxhash-rust`. Used by (planned): `frontier` (SeenSet), `store` (NearDupIndex, hashes), `crawler`. Tested by: `tests/dedup.rs`.
- `tests/`: one integration-test file per module, against the public API (no test code in `src/`). Run one with `cargo test --test <module>`.
  - `tests/urlnorm.rs`: canonicalization table, trap/scheme rejections, host key.
  - `tests/dedup.rs`: seen-set dedup, SimHash near/far on a 2000-word fixture, block index hit/reject.
- `.gitignore`: ignores `/target`, `/.cargo/`, local envs, secrets, crawl output (`data/`, `output/`) and `*.log`.
- `CLAUDE.md`: workflow rules for Claude Code (WSL terminal, STATE.md-first workflow).
- `AGENTS.md`: same rules for non-Claude agents (Codex etc.); mirror rule changes into `CLAUDE.md`.
- `STATE.md`: this file, the knowledge graph.
- `docs/superpowers/specs/2026-11-18-crawl-core-design.md`: design for sub-project 1 (crawl core) plus the 5-sub-project roadmap, synced with the plan's refinements.
- `docs/superpowers/plans/2026-11-18-crawl-core.md`: the 15-task implementation plan, with tests and interfaces per task. The source of truth for build order.
