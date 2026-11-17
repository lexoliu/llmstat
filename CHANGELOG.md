# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.5](https://github.com/lexoliu/llmstat/compare/v0.2.4...v0.2.5) - 2026-11-17

### Fixed

- resolve Gemini 4 pricing and Antigravity model identification ([#77](https://github.com/lexoliu/llmstat/pull/77)) ([#78](https://github.com/lexoliu/llmstat/pull/78))

### Other

- pass a commit SHA to gh release create --target
- build aarch64-apple-darwin on macos-15

## [0.2.4](https://github.com/lexoliu/llmstat/compare/v0.2.3...v0.2.4) - 2026-11-15

### Added

- grok and antigravity usage sources ([#66](https://github.com/lexoliu/llmstat/pull/66))
- serve — headless JSON snapshot of live stats ([#62](https://github.com/lexoliu/llmstat/pull/62))
- host mirrors + `llmstat sync` — count other machines' usage ([#61](https://github.com/lexoliu/llmstat/pull/61))
- watch — tiled read-only tails of every running agent ([#60](https://github.com/lexoliu/llmstat/pull/60))
- shell completions — dynamic + static script
- --filter/-f expression filter over calls

### Fixed

- *(speedtest)* count antigravity thinking in TTFT, not decode tok/s ([#56](https://github.com/lexoliu/llmstat/pull/56))

### Other

- open the release-plz PR after Test on main
- release from main and retire dev
- attest release artifacts with build provenance
- drop the x86_64-apple-darwin release target
- *(changelog)* drop hand-written Unreleased entries

### Added

- `llmstat sync <host>`: mirror another machine's usage data over ssh into
  `~/.local/share/llmstat/hosts/<host>/` (claude projects, codex sessions,
  devin transcripts + a `sqlite3 .backup` snapshot of sessions.db); all
  reports and `monitor` count every mirrored host automatically,
  deduplicated against local data
- `llmstat serve`: headless live-stats emitter — `monitor`'s scanners minus
  the TUI, writing a JSON snapshot (tok/s windows, per-source rates, fresh
  Claude usage-limit hits) to `~/.cache/llmstat/live.json` every tick

### Changed

- the devin/claude/codex sources accept multiple roots internally — one
  scanner covers the local dirs plus every host mirror, sharing dedup state

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
