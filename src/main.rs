mod app;
mod config;
mod dns;
mod globe;
mod resolvers;
mod sites;
mod theme;
mod ui;
mod world_data;

use std::net::IpAddr;
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use hickory_resolver::proto::rr::RecordType;
use tokio::sync::mpsc;

use app::{App, POLL_INTERVAL};
use dns::{ClientSubnet, QueryOutcome};

const CONFIG_HELP: &str = "\
Configuration:
  An optional TOML file adds resolvers to the built-in list, or replaces it
  entirely (replace = true). Looked up at $DNSGLOBE_CONFIG (error if the file
  is missing), else $XDG_CONFIG_HOME/dnsglobe/config.toml, else
  ~/.config/dnsglobe/config.toml.

  # view = \"globe\"       # map panel style: auto (default) | map | globe
  # replace = true       # use only the resolvers below, drop the built-ins
  # ecs = [\"203.0.113.0/24\"]  # EDNS Client Subnet(s) to query with;
  #                      #   cycle with Ctrl+N (--ecs overrides the list)
  [[resolvers]]
  name = \"Corp DNS\"      # required
  ip = \"10.0.0.53\"       # required, IPv4 or IPv6
  location = \"HQ\"        # optional; shown in the Loc column
  lat = 40.7             # optional map position;
  lon = -74.0            # give both or neither

  [theme]                # optional; override any UI color role
  # accent = \"lightcyan\" # roles: accent, agree, differ, error, pending,
  # stale = \"208\"        #   stale, upstream, muted, coastline, grid
  # muted = \"faint\"      # colors: ANSI names (\"lightred\"), 256-color
  #                      #   indexes (\"208\"), or hex (\"#ff8700\"); `muted`
  #                      #   also takes \"faint\" (dim the default foreground)";

/// Global DNS propagation checker TUI — watch a DNS record propagate across
/// public resolvers worldwide, on a world map in your terminal.
#[derive(Parser)]
#[command(
    version,
    after_help = "Configuration: custom resolvers via $DNSGLOBE_CONFIG or \
                  ~/.config/dnsglobe/config.toml (see --help for the syntax)",
    after_long_help = CONFIG_HELP
)]
struct Cli {
    /// Domain to start checking immediately
    domain: Option<String>,

    /// Record type to query: A, AAAA, CNAME, MX, NS, TXT or SOA [default: A]
    #[arg(value_parser = parse_record_type)]
    record_type: Option<RecordType>,

    /// Run a single check, print a plain-text table, and exit (no TTY needed)
    #[arg(long, requires = "domain")]
    once: bool,

    /// Map panel style: auto picks the globe on narrow terminals and the
    /// flat map on wide ones [overrides the config file's `view`]
    #[arg(long, value_enum)]
    view: Option<app::ViewMode>,

    /// Query with this EDNS Client Subnet (RFC 7871) to see the zone as a
    /// specific client network does (GeoDNS/split answers). CIDR or bare IP;
    /// most resolvers use at most /24 (IPv4) or /56 (IPv6), and some ignore
    /// ECS entirely (marked "no ecs"). Repeat or comma-separate to compare
    /// several networks: Ctrl+N cycles in the TUI, --once prints every one
    /// [overrides the config file's `ecs`]
    #[arg(long, value_delimiter = ',', value_parser = dns::parse_ecs)]
    ecs: Vec<ClientSubnet>,
}

fn parse_record_type(s: &str) -> Result<RecordType, String> {
    RecordType::from_str(&s.to_uppercase())
        .ok()
        .filter(|rtype| app::RECORD_TYPES.contains(rtype))
        .ok_or_else(|| {
            let supported: Vec<String> = app::RECORD_TYPES.iter().map(|t| t.to_string()).collect();
            format!(
                "unsupported record type (expected one of {})",
                supported.join(", ")
            )
        })
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Fail on a broken config before the terminal enters raw mode, so the
    // error prints normally.
    let settings = config::load()?;
    let view = cli.view.or(settings.view).unwrap_or_default();
    let ecs_list = if cli.ecs.is_empty() {
        settings.ecs
    } else {
        cli.ecs
    };
    resolvers::init(settings.resolvers);
    theme::init(settings.theme);

    // `--once` runs a single check and prints plain text — handy for scripts
    // and for testing without a TTY.
    if cli.once {
        let domain = cli.domain.expect("clap enforces `requires`");
        return run_once(domain, cli.record_type.unwrap_or(RecordType::A), ecs_list).await;
    }

    let terminal = ratatui::init();
    // Ask for the kitty keyboard protocol where supported (iTerm2, kitty,
    // Ghostty, WezTerm, ...): it's the only way terminals report Cmd (SUPER)
    // and reliably distinguish Option-modified arrows for input navigation.
    let enhanced_keys = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced_keys {
        let _ = crossterm::execute!(
            std::io::stdout(),
            event::PushKeyboardEnhancementFlags(
                event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        );
    }
    let result = run_tui(
        terminal,
        cli.domain.unwrap_or_default(),
        cli.record_type,
        view,
        ecs_list,
    )
    .await;
    if enhanced_keys {
        let _ = crossterm::execute!(std::io::stdout(), event::PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
    result
}

async fn run_tui(
    mut terminal: ratatui::DefaultTerminal,
    initial_domain: String,
    initial_rtype: Option<RecordType>,
    view: app::ViewMode,
    ecs_list: Vec<ClientSubnet>,
) -> Result<()> {
    let auto_query = !initial_domain.is_empty();
    let mut app = App::new(initial_domain);
    app.view_mode = view;
    app.set_ecs_list(ecs_list);
    if let Some(rtype) = initial_rtype {
        app.rtype_idx = app::RECORD_TYPES
            .iter()
            .position(|t| *t == rtype)
            .unwrap_or(0);
    }

    // Worker tasks send results here; keeping `tx` alive in this scope means
    // `rx.recv()` never observes a closed channel.
    let (tx, mut rx) = mpsc::unbounded_channel::<QueryOutcome>();
    // Anycast site discoveries arrive on their own channel: they have no
    // generation — the answering POP depends on our network path, not on
    // what domain is being checked. Keyed by IP, not index: the resolver
    // list can be edited while a probe is in flight.
    let (site_tx, mut site_rx) = mpsc::unbounded_channel::<(IpAddr, sites::Site)>();

    spawn_site_probes(&app, &site_tx);
    if auto_query {
        spawn_queries(&mut app, &tx);
    }

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    loop {
        terminal.draw(|frame| ui::draw(frame, &mut app))?;

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        handle_key(&mut app, &tx, key.code, key.modifiers);
                    }
                    Some(Ok(_)) => {} // resize etc. — redraw happens on loop
                    Some(Err(err)) => return Err(err.into()),
                    None => break,
                }
            }
            Some(outcome) = rx.recv() => {
                app.apply(outcome);
                // Drain whatever else already arrived so one redraw covers it.
                while let Ok(outcome) = rx.try_recv() {
                    app.apply(outcome);
                }
                if !app.in_flight() {
                    // Round complete: stop watching once every responding
                    // resolver agrees (refused/unreachable ones carry no
                    // propagation signal; SERVFAIL counts as responding, so
                    // a broken delegation keeps the watch alive), otherwise
                    // schedule the next poll.
                    let summary = app.summary();
                    if summary.responding > 0 && summary.agree == summary.responding {
                        app.auto_refresh = false;
                        app.next_poll = None;
                    } else if app.auto_refresh {
                        app.next_poll = Some(Instant::now() + POLL_INTERVAL);
                    }
                }
            }
            Some((ip, site)) = site_rx.recv() => {
                app.set_site(ip, site);
            }
            _ = tick.tick() => {
                if app.in_flight() {
                    app.spinner_frame = app.spinner_frame.wrapping_add(1);
                } else if app.next_poll.is_some_and(|at| Instant::now() >= at) {
                    poll_query(&mut app, &tx);
                }
            }
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

fn handle_key(
    app: &mut App,
    tx: &mpsc::UnboundedSender<QueryOutcome>,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    // The add-resolver dialog captures all input while open.
    if app.form.is_some() {
        handle_form_key(app, tx, code, modifiers);
        return;
    }
    match code {
        KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        // `+` never appears in a domain name, so it's free for "add a
        // resolver" despite the input field owning most printable keys.
        KeyCode::Char('+') => app.open_form(),
        // Ctrl+X: cut the highlighted resolver from the session's list.
        KeyCode::Char('x') if modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(round) = app.remove_selected() {
                spawn_round(app, tx, round);
            }
        }
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.clear_domain();
        }
        KeyCode::Char('s') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.sort = app.sort.next();
        }
        // Ctrl+O: "O" is the globe. Ctrl+G would be the natural mnemonic but
        // it's the BEL character, which some terminal setups intercept.
        KeyCode::Char('o') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.toggle_globe();
        }
        // Ctrl+N: next client network (Ctrl+E is taken by end-of-line).
        // Cycling re-queries right away — no Enter needed to see the new
        // subnet's answers; before the first query it just moves the chip.
        KeyCode::Char('n') if modifiers.contains(KeyModifiers::CONTROL) => {
            if !app.ecs_list.is_empty() {
                app.cycle_ecs();
                requery_selection(app, tx);
            }
        }
        KeyCode::Char('r') if modifiers.contains(KeyModifiers::CONTROL) => {
            if app.auto_refresh || app.next_poll.is_some() {
                app.auto_refresh = false;
                app.next_poll = None;
            } else if app.queried.is_some() {
                app.auto_refresh = true;
                if !app.in_flight() {
                    poll_query(app, tx);
                }
            }
        }
        KeyCode::Enter => spawn_queries(app, tx),
        // Tab re-queries as it cycles, like Ctrl+N for ECS — no Enter needed
        // to see the newly selected type's answers.
        KeyCode::Tab => {
            app.cycle_record_type(true);
            requery_selection(app, tx);
        }
        KeyCode::BackTab => {
            app.cycle_record_type(false);
            requery_selection(app, tx);
        }
        // Cmd+←/→ on macOS (reported as SUPER under the kitty keyboard
        // protocol): jump to the start/end of the input, like Home/End.
        KeyCode::Left if modifiers.contains(KeyModifiers::SUPER) => app.cursor = 0,
        KeyCode::Right if modifiers.contains(KeyModifiers::SUPER) => {
            app.cursor = app.domain.len();
        }
        // Option+←/→ on macOS (ALT), Ctrl+←/→ on Windows/Linux: move by one
        // dot-separated label.
        KeyCode::Left if modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) => {
            app.move_cursor_word_left();
        }
        KeyCode::Right if modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) => {
            app.move_cursor_word_right();
        }
        KeyCode::Left => app.move_cursor_left(),
        KeyCode::Right => app.move_cursor_right(),
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.domain.len(),
        // Terminal.app (and iTerm2's default profile) send Esc-b / Esc-f for
        // Option+←/→ — the readline word-motion sequences.
        KeyCode::Char('b') if modifiers.contains(KeyModifiers::ALT) => app.move_cursor_word_left(),
        KeyCode::Char('f') if modifiers.contains(KeyModifiers::ALT) => app.move_cursor_word_right(),
        // Readline line motions, for terminals that don't forward Cmd at all.
        KeyCode::Char('a') if modifiers.contains(KeyModifiers::CONTROL) => app.cursor = 0,
        KeyCode::Char('e') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.cursor = app.domain.len();
        }
        // Arrows move the table highlight; the view scrolls to follow it
        // (the draw pass keeps the selection visible).
        KeyCode::Up => app.move_selection(-1),
        KeyCode::Down => app.move_selection(1),
        KeyCode::PageUp => app.move_selection(-10),
        KeyCode::PageDown => app.move_selection(10),
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete(),
        KeyCode::Char(c)
            if !modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
                && (c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) =>
        {
            app.insert_char(c.to_ascii_lowercase());
        }
        _ => {}
    }
}

/// Key handling while the add-resolver dialog is open: Tab/↑/↓ move between
/// fields, Enter validates and adds, Esc cancels. Field text takes any
/// printable ASCII (names have spaces, IPv6 has colons).
fn handle_form_key(
    app: &mut App,
    tx: &mpsc::UnboundedSender<QueryOutcome>,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    let Some(form) = app.form.as_mut() else {
        return;
    };
    match code {
        KeyCode::Esc => app.cancel_form(),
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Enter => {
            if let Some(round) = app.submit_form() {
                spawn_round(app, tx, round);
            }
        }
        KeyCode::Tab | KeyCode::Down => form.cycle_focus(true),
        KeyCode::BackTab | KeyCode::Up => form.cycle_focus(false),
        KeyCode::Left => form.move_cursor_left(),
        KeyCode::Right => form.move_cursor_right(),
        KeyCode::Home => form.cursor_home(),
        KeyCode::End => form.cursor_end(),
        KeyCode::Backspace => form.backspace(),
        KeyCode::Delete => form.delete(),
        KeyCode::Char(c)
            if !modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            form.insert_char(c);
        }
        _ => {}
    }
}

/// Start a fresh query from the input field and turn watch mode on.
fn spawn_queries(app: &mut App, tx: &mpsc::UnboundedSender<QueryOutcome>) {
    let Some(round) = app.begin_query() else {
        return;
    };
    app.auto_refresh = true;
    app.next_poll = None;
    spawn_round(app, tx, round);
}

/// Re-run the checked domain with the current record-type/ECS selection —
/// Tab and Ctrl+N refresh the table as they cycle; inert before the first
/// query.
fn requery_selection(app: &mut App, tx: &mpsc::UnboundedSender<QueryOutcome>) {
    let Some(round) = app.begin_reselect() else {
        return;
    };
    app.next_poll = None;
    spawn_round(app, tx, round);
}

/// Re-poll the last-queried domain/type (watch mode).
fn poll_query(app: &mut App, tx: &mpsc::UnboundedSender<QueryOutcome>) {
    let Some(round) = app.begin_requery() else {
        return;
    };
    app.next_poll = None;
    spawn_round(app, tx, round);
}

/// Ask each anycast resolver which of its sites is answering us (issue #6).
/// One shot per run: the site follows our network path, not the query.
fn spawn_site_probes(app: &App, site_tx: &mpsc::UnboundedSender<(IpAddr, sites::Site)>) {
    for resolver in &app.resolvers {
        let Some(probe) = resolver.probe else {
            continue;
        };
        let site_tx = site_tx.clone();
        let server = resolver.ip;
        tokio::spawn(async move {
            if let Some(site) = sites::discover(probe, server).await {
                let _ = site_tx.send((server, site));
            }
        });
    }
}

fn spawn_round(app: &App, tx: &mpsc::UnboundedSender<QueryOutcome>, round: app::Round) {
    for resolver_index in round.indices {
        let tx = tx.clone();
        let domain = round.domain.clone();
        let (rtype, ecs, generation) = (round.rtype, round.ecs, round.generation);
        let server: IpAddr = app.resolvers[resolver_index].ip;
        tokio::spawn(async move {
            let (result, elapsed, ecs_honored) = dns::query(server, domain, rtype, ecs).await;
            let _ = tx.send(QueryOutcome {
                resolver_index,
                generation,
                result,
                elapsed,
                ecs_honored,
            });
        });
    }
}

/// Plain-text single run: query every resolver once per configured ECS
/// subnet (just once when there is none), print a table per round, and — the
/// point of an ECS list — a final per-subnet convergence summary.
async fn run_once(domain: String, rtype: RecordType, ecs_list: Vec<ClientSubnet>) -> Result<()> {
    let mut app = App::new(domain);
    app.rtype_idx = app::RECORD_TYPES
        .iter()
        .position(|t| *t == rtype)
        .unwrap_or(0);
    app.set_ecs_list(ecs_list);

    // Site probes run concurrently with the first query round.
    let mut probes = tokio::task::JoinSet::new();
    for (index, resolver) in app.resolvers.iter().enumerate() {
        if let Some(probe) = resolver.probe {
            let server = resolver.ip;
            probes.spawn(async move { (index, sites::discover(probe, server).await) });
        }
    }

    let selections: Vec<Option<usize>> = if app.ecs_list.is_empty() {
        vec![None]
    } else {
        (0..app.ecs_list.len()).map(Some).collect()
    };
    let multi = selections.len() > 1;
    let mut convergence: Vec<String> = Vec::new();

    for (nth, sel) in selections.into_iter().enumerate() {
        app.ecs_sel = sel;
        let round = app
            .begin_query()
            .ok_or_else(|| anyhow::anyhow!("empty domain"))?;

        let mut tasks = tokio::task::JoinSet::new();
        for resolver_index in round.indices {
            let domain = round.domain.clone();
            let (rtype, ecs, generation) = (round.rtype, round.ecs, round.generation);
            let server: IpAddr = app.resolvers[resolver_index].ip;
            tasks.spawn(async move {
                let (result, elapsed, ecs_honored) = dns::query(server, domain, rtype, ecs).await;
                QueryOutcome {
                    resolver_index,
                    generation,
                    result,
                    elapsed,
                    ecs_honored,
                }
            });
        }
        while let Some(outcome) = tasks.join_next().await {
            app.apply(outcome?);
        }
        if nth == 0 {
            while let Some(probed) = probes.join_next().await {
                let (index, site) = probed?;
                app.sites[index] = site;
            }
        }

        let summary = app.summary();
        print_round(&app, &summary, multi);

        if multi {
            let subnet = round.ecs.expect("multi implies a subnet per round");
            let answer = if summary.agree > 0 {
                format!(
                    "{:>2}/{} → {}",
                    summary.agree,
                    summary.responding,
                    summary.majority_values.join(", ")
                )
            } else {
                "no agreement".into()
            };
            convergence.push(format!("  {:<20} {answer}", dns::fmt_ecs(&subnet)));
            println!();
        }
    }

    if multi {
        let (domain, rtype, _) = app.queried.clone().expect("queried above");
        println!("ecs convergence for {domain} {rtype}:");
        for line in convergence {
            println!("{line}");
        }
    }
    Ok(())
}

/// One round's table and summary lines, in `--once`'s plain-text format.
fn print_round(app: &App, summary: &app::Summary, multi: bool) {
    let (domain, rtype, ecs) = app.queried.clone().expect("printed after begin_query");
    match ecs {
        Some(subnet) => println!("{domain} {rtype} · ecs {}\n", dns::fmt_ecs(&subnet)),
        None => println!("{domain} {rtype}\n"),
    }

    for (i, (resolver, row)) in app.resolvers.iter().zip(&app.rows).enumerate() {
        let line = match row {
            app::RowState::Done {
                result,
                elapsed,
                ecs_honored,
                ..
            } => {
                // A resolver that ignored the ECS option answered for its
                // own vantage point, not the probed subnet: shown, but
                // marked and kept out of the propagation summary.
                let ignored = *ecs_honored == Some(false);
                match result {
                    dns::QueryResult::Records { values, min_ttl } => {
                        let status = if ignored {
                            "NO-ECS "
                        } else if summary.majority_rows[i] {
                            "OK     "
                        } else {
                            "DIFFERS"
                        };
                        format!(
                            "{status} {:>5}ms  ttl={:<7} {}",
                            elapsed.as_millis(),
                            min_ttl,
                            values.join(", ")
                        )
                    }
                    dns::QueryResult::NoRecords(code) => {
                        let status = if ignored { "NO-ECS " } else { "NONE   " };
                        format!("{status} {:>5}ms  {code}", elapsed.as_millis())
                    }
                    dns::QueryResult::ServFail => {
                        format!(
                            "FAIL    {:>5}ms  SERVFAIL (can't resolve — broken delegation or DNSSEC?)",
                            elapsed.as_millis()
                        )
                    }
                    dns::QueryResult::Error(err) => {
                        format!("ERR     {:>5}ms  {err}", elapsed.as_millis())
                    }
                }
            }
            _ => "??".into(),
        };
        // Anycast resolvers that identified their answering site show it
        // (→YUL) instead of the operator's configured home location.
        let location = match &app.sites[i] {
            Some(site) => format!("→{}", site.code),
            None => resolver.location.clone(),
        };
        println!(
            "{:<22} {:<8} {:<16} {line}",
            resolver.name, location, resolver.ip
        );
    }

    let mut totals = format!(
        "\n{} of {} responding · {} servfail · {} unreachable · {} answer group(s)",
        summary.ok, summary.responding, summary.servfail, summary.errors, summary.groups
    );
    if summary.ecs_blind > 0 {
        totals.push_str(&format!(" · {} no-ecs", summary.ecs_blind));
    }
    println!("{totals}");
    if summary.agree > 0 {
        println!(
            "propagation ({}/{} responding): {}",
            summary.agree,
            summary.responding,
            summary.majority_values.join(", ")
        );
    }
    // The TTL planning note repeated per subnet would be noise: the zone's
    // TTL doesn't change with the vantage point.
    if !multi
        && summary.responding > 0
        && summary.agree == summary.responding
        && let Some(est) = app.estimated_ttl(summary)
        && est >= app::ADVISORY_TTL
    {
        println!(
            "note: TTL ≈ {} — planning a record change? lower the TTL first, then wait one old-TTL period before switching.",
            app::fmt_secs(u64::from(est))
        );
    }
}
