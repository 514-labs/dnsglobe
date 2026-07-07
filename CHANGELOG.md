# Changelog

All notable changes to dnsglobe are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.1] - 2026-07-06

### Fixed

- SERVFAIL answers now count as a propagation signal instead of being
  discarded as unreachable. A resolver answering SERVFAIL is saying "I tried
  to resolve this name and could not" — the exact state of a resolver stuck
  on a delegation whose old nameservers were deleted mid-NS-migration (or a
  DNSSEC validation failure). Such resolvers now hold the propagation
  percentage below 100% and keep watch mode polling; they show as
  `✗ SERVFAIL` in the table (`FAIL` in `--once` output) with their own
  footer count. Previously they were lumped in with timeouts/refusals, so a
  broken delegation could report as fully propagated.
  ([#23](https://github.com/514-labs/dnsglobe/pull/23))

## [0.3.0] - 2026-07-06

### Added

- Word and line cursor motions in the domain input: Option/Alt+←/→ (or
  Ctrl+←/→, or Alt+B/F) moves the cursor by one dot-separated label;
  Cmd+←/→ (or Home/End, or Ctrl+A/E) jumps to the start/end of the input.
  Cmd is reported via the kitty keyboard protocol, enabled when the
  terminal supports it. ([#17](https://github.com/514-labs/dnsglobe/pull/17))
- Argument parsing via clap: `--help` and `--version` now work, invalid
  arguments get proper error messages, and an optional record-type
  positional (`dnsglobe example.com TXT`) sets the starting record type in
  TUI mode too — previously it was only honored with `--once`. The long
  `--help` also documents `$DNSGLOBE_CONFIG` and the config-file syntax.
- Anycast site geolocation: large anycast resolvers (Quad9, Cloudflare,
  Google, OpenDNS, CleanBrowsing, Neustar UltraDNS) are asked which POP is
  answering via identification queries (`id.server` CH TXT and
  operator-specific probes). The discovered site shows in the Loc column
  (e.g. `→YUL`) and the resolver's map dot moves to the POP actually serving
  you. ([#13](https://github.com/514-labs/dnsglobe/pull/13))
- Cache-expiry countdowns: a live `Exp` column next to the static TTL shows
  when each resolver's cache entry must be refetched; the propagation gauge
  shows the fleet-wide bound ("old answers expire in ≤ X"). Watch mode skips
  re-polling resolvers that agree with the majority until their TTL runs out.
  ([#12](https://github.com/514-labs/dnsglobe/pull/12))
- TTL advisory: once a round settles with full agreement and the zone's TTL
  is ≥ 1 hour, a footer hint suggests lowering the TTL before a planned
  record change. ([#12](https://github.com/514-labs/dnsglobe/pull/12))
- Stale-cache verdicts in watch mode: `! PAST TTL` flags a resolver serving
  an answer past its own reported TTL, `↻ UPSTREAM` flags one that refetched
  and still got old data back (e.g. a lagging secondary authoritative
  server). Both surface in the status column and on the map.
  ([#12](https://github.com/514-labs/dnsglobe/pull/12))
- TOML config file for custom resolvers: add to (or replace) the built-in
  list via `~/.config/dnsglobe/config.toml` (XDG-aware, `DNSGLOBE_CONFIG`
  override). Entries take a name and IPv4/IPv6 address, plus optional
  location and lat/lon for the world map; invalid config is rejected at
  startup with the offending entry named.
  ([#11](https://github.com/514-labs/dnsglobe/pull/11))
- CI workflow running `cargo fmt`, `clippy`, and tests on PRs and main.
  ([#11](https://github.com/514-labs/dnsglobe/pull/11))
- README: Arch Linux AUR package information.
  ([#8](https://github.com/514-labs/dnsglobe/pull/8))

### Changed

- Releases are now cut by merging a version-bump PR: the release workflow is
  dispatched automatically and publishes to crates.io (new) as well as
  Homebrew, so `cargo install dnsglobe` stays current with each release.
  ([#19](https://github.com/514-labs/dnsglobe/pull/19))

## [0.2.0] - 2026-07-05

### Added

- Table sorting: `Ctrl+S` cycles sort order across resolver / location /
  time / status / answer; sorts are stable, so sorting by location doubles
  as group-by-location. ([#4](https://github.com/514-labs/dnsglobe/pull/4))
- README demo GIF (recorded with vhs) and crates.io badge.
  ([#3](https://github.com/514-labs/dnsglobe/pull/3))

### Changed

- Rust edition 2021 → 2024 (rust-version 1.85) and dependencies updated to
  latest: ratatui 0.30, crossterm 0.29, hickory-resolver 0.26.
  ([#4](https://github.com/514-labs/dnsglobe/pull/4))

### Fixed

- Error-message truncation panicked on multi-byte UTF-8 characters
  straddling the cut point; truncation now backs off to a char boundary.
  ([#4](https://github.com/514-labs/dnsglobe/pull/4))

## [0.1.1] - 2026-07-04

### Added

- Release pipeline via dist (cargo-dist): every `v*` tag builds prebuilt
  binaries for macOS (arm64/x64), Linux (arm64/x64), and Windows, attaches
  them to the GitHub release, and publishes a Homebrew formula to
  `514-labs/homebrew-tap`. First release with prebuilt binaries.
  ([#1](https://github.com/514-labs/dnsglobe/pull/1))

## [0.1.0] - 2026-07-04

Initial release.

### Added

- Global DNS propagation checker TUI: queries 34 verified public resolvers
  worldwide in parallel and compares their answers to show how far a record
  has propagated, in a scrollable table with per-resolver latency, TTL,
  status, and answers.
- World map with per-resolver status dots, colored by whether the answer
  agrees with the majority.
- Answer-overlap grouping, so round-robin pools count as one answer;
  refused/unreachable resolvers are excluded from the propagation
  percentage.
- Record-type selector (A, AAAA, CNAME, MX, NS, TXT, SOA) and watch mode
  that re-polls every 30s until all responding resolvers agree.
- `--once` plain-text mode for scripts.

[Unreleased]: https://github.com/514-labs/dnsglobe/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/514-labs/dnsglobe/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/514-labs/dnsglobe/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/514-labs/dnsglobe/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/514-labs/dnsglobe/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/514-labs/dnsglobe/releases/tag/v0.1.0
