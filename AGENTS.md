# Working on dnsglobe

dnsglobe is a Rust TUI (ratatui + crossterm + hickory-resolver) that watches
a DNS record propagate across public resolvers worldwide. This file describes
the process for building a new feature; it applies to any coding agent or
human contributor.

## Module map

| File | Owns |
|---|---|
| `src/main.rs` | CLI (clap), event loop, key handling, `--once` mode |
| `src/app.rs` | `App` state, input editing, answer grouping, TTL verdicts |
| `src/ui.rs` | All rendering: map, table, gauge, footer |
| `src/dns.rs` | Resolver queries and error mapping |
| `src/resolvers.rs` | Built-in resolver list |
| `src/config.rs` | TOML config file (custom resolvers) |
| `src/sites.rs` | Anycast POP discovery (id.server probes, IATA codes) |

Keep logic in `app.rs` (testable, no I/O) and rendering in `ui.rs`. Key
bindings live in `handle_key` in `main.rs` and should call one-line `App`
methods rather than mutating state inline.

## Process for a new feature

1. **Branch off `main`.** Direct pushes to `main` are blocked by an org
   ruleset — all changes land via PR.

2. **Implement, with state and rendering separated.** New behavior goes in
   `App` methods; `ui.rs` only reads state. Comments explain *why* (protocol
   quirks, terminal compatibility, DNS semantics), not what the code does —
   match the density and tone of the surrounding code.

3. **Unit-test the logic.** Tests live in `#[cfg(test)] mod tests` at the
   bottom of the same file. Anything in `app.rs` should be testable without
   a terminal or network.

4. **Run the quality gates locally** — CI enforces exactly these on every PR:

   ```sh
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```

5. **Verify end-to-end, not just with tests.**
   - Query/output logic: `cargo run -- example.com --once` prints a plain
     table without needing a TTY.
   - TUI behavior (key bindings, rendering, watch mode): drive the real
     binary through a PTY, e.g. with `expect` or tmux, and assert on the
     rendered output. Two PTY gotchas: an `expect`-spawned PTY starts with
     zero size (fix with `stty rows 30 columns 110 < $spawn_out(slave,name)`
     after spawn), and Tcl's `\x` escape eats all following hex digits —
     write `Esc` as `\033`, never `\x1b` followed by a hex-looking char.

6. **Keep the UI discoverable.** A new key binding must appear in the footer
   hint line in `draw_footer` (`src/ui.rs`). Mind macOS/Windows/Linux
   differences: Option on macOS arrives as ALT (or as `Esc b`/`Esc f` from
   Terminal.app); Cmd (SUPER) only arrives under the kitty keyboard
   protocol, so always provide a universal fallback (Home/End, Ctrl+letter).

7. **Update `CHANGELOG.md`.** Add an entry under `[Unreleased]` in the
   matching Keep-a-Changelog section (`Added`/`Changed`/`Fixed`), written
   for users, ending with a link to the PR.

8. **Refresh the demo if visuals changed.** The README GIF is recorded with
   [vhs](https://github.com/charmbracelet/vhs) from `demo/demo.tape`.

9. **Open a PR against `main`.** Describe what changed, how it works, and
   how it was verified (including the end-to-end check from step 5). CI must
   pass before merge.

## Releases

Releases are cut with a version-bump PR:

1. Bump `version` in `Cargo.toml` and run `cargo check` so `Cargo.lock`
   picks up the new version.
2. In `CHANGELOG.md`, rename `[Unreleased]` to the new version with today's
   date and add a fresh empty `[Unreleased]` heading above it.
3. Open a PR with just those changes and merge it.

On merge, `tag-release.yml` sees the new (untagged) version on `main` and
dispatches the dist-generated `release.yml`, which builds binaries, publishes
to crates.io (`publish-crates.yml`, needs the `CARGO_REGISTRY_TOKEN` secret)
and the Homebrew tap, and creates the `v*` tag and GitHub release. A release
can also be re-run or cut manually from the Actions tab by dispatching
`release.yml` with the tag.

Feature PRs never touch versioning — just leave the changelog entry under
`[Unreleased]`.
