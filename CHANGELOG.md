# Changelog

All notable changes to ctxguard are documented in this file.

## [0.1.0] - 2026-07-18

### Added
- `ctxguard parse <file>` — single Claude Code session JSONL → token summary with
  effective_context column (input + cache_read + cache_creation).
- `ctxguard profile [--days N]` — aggregate token usage across `~/.claude/projects/`,
  with `--by model|day|hour|file` for top-N breakdowns.
- `ctxguard run --budget=N --on-full=warn|compress|kill -- <cmd>` — wraps a live
  agent run, watches the session JSONL via `notify`, fires `--on-full` action when
  cumulative effective_context crosses the budget.
- 4 platform CI (ubuntu/macos/windows × stable rustc + clippy)
- GitHub Actions release workflow — on `v*` tag push, builds + uploads cross-platform
  tarballs and zips to GitHub Releases
- `bench.sh` — real benchmark vs `ccusage` (measured 810× faster on 14 MB session)

### Real-data results
- 7 days of one user's sessions: 2.1 B context tokens, top session 558 M (2790× the
  200k standard window), cache_read accounts for ~95% of every long session.

[0.1.0]: https://github.com/zhuke-ai/ctxguard/releases/tag/v0.1.0
