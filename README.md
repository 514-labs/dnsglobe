# dnsglobe

[![crates.io](https://img.shields.io/crates/v/dnsglobe)](https://crates.io/crates/dnsglobe)
[![license](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

**A global DNS propagation checker for your terminal** — a Rust TUI that
queries 34 public DNS resolvers around the world in parallel, compares their
answers, and shows the propagation of your record on a world map.

![dnsglobe demo — checking A and NS records for a domain across 34 resolvers worldwide](demo/demo.gif)

Think dnschecker.org / whatsmydns.net, but in your terminal, with watch mode:
start a check and it re-polls until the record has propagated everywhere.

Resolvers span the global anycast networks (Google, Cloudflare, Quad9),
North America, Europe, Russia, the Middle East, East Asia, and the southern
hemisphere (Telstra AU, SafeSurfer NZ, UOL BR) — each queried directly, so
you see every server's own current view of the record.

Each resolver is queried directly (no cache, EDNS0, TCP fallback for
truncated answers), so what you see is each server's own current view of the
record. Answers sharing any record are grouped together — so round-robin DNS
(each resolver caching a different subset of an IP pool) counts as one
consistent answer, not twenty conflicting ones. The propagation gauge shows
how many resolvers are in the majority group; outliers are flagged
`≠ DIFFERS` once all results are in.

On terminals ≥150 columns wide, a world map appears on the right with one
dot per resolver, colored by status (green agrees, magenta differs, red
error, yellow in flight).

## Usage

[![dnsglobe](https://img.shields.io/aur/version/dnsglobe?label=dnsglobe)](https://aur.archlinux.org/packages/dnsglobe/)
[![dnsglobe-bin](https://img.shields.io/aur/version/dnsglobe-bin?label=dnsglobe-bin)](https://aur.archlinux.org/packages/dnsglobe-bin/)

Install:

```sh
brew install 514-labs/tap/dnsglobe   # Homebrew (macOS/Linux)
cargo install dnsglobe               # from crates.io
yay -S dnsglobe                      # from archlinux aur (compile from source)
yay -S dnsglobe-bin                  # from archlinux aur (install prebuilt binary)
# or grab a prebuilt binary from the GitHub Releases page
```

Run:

```sh
dnsglobe                            # start empty, type a domain
dnsglobe example.com                # query immediately and watch
dnsglobe --once example.com TXT    # no TUI: print results, exit (for scripts)
```

### Keys

| Key            | Action                          |
| -------------- | ------------------------------- |
| type / ⌫ / Del | edit domain                     |
| ←/→ / Home/End | move cursor in the domain field |
| Enter          | start the check and watch: re-polls every 30 s until propagation reaches 100% |
| Ctrl+R         | stop or resume watching         |
| Tab / Shift-Tab | select record type (A, AAAA, CNAME, MX, NS, TXT, SOA) |
| ↑/↓ / PgUp/PgDn | scroll the resolver table |
| Ctrl+S         | cycle table sort: resolver / location / time / status / answer |
| Ctrl+U         | clear domain                    |
| Esc / Ctrl+C   | quit                            |

## Notes

- Several resolvers are anycast networks, so the responding node is the one
  nearest to you; the location column is the operator's home region.
- Resolver list lives in `src/resolvers.rs` — add or remove entries freely.
  Every entry was verified to answer external queries; many well-known ISP
  resolvers (and, notably, all major African ones) refuse queries from
  outside their network, so they can't be included.
