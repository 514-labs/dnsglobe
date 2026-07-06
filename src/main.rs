mod app;
mod config;
mod dns;
mod resolvers;
mod sites;
mod ui;

use std::net::IpAddr;
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use hickory_resolver::proto::rr::RecordType;
use tokio::sync::mpsc;

use app::{App, POLL_INTERVAL};
use dns::QueryOutcome;

const CONFIG_HELP: &str = "\
Configuration:
  An optional TOML file adds resolvers to the built-in list, or replaces it
  entirely (replace = true). Looked up at $DNSGLOBE_CONFIG (error if the file
  is missing), else $XDG_CONFIG_HOME/dnsglobe/config.toml, else
  ~/.config/dnsglobe/config.toml.

  # replace = true       # use only the resolvers below, drop the built-ins
  [[resolvers]]
  name = \"Corp DNS\"      # required
  ip = \"10.0.0.53\"       # required, IPv4 or IPv6
  location = \"HQ\"        # optional; shown in the Loc column
  lat = 40.7             # optional map position;
  lon = -74.0            # give both or neither";

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
    resolvers::init(config::load()?);

    // `--once` runs a single check and prints plain text — handy for scripts
    // and for testing without a TTY.
    if cli.once {
        let domain = cli.domain.expect("clap enforces `requires`");
        return run_once(domain, cli.record_type.unwrap_or(RecordType::A)).await;
    }

    let terminal = ratatui::init();
    let result = run_tui(terminal, cli.domain.unwrap_or_default(), cli.record_type).await;
    ratatui::restore();
    result
}

async fn run_tui(
    mut terminal: ratatui::DefaultTerminal,
    initial_domain: String,
    initial_rtype: Option<RecordType>,
) -> Result<()> {
    let auto_query = !initial_domain.is_empty();
    let mut app = App::new(initial_domain);
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
    // what domain is being checked.
    let (site_tx, mut site_rx) = mpsc::unbounded_channel::<(usize, sites::Site)>();

    spawn_site_probes(&site_tx);
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
                    // propagation signal), otherwise schedule the next poll.
                    let summary = app.summary();
                    if summary.responding > 0 && summary.agree == summary.responding {
                        app.auto_refresh = false;
                        app.next_poll = None;
                    } else if app.auto_refresh {
                        app.next_poll = Some(Instant::now() + POLL_INTERVAL);
                    }
                }
            }
            Some((index, site)) = site_rx.recv() => {
                app.sites[index] = Some(site);
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
    match code {
        KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.clear_domain();
        }
        KeyCode::Char('s') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.sort = app.sort.next();
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
        KeyCode::Tab => app.cycle_record_type(true),
        KeyCode::BackTab => app.cycle_record_type(false),
        KeyCode::Left => app.move_cursor_left(),
        KeyCode::Right => app.move_cursor_right(),
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.domain.len(),
        KeyCode::Up => app.scroll = app.scroll.saturating_sub(1),
        KeyCode::Down => app.scroll += 1, // clamped during draw
        KeyCode::PageUp => app.scroll = app.scroll.saturating_sub(10),
        KeyCode::PageDown => app.scroll += 10,
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete(),
        KeyCode::Char(c)
            if !modifiers.contains(KeyModifiers::CONTROL)
                && (c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) =>
        {
            app.insert_char(c.to_ascii_lowercase());
        }
        _ => {}
    }
}

/// Start a fresh query from the input field and turn watch mode on.
fn spawn_queries(app: &mut App, tx: &mpsc::UnboundedSender<QueryOutcome>) {
    let Some(params) = app.begin_query() else {
        return;
    };
    app.auto_refresh = true;
    app.next_poll = None;
    spawn_round(tx, params);
}

/// Re-poll the last-queried domain/type (watch mode).
fn poll_query(app: &mut App, tx: &mpsc::UnboundedSender<QueryOutcome>) {
    let Some(params) = app.begin_requery() else {
        return;
    };
    app.next_poll = None;
    spawn_round(tx, params);
}

/// Ask each anycast resolver which of its sites is answering us (issue #6).
/// One shot per run: the site follows our network path, not the query.
fn spawn_site_probes(site_tx: &mpsc::UnboundedSender<(usize, sites::Site)>) {
    for (index, resolver) in resolvers::active().iter().enumerate() {
        let Some(probe) = resolver.probe else {
            continue;
        };
        let site_tx = site_tx.clone();
        let server = resolver.ip;
        tokio::spawn(async move {
            if let Some(site) = sites::discover(probe, server).await {
                let _ = site_tx.send((index, site));
            }
        });
    }
}

fn spawn_round(
    tx: &mpsc::UnboundedSender<QueryOutcome>,
    (domain, rtype, generation, indices): (String, RecordType, u64, Vec<usize>),
) {
    for resolver_index in indices {
        let tx = tx.clone();
        let domain = domain.clone();
        let server: IpAddr = resolvers::active()[resolver_index].ip;
        tokio::spawn(async move {
            let (result, elapsed) = dns::query(server, domain, rtype).await;
            let _ = tx.send(QueryOutcome {
                resolver_index,
                generation,
                result,
                elapsed,
            });
        });
    }
}

/// Plain-text single run: query every resolver once, print a table, exit.
async fn run_once(domain: String, rtype: RecordType) -> Result<()> {
    let mut app = App::new(domain);
    app.rtype_idx = app::RECORD_TYPES
        .iter()
        .position(|t| *t == rtype)
        .unwrap_or(0);
    let (domain, rtype, generation, indices) = app
        .begin_query()
        .ok_or_else(|| anyhow::anyhow!("empty domain"))?;

    // Site probes run concurrently with the query round.
    let mut probes = tokio::task::JoinSet::new();
    for (index, resolver) in resolvers::active().iter().enumerate() {
        if let Some(probe) = resolver.probe {
            let server = resolver.ip;
            probes.spawn(async move { (index, sites::discover(probe, server).await) });
        }
    }

    let mut tasks = tokio::task::JoinSet::new();
    for resolver_index in indices {
        let domain = domain.clone();
        let server: IpAddr = resolvers::active()[resolver_index].ip;
        tasks.spawn(async move {
            let (result, elapsed) = dns::query(server, domain, rtype).await;
            QueryOutcome {
                resolver_index,
                generation,
                result,
                elapsed,
            }
        });
    }
    while let Some(outcome) = tasks.join_next().await {
        app.apply(outcome?);
    }
    while let Some(probed) = probes.join_next().await {
        let (index, site) = probed?;
        app.sites[index] = site;
    }

    let summary = app.summary();
    println!("{domain} {rtype}\n");
    for (i, (resolver, row)) in resolvers::active().iter().zip(&app.rows).enumerate() {
        let line = match row {
            app::RowState::Done {
                result, elapsed, ..
            } => match result {
                dns::QueryResult::Records { values, min_ttl } => {
                    let status = if summary.majority_rows[i] {
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
                    format!("NONE    {:>5}ms  {code}", elapsed.as_millis())
                }
                dns::QueryResult::Error(err) => {
                    format!("ERR     {:>5}ms  {err}", elapsed.as_millis())
                }
            },
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

    println!(
        "\n{} of {} responding · {} unreachable · {} answer group(s)",
        summary.ok, summary.responding, summary.errors, summary.groups
    );
    if summary.agree > 0 {
        println!(
            "propagation ({}/{} responding): {}",
            summary.agree,
            summary.responding,
            summary.majority_values.join(", ")
        );
    }
    if summary.responding > 0
        && summary.agree == summary.responding
        && let Some(est) = app.estimated_ttl(&summary)
        && est >= app::ADVISORY_TTL
    {
        println!(
            "note: TTL ≈ {} — planning a record change? lower the TTL first, then wait one old-TTL period before switching.",
            app::fmt_secs(u64::from(est))
        );
    }
    Ok(())
}
