# Changelog

All notable changes to dnsglobe are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0] - 2026-08-07

### Changed

- The resolver table now fits an 80-column terminal without cropping
  anything that matters: resolver IPs are shown in full, the numeric columns
  (Ping, TTL, Exp) are right-aligned so their digits line up, units that the
  header already implies are gone, and where there isn't room for the
  spelled-out status the verdict moves to a glyph in the left margin
  (`✓ ≠ ! ↻ ∅ ✗`) — one place to scan for failures. The round-trip column is
  now headed `Ping` (it was `Time`). Wider terminals are unchanged: they keep
  the status word, the same answer column and the same map/globe thresholds.
  `--once` matches: it prints the same name/location/IP column widths as the
  TUI and right-aligns its TTL, so the two views line up.
  ([#33](https://github.com/514-labs/dnsglobe/issues/33),
  [#40](https://github.com/514-labs/dnsglobe/pull/40))
- The per-row expiry countdown is coarse: at most two digits and a unit
  (`59s`, `1m`, `59m`, `1h`, `23h`, `1d`, `99d`). A whole column of seconds
  ticking out of unison was a distraction, and above a minute the exact
  second never changed what you'd do. The TTL advisory notes still quote the
  precise figure (`TTL ≈ 2h23m`).
  ([#33](https://github.com/514-labs/dnsglobe/issues/33),
  [#40](https://github.com/514-labs/dnsglobe/pull/40))
- Failures now show as a white-on-red badge — on the status glyph and word in
  the table, and on the `not a domain name` label under a malformed input —
  rather than red text, which went washed-out on terminal themes with a
  mid-toned background (macOS Terminal's "Ocean"). Only the marker is
  filled — error messages, map dots, the propagation gauge and slow ping
  times keep the plain red, so the table doesn't turn into a wall of red
  bars. `theme.error` accepts the new `"<fg> on <bg>"` form (for example
  `error = "black on 208"`); a plain color still works and means no badge.
  ([#33](https://github.com/514-labs/dnsglobe/issues/33),
  [#40](https://github.com/514-labs/dnsglobe/pull/40))
- Anycast site discovery now asks every resolver for its NSID (RFC 5001)
  first — a standard EDNS option servers answer with their own node name —
  and only falls back to the old operator-specific `id.server` probes when
  that names no place. More resolvers report where they actually answered
  from: Lumen shows `→JFK`, CIRA Canadian Shield `→YYZ`, DNS4EU `→AMS` and
  DNS.SB `→KIX` where they used to show only the operator's home region.
  Resolvers you add yourself in the config file can now report a site too,
  since NSID needs no per-operator support, and Google's site takes one
  query instead of two. Nothing that already resolved to a site changed.
  ([#36](https://github.com/514-labs/dnsglobe/issues/36),
  [#39](https://github.com/514-labs/dnsglobe/pull/39))

### Fixed

- Names with an underscore in the middle of a label are queried instead of
  failing on every resolver with `protocol error: Label contains invalid
  character`. Underscores are legal anywhere in a DNS label — the
  letter-digit-hyphen rule is about *hostnames*, not the DNS wire format — so
  the delegated SPF/DMARC hosts that EasyDMARC, Valimail and friends generate
  (`_spf.514_ax._d.example.com`) now resolve like they do in `dig`. Default
  and `--ecs` runs accept exactly the same set of names; previously only
  `--ecs` handled these.
  ([#34](https://github.com/514-labs/dnsglobe/issues/34),
  [#37](https://github.com/514-labs/dnsglobe/pull/37))
- A domain that really is malformed (an empty label, a label over 63
  characters) is now reported once — as a startup error for a name given on
  the command line, or in place of the propagation gauge for one typed in the
  TUI — instead of filling the table with one identical error per resolver,
  which read like a network outage.
  ([#37](https://github.com/514-labs/dnsglobe/pull/37))
- The TTL note no longer lets a single resolver speak for the zone. It used to
  report the longest TTL any agreeing resolver returned, so one resolver
  handing back an invented 8423s turned a 300s zone into "TTL ≈ 2h23m" and
  advised lowering a TTL that was already low. The estimate now ignores
  reports wildly out of line with the rest of the fleet (never more than a
  tenth of it), and any resolver that reported one is named on the note line
  with what it claims — that cache really will serve the old answer after a
  change, and it is worth knowing which one it is.
  ([#35](https://github.com/514-labs/dnsglobe/issues/35),
  [#38](https://github.com/514-labs/dnsglobe/pull/38))

## [0.4.0] - 2026-07-11

### Added

- Resolvers can be added and removed at runtime: `+` opens an add-resolver
  dialog (name and IP, plus optional location and map coordinates, validated
  like the config file), ↑/↓ highlight a row in the table, and Ctrl+X
  removes the highlighted resolver. Changes last for the session; permanent
  resolvers still belong in the config file. A resolver added mid-watch is
  queried immediately and joins the propagation math.
  ([#30](https://github.com/514-labs/dnsglobe/pull/30))
- EDNS Client Subnet (RFC 7871) support: `--ecs 203.0.113.0/24` (or
  `ecs = [...]` in the config file) attaches that client subnet to every
  query, so GeoDNS zones answer for that network instead of each resolver's
  own vantage point. Takes CIDRs or bare IPs, and accepts several subnets to
  compare how answers converge across client networks: Ctrl+N cycles the
  active subnet in the TUI (re-querying as it goes), and `--once` prints one
  table per subnet plus a convergence summary. Resolvers that deliberately ignore ECS (Cloudflare,
  Quad9, …) are tagged `NO ECS` and left out of the propagation percentage —
  their answer describes their own location, not the probed network.
  ([#14](https://github.com/514-labs/dnsglobe/issues/14),
  [#29](https://github.com/514-labs/dnsglobe/pull/29))
- 3D rotating globe view: the flat resolver map can morph into a spinning
  orthographic globe, with the resolver status dots riding their continents
  and hiding on the far hemisphere as the planet turns. The globe keeps the
  map's braille rendering, adds a faint graticule so the rotation reads even
  over open ocean, and is tilted 15° so the northern-hemisphere resolver
  clusters stay clear of the limb.
  ([#26](https://github.com/514-labs/dnsglobe/pull/26))
- The view is responsive by default: the square-ish globe needs fewer
  columns than the 350°-wide flat map, so narrow terminals get the globe
  (down to widths that previously showed no map at all) and wide ones the
  flat map — resizing across the threshold animates the morph live. Ctrl+O
  toggles by hand and pins the choice; `--view auto|map|globe` or `view =
  "..."` in the config file force it outright (flag beats config).
  ([#26](https://github.com/514-labs/dnsglobe/pull/26))
- Nix flake support: `nix run github:514-labs/dnsglobe` builds and runs
  dnsglobe from source on any system with Nix flakes enabled; specific
  releases can be pinned via git tag (`github:514-labs/dnsglobe/v0.3.0`).
  A `devbox.json` is included for reproducible development environments.
  ([#10](https://github.com/514-labs/dnsglobe/issues/10),
  [#21](https://github.com/514-labs/dnsglobe/pull/21))
- Custom color themes: a `[theme]` table in the config file recolors any UI
  role (`accent`, `agree`, `differ`, `error`, `pending`, `stale`,
  `upstream`, `muted`, `coastline`, `grid`) using ANSI color names,
  256-color indexes, or hex values.
  ([#27](https://github.com/514-labs/dnsglobe/pull/27))

### Changed

- ↑/↓ (and PageUp/PageDown) move a highlight through the resolver table
  instead of scrolling it directly; the view scrolls along to keep the
  highlight visible. ([#30](https://github.com/514-labs/dnsglobe/pull/30))
- Tab / Shift-Tab now re-query the checked domain with the newly selected
  record type right away, instead of waiting for Enter — matching the new
  Ctrl+N behavior for ECS subnets.
  ([#29](https://github.com/514-labs/dnsglobe/pull/29))
- The default palette now stays legible on terminal themes with mid-toned
  backgrounds, like macOS Terminal's "Ocean": de-emphasized text uses the
  faint attribute instead of dark gray (dimming your theme's own foreground
  color, which is always readable), and status colors moved to the bright
  ANSI range. Past-TTL is now orange to stay distinct from the brighter
  error red. Set `muted = "darkgray"` in `[theme]` to restore the old
  de-emphasis on terminals that render faint poorly.
  ([#25](https://github.com/514-labs/dnsglobe/issues/25),
  [#27](https://github.com/514-labs/dnsglobe/pull/27))

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

[Unreleased]: https://github.com/514-labs/dnsglobe/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/514-labs/dnsglobe/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/514-labs/dnsglobe/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/514-labs/dnsglobe/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/514-labs/dnsglobe/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/514-labs/dnsglobe/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/514-labs/dnsglobe/releases/tag/v0.1.0
