mod app;
mod config;
mod dns;
mod resolvers;
mod ui;

use std::net::IpAddr;
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use hickory_resolver::proto::rr::RecordType;
use tokio::sync::mpsc;

use app::{App, POLL_INTERVAL};
use dns::QueryOutcome;

#[tokio::main]
async fn main() -> Result<()> {
    // Fail on a broken config before the terminal enters raw mode, so the
    // error prints normally.
    resolvers::init(config::load()?);

    let mut args = std::env::args().skip(1).peekable();

    // `--once <domain> [type]` runs a single check and prints plain text —
    // handy for scripts and for testing without a TTY.
    if args.peek().map(String::as_str) == Some("--once") {
        args.next();
        let domain = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("usage: dnsglobe --once <domain> [type]"))?;
        let rtype = match args.next() {
            Some(t) => RecordType::from_str(&t.to_uppercase())
                .map_err(|_| anyhow::anyhow!("unknown record type: {t}"))?,
            None => RecordType::A,
        };
        return run_once(domain, rtype).await;
    }

    let initial_domain = args.next().unwrap_or_default();
    let terminal = ratatui::init();
    let result = run_tui(terminal, initial_domain).await;
    ratatui::restore();
    result
}

async fn run_tui(mut terminal: ratatui::DefaultTerminal, initial_domain: String) -> Result<()> {
    let auto_query = !initial_domain.is_empty();
    let mut app = App::new(initial_domain);

    // Worker tasks send results here; keeping `tx` alive in this scope means
    // `rx.recv()` never observes a closed channel.
    let (tx, mut rx) = mpsc::unbounded_channel::<QueryOutcome>();

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
        println!(
            "{:<22} {:<8} {:<16} {line}",
            resolver.name, resolver.location, resolver.ip
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
