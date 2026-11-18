# STATE.md — knowledge graph

**Read this before touching code. Update it before finishing.** This replaces scanning the directory.

## Status

- **2026-11-18:** McCrawl — a high-performance web crawler in Rust, with a Python ML side to come. Only the `cargo init` skeleton exists so far; it builds and prints "Hello, world!" (checked with `cargo run` on Windows and `cargo build` in WSL). WSL toolchain: cargo 1.97.0, Python 3.14.4, uv. Brainstorming done: project split into 5 sub-projects, and the sub-project 1 (Rust crawl core) design spec is written and awaiting user review. Next: user reviews the spec → implementation plan (writing-plans / plan mode) → build file by file.

## Files

- `Cargo.toml`: crate manifest, package `mccrawl`, edition 2024, no dependencies yet. Used by: `cargo`.
- `Cargo.lock`: generated lockfile. Depends on: `Cargo.toml`.
- `src/main.rs`: binary entry point; prints "Hello, world!" (placeholder). Depends on: nothing.
- `.gitignore`: ignores `/target`, local envs, secrets, crawl output (`data/`, `output/`) and `*.log`.
- `CLAUDE.md`: workflow rules for Claude Code (WSL terminal, STATE.md-first workflow).
- `AGENTS.md`: same rules for non-Claude agents (Codex etc.); mirror rule changes into `CLAUDE.md`.
- `STATE.md`: this file — the knowledge graph.
- `docs/superpowers/specs/2026-11-18-crawl-core-design.md`: approved design for sub-project 1 (crawl core), plus the 5-sub-project roadmap. Source of truth for what `src/` will contain. Used by: the upcoming implementation plan.
