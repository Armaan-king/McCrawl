# AGENTS.md — instructions for non-Claude agents (Codex, etc.)

You are subbing in for Claude Code, usually because it ran out of credits. Same project, same rules. Your role: critical thinker — question the approach, catch what was missed, but keep the diff small.

## Terminal: WSL (Ubuntu)

All shell commands for this project run in **WSL Ubuntu**, not PowerShell or Git Bash. Install libraries, run code, and run git from WSL.

- Project path in WSL: `/mnt/c/Users/iidab/OneDrive/Desktop/McCrawl`
- If your shell is Windows-side, wrap commands as: `wsl -d Ubuntu -e bash -lc "cd /mnt/c/Users/iidab/OneDrive/Desktop/McCrawl && <command>"`
- Git in WSL shares the Windows credential manager — no separate login.
- Install where you run: everything goes into Ubuntu (apt/pip/npm inside WSL), never into Windows.

## Workflow (every prompt)

1. **Read `STATE.md` first.** It is the knowledge graph: every file, what it does, and what it depends on / is depended on by. Use it to find the files you need. Do NOT list or scan the whole directory — that wastes credits.
2. Open only the files `STATE.md` points you to.
3. Do the work.
4. **Update `STATE.md` before finishing** — add new files, update changed dependencies, remove deleted files. If it isn't in `STATE.md`, it doesn't exist to the next session.

## Rules

- `STATE.md` is the source of truth for project structure. Keep it accurate; a stale graph costs more than no graph.
- When a change touches file A, check `STATE.md` for everything that depends on A before editing. Fix all affected files in the same pass.
- Shortest working diff. No speculative abstractions, no scaffolding "for later".
- Tests go in `tests/<module>.rs` (integration tests against the public API). No `#[cfg(test)]` code in `src/`.
- Cover edge cases, not just the happy path: hostile input, limits, overflow, failure and recovery paths. For every bug fix and core rule, break the code on purpose, confirm a test fails, then restore it (a mutation check).
- Don't re-read files already summarised in `STATE.md` unless you're editing them.
- If something is unclear, ask instead of assuming.
- `CLAUDE.md` holds the same rules for Claude Code. If you change a rule here, mirror it there.

## Hand-off back to Claude

Leave `STATE.md` accurate and note anything half-finished in its **Status** section so Claude can resume without re-reading the tree.
