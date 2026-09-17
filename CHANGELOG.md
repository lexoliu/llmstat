# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `--filter`/`-f` expression filter over calls — model, family, source, session, date, token counts, cost, and free/paid/unpriced/estimated flags with and/or/not logic — for reports and `monitor`
- shell completions: dynamic via `COMPLETE=<shell> llmstat` (completes `--filter` expressions and `speedtest --effort`), static scripts via `llmstat completions <shell>`

## [0.2.3](https://github.com/lexoliu/llmstat/compare/v0.2.2...v0.2.3) - 2026-09-13

### Added

- rename report subcommands to daily/weekly/monthly
- "did you know" energy footer on report commands

### Fixed

- *(devin)* emit cached sessions.db calls on first tick ([#45](https://github.com/lexoliu/llmstat/pull/45))
- *(monitor)* white session text, O(1) hit test, unstarvable ticks ([#38](https://github.com/lexoliu/llmstat/pull/38))

### Other

- *(readme)* plain rewrite, transparent screenshot ([#42](https://github.com/lexoliu/llmstat/pull/42))
- *(readme)* badges, monitor screenshot, features list ([#40](https://github.com/lexoliu/llmstat/pull/40))
- update install section for dist-built releases

## [0.2.2](https://github.com/lexoliu/llmstat/compare/v0.2.1...v0.2.2) - 2026-09-13

### Other

- release-plz publish + cargo-dist prebuilt binaries ([#25](https://github.com/lexoliu/llmstat/pull/25))
