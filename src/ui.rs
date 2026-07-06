use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Map, MapResolution};
use ratatui::widgets::{Block, Borders, Cell, LineGauge, Paragraph, Row, Table, TableState};

use crate::app::{
    ADVISORY_TTL, App, RECORD_TYPES, RowState, SPINNER, Summary, TtlVerdict, fmt_secs,
};
use crate::dns::QueryResult;
use crate::resolvers;

const ACCENT: Color = Color::Cyan;
/// Table needs ~103 cols; only show the map when there's room for both.
const MIN_WIDTH_FOR_MAP: u16 = 157;
const TABLE_WIDTH: u16 = 103;
/// Dot/status color for a cache serving an answer past its own TTL.
const STALE_COLOR: Color = Color::LightRed;
/// Dot/status color for "refetched but upstream still serves the old data".
const UPSTREAM_COLOR: Color = Color::LightBlue;
const MAP_MAX_WIDTH: u16 = 170;
/// Map bounds: lon −170..180, lat −55..72 (poles cropped).
const MAP_LON_SPAN: f64 = 350.0;
const MAP_LAT_SPAN: f64 = 127.0;
/// Rows per column that keep the projection square: braille dots are ~square
/// in a 1:2 terminal font, and a cell is 2 dots wide × 4 tall, so
/// rows = cols × (lat/lon span) × 2/4. Sizing the map by this instead of
/// filling available height is what keeps the continents recognizable.
const MAP_ASPECT: f64 = MAP_LAT_SPAN / MAP_LON_SPAN * 2.0 / 4.0;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let summary = app.summary();
    // Group comparison only settles once every resolver has answered;
    // flagging outliers mid-flight makes rows flap as the majority shifts.
    let complete = summary.done > 0 && !app.in_flight();

    let advisory = ttl_advisory(app, &summary, complete);
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(6),
        Constraint::Length(if advisory.is_some() { 3 } else { 2 }),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);

    let (left, right) = if body.width >= MIN_WIDTH_FOR_MAP {
        let map_width = (body.width - TABLE_WIDTH).min(MAP_MAX_WIDTH);
        let [left, right] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(map_width)]).areas(body);
        (left, Some(right))
    } else {
        (body, None)
    };

    let [gauge, table] = Layout::vertical([Constraint::Length(1), Constraint::Min(5)]).areas(left);
    draw_gauge(frame, app, &summary, gauge);
    // Clamp scroll so the last page stays full; height minus borders+header.
    let visible = table.height.saturating_sub(3) as usize;
    app.scroll = app
        .scroll
        .min(resolvers::active().len().saturating_sub(visible));
    draw_table(frame, app, &summary, complete, table);
    if let Some(right) = right {
        // Height follows from width via the aspect ratio; leftover space
        // below the map shows the majority answer in full.
        let map_height = ((f64::from(right.width.saturating_sub(2)) * MAP_ASPECT).round() as u16)
            .saturating_add(2)
            .min(right.height);
        let [map_area, info_area] =
            Layout::vertical([Constraint::Length(map_height), Constraint::Fill(1)]).areas(right);
        draw_map(frame, app, &summary, complete, map_area);
        draw_map_info(frame, app, &summary, complete, info_area);
    }
    draw_footer(frame, app, &summary, advisory, footer);
}

/// One-line "lower your TTL before migrating" hint, shown once a round has
/// settled with full agreement (the planning phase — mid-migration the advice
/// comes too late) and the zone's TTL is long.
fn ttl_advisory(app: &App, summary: &Summary, complete: bool) -> Option<String> {
    if !complete || summary.responding == 0 || summary.agree != summary.responding {
        return None;
    }
    let est = app.estimated_ttl(summary)?;
    (est >= ADVISORY_TTL).then(|| {
        format!(
            "TTL ≈ {} — planning a record change? Lower the TTL first, then wait one old-TTL period before switching.",
            fmt_secs(u64::from(est))
        )
    })
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let (before, after) = app.domain.split_at(app.cursor.min(app.domain.len()));
    let input = Line::from(vec![
        Span::styled(" Domain: ", Style::new().fg(Color::DarkGray)),
        Span::styled(before, Style::new().bold()),
        Span::styled("▏", Style::new().fg(ACCENT)),
        Span::styled(after, Style::new().bold()),
    ]);

    let mut types = vec![Span::styled(" Type:   ", Style::new().fg(Color::DarkGray))];
    for (i, rtype) in RECORD_TYPES.iter().enumerate() {
        let label = format!(" {rtype} ");
        types.push(if i == app.rtype_idx {
            Span::styled(label, Style::new().fg(Color::Black).bg(ACCENT).bold())
        } else {
            Span::styled(label, Style::new().fg(Color::DarkGray))
        });
        types.push(Span::raw(" "));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(ACCENT))
        .title(" 🌍 DNS Propagation Checker ")
        .title_style(Style::new().bold());
    frame.render_widget(
        Paragraph::new(vec![input, Line::from(types)]).block(block),
        area,
    );
}

fn draw_gauge(frame: &mut Frame, app: &App, summary: &Summary, area: Rect) {
    let total = resolvers::active().len();

    if app.queried.is_none() {
        let hint = Paragraph::new(Line::from(Span::styled(
            "  type a domain and press Enter",
            Style::new().fg(Color::DarkGray).italic(),
        )));
        frame.render_widget(hint, area);
        return;
    }

    let (ratio, color, label) = if app.in_flight() {
        (
            summary.done as f64 / total as f64,
            ACCENT,
            format!(
                "{} checking… {}/{} ",
                SPINNER[app.spinner_frame % SPINNER.len()],
                summary.done,
                total
            ),
        )
    } else {
        let responding = summary.responding.max(1);
        let ratio = summary.agree as f64 / responding as f64;
        let color = if ratio >= 0.9 {
            Color::Green
        } else if ratio >= 0.5 {
            Color::Yellow
        } else {
            Color::Red
        };
        let mut label = format!(
            " propagation {}/{} ({:.0}%)",
            summary.agree,
            summary.responding,
            ratio * 100.0
        );
        if summary.errors > 0 {
            label.push_str(&format!(" · {} unreachable", summary.errors));
        }
        // Worst case, every disagreeing cache must refetch within this — the
        // number the whole watch is really about.
        if summary.agree < summary.responding
            && let Some(bound) = app.stale_expiry_bound(summary, Instant::now())
        {
            label.push_str(&format!(
                " · old answers expire in ≤ {}",
                fmt_secs(bound.as_secs())
            ));
        }
        if summary.responding > 0 && summary.agree == summary.responding {
            label.push_str(" · complete ");
        } else if let Some(at) = app.next_poll {
            let secs = at
                .saturating_duration_since(std::time::Instant::now())
                .as_secs();
            label.push_str(&format!(" · next poll in {secs}s (Ctrl+R stops) "));
        } else {
            label.push_str(" · watch off (Ctrl+R resumes) ");
        }
        (ratio, color, label)
    };

    let gauge = LineGauge::default()
        .ratio(ratio)
        .label(label)
        .filled_style(Style::new().fg(color).add_modifier(Modifier::BOLD))
        .unfilled_style(Style::new().fg(Color::DarkGray));
    frame.render_widget(gauge, area);
}

fn draw_table(frame: &mut Frame, app: &App, summary: &Summary, complete: bool, area: Rect) {
    let header = Row::new([
        "Resolver", "Loc", "IP", "Time", "TTL", "Exp", "Status", "Answer",
    ])
    .style(Style::new().fg(ACCENT).bold());
    let now = Instant::now();

    let rows = app
        .display_order(summary)
        .into_iter()
        .map(|i| (i, (&resolvers::active()[i], &app.rows[i])))
        .map(|(i, (resolver, state))| {
            let (time_cell, ttl_cell, exp_cell, status_cell, answer_cell) = match state {
                RowState::Idle => (
                    Cell::from("—"),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(Span::styled("idle", Style::new().fg(Color::DarkGray))),
                    Cell::from(""),
                ),
                RowState::Pending => (
                    Cell::from("…"),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(Span::styled(
                        format!("{} query", SPINNER[app.spinner_frame % SPINNER.len()]),
                        Style::new().fg(Color::Yellow),
                    )),
                    Cell::from(""),
                ),
                RowState::Done {
                    result, elapsed, ..
                } => {
                    let ms = elapsed.as_millis();
                    let time_style = if ms < 100 {
                        Style::new().fg(Color::Green)
                    } else if ms < 400 {
                        Style::new().fg(Color::Yellow)
                    } else {
                        Style::new().fg(Color::Red)
                    };
                    let time = Cell::from(Span::styled(format!("{ms}ms"), time_style));
                    match result {
                        QueryResult::Records { values, min_ttl } => {
                            let matches_majority = !complete || summary.majority_rows[i];
                            let verdict = if matches_majority {
                                None
                            } else {
                                app.ttl_verdict(i, now)
                            };
                            let (status, style) = match verdict {
                                Some(TtlVerdict::PastTtl) => {
                                    ("! PAST TTL", Style::new().fg(STALE_COLOR).bold())
                                }
                                Some(TtlVerdict::Upstream) => {
                                    ("↻ UPSTREAM", Style::new().fg(UPSTREAM_COLOR).bold())
                                }
                                None if matches_majority => {
                                    ("✓ OK", Style::new().fg(Color::Green).bold())
                                }
                                None => ("≠ DIFFERS", Style::new().fg(Color::Magenta).bold()),
                            };
                            // Live countdown to the moment this cache entry
                            // must be refetched. For disagreeing rows this is
                            // "how much longer the old answer can survive
                            // here", so it carries the status color.
                            let remaining = state.remaining_ttl(now).unwrap_or_default().as_secs();
                            let exp = if remaining == 0 {
                                Span::styled("expired", Style::new().fg(Color::DarkGray).italic())
                            } else if matches_majority {
                                Span::styled(fmt_secs(remaining), Style::new().fg(Color::DarkGray))
                            } else {
                                Span::styled(fmt_secs(remaining), style)
                            };
                            (
                                time,
                                Cell::from(format!("{min_ttl}")),
                                Cell::from(exp),
                                Cell::from(Span::styled(status, style)),
                                Cell::from(Span::styled(
                                    values.join(", "),
                                    if matches_majority {
                                        Style::new()
                                    } else {
                                        Style::new().fg(style.fg.unwrap_or(Color::Magenta))
                                    },
                                )),
                            )
                        }
                        QueryResult::NoRecords(code) => (
                            time,
                            Cell::from(""),
                            Cell::from(""),
                            Cell::from(Span::styled("∅ NONE", Style::new().fg(Color::Red).bold())),
                            Cell::from(Span::styled(code.clone(), Style::new().fg(Color::Red))),
                        ),
                        QueryResult::Error(message) => (
                            time,
                            Cell::from(""),
                            Cell::from(""),
                            Cell::from(Span::styled("✗ ERR", Style::new().fg(Color::Red).bold())),
                            Cell::from(Span::styled(
                                message.clone(),
                                Style::new().fg(Color::Red).italic(),
                            )),
                        ),
                    }
                }
            };
            // A discovered anycast site ("→YUL") replaces the configured
            // home location: it names the POP actually answering us.
            let loc_cell = match &app.sites[i] {
                Some(site) => Cell::from(Span::styled(
                    format!("→{}", site.code),
                    Style::new().fg(ACCENT),
                )),
                None => Cell::from(Span::styled(
                    resolver.location.as_str(),
                    Style::new().fg(Color::DarkGray),
                )),
            };
            Row::new(vec![
                Cell::from(resolver.name.as_str()),
                loc_cell,
                Cell::from(Span::styled(
                    resolver.ip.to_string(),
                    Style::new().fg(Color::DarkGray),
                )),
                time_cell,
                ttl_cell,
                exp_cell,
                status_cell,
                answer_cell,
            ])
        });

    let table = Table::new(
        rows,
        [
            Constraint::Length(21),
            Constraint::Length(8),
            Constraint::Length(15),
            Constraint::Length(7),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .column_spacing(1)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(Color::DarkGray))
            .title_bottom(
                Line::from(format!(
                    " sort: {} (Ctrl+S) · {} resolvers (↑/↓ scroll) ",
                    app.sort.label(),
                    resolvers::active().len()
                ))
                .right_aligned()
                .style(Style::new().fg(Color::DarkGray)),
            ),
    );

    let mut state = TableState::default().with_offset(app.scroll);
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_map(frame: &mut Frame, app: &App, summary: &Summary, complete: bool, area: Rect) {
    let canvas = Canvas::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::DarkGray))
                .title(" Resolver Map ")
                .title_style(Style::new().fg(ACCENT).bold()),
        )
        .x_bounds([-170.0, 180.0])
        .y_bounds([-55.0, 72.0])
        .paint(|ctx| {
            ctx.draw(&Map {
                color: Color::DarkGray,
                resolution: MapResolution::High,
            });
            let now = Instant::now();
            for (i, state) in app.rows.iter().enumerate() {
                // Discovered anycast site position when known, else the
                // configured one; None keeps the resolver off the map.
                let Some((lat, lon)) = app.effective_coords(i) else {
                    continue;
                };
                let color = match state {
                    RowState::Idle => Color::DarkGray,
                    RowState::Pending => Color::Yellow,
                    RowState::Done { result, .. } => match result {
                        QueryResult::Records { .. } => {
                            if !complete || summary.majority_rows[i] {
                                Color::Green
                            } else {
                                match app.ttl_verdict(i, now) {
                                    Some(TtlVerdict::PastTtl) => STALE_COLOR,
                                    Some(TtlVerdict::Upstream) => UPSTREAM_COLOR,
                                    None => Color::Magenta,
                                }
                            }
                        }
                        QueryResult::NoRecords(_) | QueryResult::Error(_) => Color::Red,
                    },
                };
                ctx.print(lon, lat, Span::styled("●", Style::new().fg(color).bold()));
            }
        });
    frame.render_widget(canvas, area);
}

fn draw_map_info(frame: &mut Frame, app: &App, summary: &Summary, complete: bool, area: Rect) {
    if area.height < 3 {
        return;
    }
    let mut lines = vec![Line::from(vec![
        Span::styled("● agrees  ", Style::new().fg(Color::Green)),
        Span::styled("● differs  ", Style::new().fg(Color::Magenta)),
        Span::styled("● past-ttl  ", Style::new().fg(STALE_COLOR)),
        Span::styled("● upstream  ", Style::new().fg(UPSTREAM_COLOR)),
        Span::styled("● error  ", Style::new().fg(Color::Red)),
        Span::styled("● pending", Style::new().fg(Color::Yellow)),
    ])];
    if complete && !summary.majority_values.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!(
                "Majority answer ({}/{} resolvers):",
                summary.agree,
                resolvers::active().len()
            ),
            Style::new().fg(ACCENT).bold(),
        )));
        for value in &summary.majority_values {
            lines.push(Line::from(vec![
                Span::styled("  • ", Style::new().fg(Color::DarkGray)),
                Span::raw(value.as_str()),
            ]));
        }
    } else if app.queried.is_some() && app.in_flight() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "waiting for all resolvers…",
            Style::new().fg(Color::DarkGray).italic(),
        )));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::DarkGray));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .block(block),
        area,
    );
}

fn draw_footer(
    frame: &mut Frame,
    app: &App,
    summary: &Summary,
    advisory: Option<String>,
    area: Rect,
) {
    let mut status = Line::default();
    if let Some((domain, rtype)) = &app.queried {
        status.push_span(Span::styled(
            format!(" {domain} {rtype}: "),
            Style::new().bold(),
        ));
        status.push_span(Span::styled(
            format!("{} ok", summary.ok),
            Style::new().fg(Color::Green),
        ));
        status.push_span(Span::raw(" · "));
        status.push_span(Span::styled(
            format!("{} none", summary.no_records),
            Style::new().fg(Color::Red),
        ));
        status.push_span(Span::raw(" · "));
        status.push_span(Span::styled(
            format!("{} err", summary.errors),
            Style::new().fg(Color::Red),
        ));
        status.push_span(Span::raw(" · "));
        status.push_span(Span::styled(
            format!("{} answer group(s)", summary.groups),
            if summary.groups > 1 {
                Style::new().fg(Color::Magenta)
            } else {
                Style::new().fg(Color::DarkGray)
            },
        ));
    }
    let keys = Line::from(Span::styled(
        " type to edit · ←/→ move cursor (⌥/Ctrl word, ⌘/Home/End ends) · Enter query+watch · Ctrl+R watch on/off · Ctrl+S sort · Tab record type · ↑/↓ scroll · Esc quit",
        Style::new().fg(Color::DarkGray),
    ));
    if let Some(advisory) = advisory {
        let advisory_line = Line::from(vec![
            Span::styled(" ℹ ", Style::new().fg(ACCENT)),
            Span::styled(advisory, Style::new().fg(Color::DarkGray).italic()),
        ]);
        let [advisory_area, status_area, keys_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        frame.render_widget(Paragraph::new(advisory_line), advisory_area);
        frame.render_widget(Paragraph::new(status), status_area);
        frame.render_widget(Paragraph::new(keys), keys_area);
    } else {
        let [status_area, keys_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
        frame.render_widget(Paragraph::new(status), status_area);
        frame.render_widget(Paragraph::new(keys), keys_area);
    }
}
