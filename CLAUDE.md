# CLAUDE.md — instructions for Claude Code

## Terminal: WSL (Ubuntu)

All shell commands for this project run in **WSL Ubuntu**, not PowerShell or Git Bash. Install libraries, run code, and run git from WSL.

- Project path in WSL: `/mnt/c/Users/iidab/OneDrive/Desktop/McCrawl`
- From Claude Code's shell tool, wrap commands as: `wsl -d Ubuntu -e bash -lc "cd /mnt/c/Users/iidab/OneDrive/Desktop/McCrawl && <command>"`
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

## Hand-off

Other agents (see `AGENTS.md`) take over when Claude runs out of credits. They follow the same `STATE.md` workflow. Leave `STATE.md` in a state you'd want to inherit.
