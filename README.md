# McCrawl

A high-performance web crawler: an async Rust crawl engine, with Python ML for crawl prioritization and search on top.

It focuses on the hard parts of crawling at scale:
- **Throughput:** thousands of hosts in flight on one machine.
- **Politeness:** per-host rate limits that adapt to how each server responds, and RFC 9309 robots.txt.
- **Dedup:** canonical URLs plus SimHash near-duplicate detection.
- **Archival output:** standard WARC files.

> **Status:** sub-project 1 (crawl core) is in progress, 2 of 15 tasks done. The full design is in [`docs/superpowers/specs/2026-11-18-crawl-core-design.md`](docs/superpowers/specs/2026-11-18-crawl-core-design.md) and the task-by-task plan in [`docs/superpowers/plans/2026-11-18-crawl-core.md`](docs/superpowers/plans/2026-11-18-crawl-core.md).

## What the crawler does

### Pipeline

A single scheduler task owns the frontier, so the frontier needs no locks. The scheduler hands jobs to async fetch tasks. HTML goes to a CPU-bound parse pool; every record goes to a single store writer. Links found during parsing flow back into the frontier.

```mermaid
flowchart LR
    seeds(["seed URLs"]) --> F

    subgraph SCHED["Scheduler task: owns the frontier, no locks"]
        F["Frontier<br/>per-host queues<br/>waiting heap · ready heap"]
    end

    F -- "next(now): robots.txt or page job" --> FT["Fetch tasks ×N<br/>async reqwest, 2 MiB cap"]
    FT -- "outcome + latency, redirect target" --> F
    FT -- "HTML, bounded channel = backpressure" --> P["Parse pool<br/>lol_html streaming, ≤ 1 per core"]
    FT -- "errors, non-HTML: record only" --> ST
    P -- "links → normalize → seen-set" --> F
    P -- "page record + SimHash" --> ST["Store thread<br/>single writer"]
    ST --> OUT[("crawl-NNNNN.warc.gz<br/>pages.jsonl · report.json")]
```

### Politeness: the life of a host in the frontier

Every host is in exactly one place at a time:
- the **waiting heap** (sorted by when it may next be contacted);
- the **ready heap** (sorted by the priority of its best queued URL);
- **in flight** (at most one request per host).

The waiting heap decides *when* a host may be contacted, and the ready heap decides *which* allowed host is worth the most. This is a deterministic variant of Mercator's front/back queues.

```mermaid
stateDiagram-v2
    [*] --> Waiting: first URL seen, next_allowed = now
    Waiting --> Ready: now ≥ next_allowed
    Ready --> InFlight: popped, best priority first
    InFlight --> Waiting: done, next_allowed = now + delay
    InFlight --> Idle: done, queue empty
    Idle --> Waiting: new URL pushed
    InFlight --> Dead: 5 failures in a row, or robots.txt down 3 times
    Dead --> [*]

    note right of Ready
        A host's first job is always its robots.txt,
        refetched after 24 h. Disallowed URLs are
        dropped, and so is the queue past the per-host cap.
    end note
```

The delay after each response adapts to the server:

```
delay = max(robots Crawl-delay, min_delay = 1 s, 5 × response_time) × backoff

429 / 503    → backoff × 2 (capped at 10 min), Retry-After always honored
success      → backoff ÷ 2 (down to 1)
robots.txt   → 2xx: obey · 4xx: allow all · 5xx/unreachable: fetch nothing, retry in 60 s, then 120 s
```

### Dedup and trap defenses

| Layer | How |
|---|---|
| Same URL spelled differently | WHATWG parse, lowercase host, drop default port, fragment and empty query (`urlnorm`) |
| Crawler traps | URL > 2048 chars, or any path segment repeated > 3 times (`/a/b/a/b/a/b/a/`); ≤ 5 redirect hops; per-host page cap |
| Same URL twice | 64-bit xxh3 seen-set |
| Identical body | xxh3 body hash |
| Near-identical body | 64-bit SimHash over word 3-shingles; Hamming distance ≤ 3, found via a 4 × 16-bit block index (Manku et al., 2007), not a linear scan |

## The plan

### Roadmap

Each sub-project gets its own spec → plan → build cycle.

```mermaid
flowchart LR
    SP1["1 · Crawl core<br/>Rust<br/>efficiency, politeness"]:::wip
    SP2["2 · Distribution<br/>host-hash sharding,<br/>disk frontier, checkpoints"]
    SP3["3 · Focused-crawl ML<br/>score URLs before fetch;<br/>harvest rate vs BFS"]
    SP4["4 · Dynamic content<br/>JS-need detection,<br/>headless Chrome"]
    SP5["5 · Knowledge layer<br/>quality, embeddings,<br/>search"]
    SP1 --> SP2 --> SP3 --> SP4 --> SP5

    classDef wip fill:#fff3cd,stroke:#d39e00,color:#000
```

### Sub-project 1 build order

Each box is one task, built test-first. An arrow `A → B` means B uses A. Green means done.

```mermaid
flowchart TD
    urlnorm["1 · urlnorm<br/>canonical URLs, trap guards"]:::done
    dedup["2 · dedup<br/>seen-set, SimHash, block index"]:::done
    robots["3 · robots<br/>RFC 9309 rules"]
    frontier["4–5 · frontier<br/>two heaps, politeness timing"]
    parse["6 · parse<br/>streaming links, text, meta"]
    mockweb["7 · mockweb<br/>deterministic synthetic web"]
    fetch["8 · fetch<br/>reqwest, body cap, error classes"]
    store["9 · store<br/>WARC + JSONL, near-dup marking"]
    metrics["10 · metrics<br/>tally, percentiles, report"]
    crawler["11 · crawler<br/>scheduler actor + pipeline"]
    cli["12 · CLI<br/>crawl · mockweb · bench"]
    proof["13 · politeness proof<br/>+ warcio validation"]
    bench["14 · benchmarks<br/>+ samply profiling"]
    real["15 · real crawl<br/>technical seed list"]

    urlnorm --> frontier
    dedup --> frontier
    robots --> frontier
    urlnorm --> parse
    dedup --> store
    frontier --> crawler
    parse --> crawler
    fetch --> crawler
    store --> crawler
    metrics --> crawler
    crawler --> cli
    mockweb --> cli
    cli --> proof --> bench --> real

    classDef done fill:#d4edda,stroke:#28a745,color:#000
```

## Layout

```
src/        the crate: one module per box above (lib.rs declares them, main.rs is the CLI)
tests/      one test file per module, against the public API
docs/       design spec and implementation plan
```

## Running

```sh
cargo test                 # everything
cargo test --test dedup    # one module
```

The `crawl`, `mockweb` and `bench` commands arrive with Task 12.
