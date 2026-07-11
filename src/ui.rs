use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Map, MapResolution, Painter, Shape};
use ratatui::widgets::{Block, Borders, Cell, Clear, LineGauge, Paragraph, Row, Table, TableState};

use crate::app::{
    ADVISORY_TTL, App, RECORD_TYPES, ResolverForm, RowState, SPINNER, Summary, TtlVerdict, fmt_secs,
};
use crate::dns::QueryResult;
use crate::theme;
use crate::{globe, world_data};

/// Table needs ~103 cols; only show the flat map when there's room for both.
const MIN_WIDTH_FOR_MAP: u16 = 157;
/// The square-ish globe panel stays legible much narrower than the flat map,
/// so it appears on terminals the flat map would have left map-less.
const MIN_WIDTH_FOR_GLOBE: u16 = TABLE_WIDTH + 28;
const TABLE_WIDTH: u16 = 103;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let summary = app.summary();
    // Group comparison only settles once every resolver has answered;
    // flagging outliers mid-flight makes rows flap as the majority shifts.
    let complete = summary.done > 0 && !app.in_flight();

    let advisory = ttl_advisory(app, &summary, complete);
    // The header grows one row for the ECS line, only when --ecs/config set
    // subnets up — an ECS-less run renders exactly as before.
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(if app.ecs_list.is_empty() { 4 } else { 5 }),
        Constraint::Min(6),
        Constraint::Length(if advisory.is_some() { 3 } else { 2 }),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);

    // Steer the view for this width (auto mode flips at a threshold; forced
    // and pinned modes hold), then size the panel at the morph's current
    // position so the panel reshapes along with the projection.
    app.sync_view(body.width);
    let geom = globe::panel_geometry(
        body.width.saturating_sub(TABLE_WIDTH),
        body.height,
        app.globe.t(Instant::now()),
        info_rows(app, &summary, complete),
    );
    let min_width = if app.globe.target() {
        MIN_WIDTH_FOR_GLOBE
    } else {
        MIN_WIDTH_FOR_MAP
    };
    let (left, right) = if body.width >= min_width {
        let [left, right] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(geom.width)]).areas(body);
        (left, Some(right))
    } else {
        (body, None)
    };

    let [gauge, table] = Layout::vertical([Constraint::Length(1), Constraint::Min(5)]).areas(left);
    draw_gauge(frame, app, &summary, gauge);
    // Clamp scroll so the last page stays full; height minus borders+header.
    let visible = table.height.saturating_sub(3) as usize;
    app.scroll = app.scroll.min(app.resolvers.len().saturating_sub(visible));
    draw_table(frame, app, &summary, complete, table);
    if let Some(right) = right {
        // Leftover space below the map shows the majority answer in full.
        let [map_area, info_area] =
            Layout::vertical([Constraint::Length(geom.height), Constraint::Fill(1)]).areas(right);
        draw_map(frame, app, &summary, complete, &geom, map_area);
        draw_map_info(frame, app, &summary, complete, info_area);
    }
    draw_footer(frame, app, &summary, advisory, footer);
    if let Some(form) = &app.form {
        draw_resolver_form(frame, form, frame.area());
    }
}

/// Rows to reserve below the globe for the info box, mirroring what
/// `draw_map_info` will render: borders plus the legend (which wraps to two
/// lines on narrow panels), plus the majority-answer block once a round has
/// settled. Passing this into the panel geometry keeps a height-capped globe
/// from growing over the answers on terminals both wide and tall.
fn info_rows(app: &App, summary: &Summary, complete: bool) -> u16 {
    let legend = 4; // two borders + the legend's up-to-two wrapped lines
    if complete && !summary.majority_values.is_empty() {
        // Blank + heading + one row per value, capped so a many-valued
        // record (TXT, round-robin pools) doesn't crush the globe.
        legend + 2 + summary.majority_values.len().min(20) as u16
    } else if app.queried.is_some() && app.in_flight() {
        legend + 2 // blank + "waiting for all resolvers…"
    } else {
        legend
    }
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
    let th = theme::active();
    let (before, after) = app.domain.split_at(app.cursor.min(app.domain.len()));
    let input = Line::from(vec![
        Span::styled(" Domain: ", th.muted.style()),
        Span::styled(before, Style::new().bold()),
        Span::styled("▏", Style::new().fg(th.accent)),
        Span::styled(after, Style::new().bold()),
    ]);

    let mut types = vec![Span::styled(" Type:   ", th.muted.style())];
    for (i, rtype) in RECORD_TYPES.iter().enumerate() {
        let label = format!(" {rtype} ");
        types.push(if i == app.rtype_idx {
            Span::styled(label, Style::new().fg(Color::Black).bg(th.accent).bold())
        } else {
            Span::styled(label, th.muted.style())
        });
        types.push(Span::raw(" "));
    }
    let mut lines = vec![input, Line::from(types)];
    // Active client subnet on its own line, only when --ecs/config set one
    // up — an ECS-less run renders exactly as before (the header layout in
    // `draw` reserves the extra row on the same condition).
    if !app.ecs_list.is_empty() {
        let mut ecs = vec![Span::styled(" ECS:    ", th.muted.style())];
        match app.ecs_sel {
            Some(i) => {
                ecs.push(Span::styled(
                    format!(" {} ", crate::dns::fmt_ecs(&app.ecs_list[i])),
                    Style::new().fg(Color::Black).bg(th.accent).bold(),
                ));
                if app.ecs_list.len() > 1 {
                    ecs.push(Span::styled(
                        format!(" {}/{}", i + 1, app.ecs_list.len()),
                        th.muted.style(),
                    ));
                }
            }
            None => ecs.push(Span::styled(" off ", th.muted.style())),
        }
        lines.push(Line::from(ecs));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(th.accent))
        .title(" 🌍 DNS Propagation Checker ")
        .title_style(Style::new().bold());
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_gauge(frame: &mut Frame, app: &App, summary: &Summary, area: Rect) {
    let th = theme::active();
    let total = app.resolvers.len();

    if app.queried.is_none() {
        let hint = Paragraph::new(Line::from(Span::styled(
            "  type a domain and press Enter",
            th.muted.style().italic(),
        )));
        frame.render_widget(hint, area);
        return;
    }

    let (ratio, color, label) = if app.in_flight() {
        (
            summary.done as f64 / total as f64,
            th.accent,
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
            th.agree
        } else if ratio >= 0.5 {
            th.pending
        } else {
            th.error
        };
        let mut label = format!(
            " propagation {}/{} ({:.0}%)",
            summary.agree,
            summary.responding,
            ratio * 100.0
        );
        if summary.servfail > 0 {
            label.push_str(&format!(" · {} servfail", summary.servfail));
        }
        if summary.errors > 0 {
            label.push_str(&format!(" · {} unreachable", summary.errors));
        }
        if summary.ecs_blind > 0 {
            label.push_str(&format!(" · {} no-ecs", summary.ecs_blind));
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
        .unfilled_style(th.muted.style());
    frame.render_widget(gauge, area);
}

fn draw_table(frame: &mut Frame, app: &mut App, summary: &Summary, complete: bool, area: Rect) {
    let th = theme::active();
    let header = Row::new([
        "Resolver", "Loc", "IP", "Time", "TTL", "Exp", "Status", "Answer",
    ])
    .style(Style::new().fg(th.accent).bold());
    let now = Instant::now();

    let order = app.display_order(summary);
    // The highlight tracks a resolver, not a row: find where the display
    // order put it this frame.
    let selected = app
        .selected
        .and_then(|sel| order.iter().position(|&i| i == sel));
    let rows = order
        .iter()
        .map(|&i| (i, (&app.resolvers[i], &app.rows[i])))
        .map(|(i, (resolver, state))| {
            let (time_cell, ttl_cell, exp_cell, status_cell, answer_cell) = match state {
                RowState::Idle => (
                    Cell::from("—"),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(Span::styled("idle", th.muted.style())),
                    Cell::from(""),
                ),
                RowState::Pending => (
                    Cell::from("…"),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(Span::styled(
                        format!("{} query", SPINNER[app.spinner_frame % SPINNER.len()]),
                        Style::new().fg(th.pending),
                    )),
                    Cell::from(""),
                ),
                RowState::Done {
                    result,
                    elapsed,
                    ecs_honored,
                    ..
                } => {
                    let ms = elapsed.as_millis();
                    let time_style = if ms < 100 {
                        Style::new().fg(th.agree)
                    } else if ms < 400 {
                        Style::new().fg(th.pending)
                    } else {
                        Style::new().fg(th.error)
                    };
                    let time = Cell::from(Span::styled(format!("{ms}ms"), time_style));
                    // Answered without using the round's ECS option: its own
                    // vantage point's answer, excluded from the propagation
                    // math, so agree/differ verdicts don't apply to it.
                    let ecs_ignored = *ecs_honored == Some(false);
                    match result {
                        QueryResult::Records { values, min_ttl } => {
                            let matches_majority = !complete || summary.majority_rows[i];
                            let verdict = if matches_majority || ecs_ignored {
                                None
                            } else {
                                app.ttl_verdict(i, now)
                            };
                            let (status, style) = if ecs_ignored {
                                ("◌ NO ECS", th.muted.style())
                            } else {
                                match verdict {
                                    Some(TtlVerdict::PastTtl) => {
                                        ("! PAST TTL", Style::new().fg(th.stale).bold())
                                    }
                                    Some(TtlVerdict::Upstream) => {
                                        ("↻ UPSTREAM", Style::new().fg(th.upstream).bold())
                                    }
                                    None if matches_majority => {
                                        ("✓ OK", Style::new().fg(th.agree).bold())
                                    }
                                    None => ("≠ DIFFERS", Style::new().fg(th.differ).bold()),
                                }
                            };
                            // Live countdown to the moment this cache entry
                            // must be refetched. For disagreeing rows this is
                            // "how much longer the old answer can survive
                            // here", so it carries the status color.
                            let remaining = state.remaining_ttl(now).unwrap_or_default().as_secs();
                            let exp = if remaining == 0 {
                                Span::styled("expired", th.muted.style().italic())
                            } else if matches_majority || ecs_ignored {
                                Span::styled(fmt_secs(remaining), th.muted.style())
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
                                    if matches_majority || ecs_ignored {
                                        Style::new()
                                    } else {
                                        Style::new().fg(style.fg.unwrap_or(th.differ))
                                    },
                                )),
                            )
                        }
                        QueryResult::NoRecords(code) => {
                            let (status, style) = if ecs_ignored {
                                ("◌ NO ECS", th.muted.style())
                            } else {
                                ("∅ NONE", Style::new().fg(th.error).bold())
                            };
                            (
                                time,
                                Cell::from(""),
                                Cell::from(""),
                                Cell::from(Span::styled(status, style)),
                                Cell::from(Span::styled(
                                    code.clone(),
                                    if ecs_ignored {
                                        th.muted.style()
                                    } else {
                                        Style::new().fg(th.error)
                                    },
                                )),
                            )
                        }
                        QueryResult::ServFail => (
                            time,
                            Cell::from(""),
                            Cell::from(""),
                            Cell::from(Span::styled(
                                "✗ SERVFAIL",
                                Style::new().fg(th.error).bold(),
                            )),
                            Cell::from(Span::styled(
                                "can't resolve — broken delegation or DNSSEC?",
                                Style::new().fg(th.error),
                            )),
                        ),
                        QueryResult::Error(message) => (
                            time,
                            Cell::from(""),
                            Cell::from(""),
                            Cell::from(Span::styled("✗ ERR", Style::new().fg(th.error).bold())),
                            Cell::from(Span::styled(
                                message.clone(),
                                Style::new().fg(th.error).italic(),
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
                    Style::new().fg(th.accent),
                )),
                None => Cell::from(Span::styled(resolver.location.as_str(), th.muted.style())),
            };
            Row::new(vec![
                Cell::from(resolver.name.as_str()),
                loc_cell,
                Cell::from(Span::styled(resolver.ip.to_string(), th.muted.style())),
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
    // Reversed, not a color: readable on any theme, and it can't be confused
    // with the status colors the row already carries.
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(th.muted.style())
            .title_bottom(
                Line::from(format!(
                    " sort: {} (Ctrl+S) · {} resolvers (↑/↓ select) ",
                    app.sort.label(),
                    app.resolvers.len()
                ))
                .right_aligned()
                .style(th.muted.style()),
            ),
    );

    let mut state = TableState::default()
        .with_offset(app.scroll)
        .with_selected(selected);
    frame.render_stateful_widget(table, area, &mut state);
    // Ratatui scrolls the offset to keep the selection visible; persist that
    // so the view doesn't snap back next frame.
    app.scroll = state.offset();
}

/// The world mid-morph: coastline points run through the flat↔globe
/// projection at parameter `t`, plus a graticule and the limb once the
/// sphere has (mostly) formed. Painted point-by-point like ratatui's `Map` —
/// at t=0 it would be pixel-identical to it, so `draw_map` swaps back to the
/// built-in shape there.
struct MorphedWorld {
    t: f64,
    center_lon: f64,
}

impl MorphedWorld {
    fn paint(&self, painter: &mut Painter, lon: f64, lat: f64, color: Color) {
        if let Some((x, y)) = globe::project(lon, lat, self.center_lon, self.t)
            && let Some((px, py)) = painter.get_point(x, y)
        {
            painter.paint(px, py, color);
        }
    }
}

impl Shape for MorphedWorld {
    fn draw(&self, painter: &mut Painter) {
        let th = theme::active();
        // Graticule first so land overdraws it. It carries the spin where
        // there's no coastline (the Pacific hemisphere is nearly all water)
        // and, mid-morph, shows the map's grid curling into a sphere.
        for meridian in (-180..180).step_by(30) {
            for lat in (-80..=80).step_by(2) {
                self.paint(painter, f64::from(meridian), f64::from(lat), th.grid);
            }
        }
        for parallel in (-60..=60).step_by(30) {
            for lon in (-180..180).step_by(2) {
                self.paint(painter, f64::from(lon), f64::from(parallel), th.grid);
            }
        }
        // The limb only exists on the sphere — fade it in near the end of
        // the morph instead of drawing a floating circle over the flat map.
        if self.t > 0.9 {
            for step in 0..360 {
                let a = f64::from(step).to_radians();
                if let Some((px, py)) = painter.get_point(
                    globe::CENTER_X + globe::RADIUS * a.cos(),
                    globe::CENTER_Y + globe::RADIUS * a.sin(),
                ) {
                    painter.paint(px, py, th.grid);
                }
            }
        }
        for &(lon, lat) in &world_data::WORLD {
            self.paint(painter, lon, lat, th.coastline);
        }
    }
}

fn draw_map(
    frame: &mut Frame,
    app: &App,
    summary: &Summary,
    complete: bool,
    geom: &globe::PanelGeom,
    area: Rect,
) {
    let th = theme::active();
    let now = Instant::now();
    // Layout and projection share geom.t so the zoom tracks the morph.
    let t = geom.t;
    let center_lon = app.globe.center_lon(now);
    let canvas = Canvas::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(th.muted.style())
                .title(if t > 0.5 {
                    " Resolver Globe "
                } else {
                    " Resolver Map "
                })
                .title_style(Style::new().fg(th.accent).bold()),
        )
        .x_bounds(geom.x_bounds())
        .y_bounds(geom.y_bounds())
        .paint(|ctx| {
            if t > 0.0 {
                ctx.draw(&MorphedWorld { t, center_lon });
            } else {
                ctx.draw(&Map {
                    color: th.coastline,
                    resolution: MapResolution::High,
                });
            }
            for (i, state) in app.rows.iter().enumerate() {
                // Discovered anycast site position when known, else the
                // configured one; None keeps the resolver off the map.
                let Some((lat, lon)) = app.effective_coords(i) else {
                    continue;
                };
                // Same morph as the coastline, so dots ride their continents;
                // None = rotated onto the far hemisphere.
                let Some((x, y)) = globe::project(lon, lat, center_lon, t) else {
                    continue;
                };
                let style = match state {
                    // Faint has no color to give a dot, so idle dots take
                    // the whole muted style instead of an fg like the rest.
                    RowState::Idle => th.muted.style(),
                    RowState::Pending => Style::new().fg(th.pending),
                    // ECS-ignoring resolvers answered a different question
                    // than the probed subnet: muted like idle, not judged.
                    RowState::Done {
                        result: QueryResult::Records { .. } | QueryResult::NoRecords(_),
                        ecs_honored: Some(false),
                        ..
                    } => th.muted.style(),
                    RowState::Done { result, .. } => Style::new().fg(match result {
                        QueryResult::Records { .. } => {
                            if !complete || summary.majority_rows[i] {
                                th.agree
                            } else {
                                match app.ttl_verdict(i, now) {
                                    Some(TtlVerdict::PastTtl) => th.stale,
                                    Some(TtlVerdict::Upstream) => th.upstream,
                                    None => th.differ,
                                }
                            }
                        }
                        QueryResult::NoRecords(_)
                        | QueryResult::ServFail
                        | QueryResult::Error(_) => th.error,
                    }),
                };
                ctx.print(x, y, Span::styled("●", style.bold()));
            }
        });
    frame.render_widget(canvas, area);
}

fn draw_map_info(frame: &mut Frame, app: &App, summary: &Summary, complete: bool, area: Rect) {
    if area.height < 3 {
        return;
    }
    let th = theme::active();
    let mut legend = vec![
        Span::styled("● agrees  ", Style::new().fg(th.agree)),
        Span::styled("● differs  ", Style::new().fg(th.differ)),
        Span::styled("● past-ttl  ", Style::new().fg(th.stale)),
        Span::styled("● upstream  ", Style::new().fg(th.upstream)),
        Span::styled("● error  ", Style::new().fg(th.error)),
        Span::styled("● pending", Style::new().fg(th.pending)),
    ];
    if !app.ecs_list.is_empty() {
        legend.push(Span::styled("  ● no-ecs", th.muted.style()));
    }
    let mut lines = vec![Line::from(legend)];
    if complete && !summary.majority_values.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!(
                "Majority answer ({}/{} resolvers):",
                summary.agree,
                app.resolvers.len()
            ),
            Style::new().fg(th.accent).bold(),
        )));
        for value in &summary.majority_values {
            lines.push(Line::from(vec![
                Span::styled("  • ", th.muted.style()),
                Span::raw(value.as_str()),
            ]));
        }
    } else if app.queried.is_some() && app.in_flight() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "waiting for all resolvers…",
            th.muted.style().italic(),
        )));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(th.muted.style());
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
    let th = theme::active();
    let mut status = Line::default();
    if let Some((domain, rtype, ecs)) = &app.queried {
        // Name the round's subnet: cycling the selection doesn't re-query,
        // so the table may show a different subnet than the header chip.
        let ecs = match ecs {
            Some(subnet) => format!(" ecs {}", crate::dns::fmt_ecs(subnet)),
            None => String::new(),
        };
        status.push_span(Span::styled(
            format!(" {domain} {rtype}{ecs}: "),
            Style::new().bold(),
        ));
        status.push_span(Span::styled(
            format!("{} ok", summary.ok),
            Style::new().fg(th.agree),
        ));
        status.push_span(Span::raw(" · "));
        status.push_span(Span::styled(
            format!("{} none", summary.no_records),
            Style::new().fg(th.error),
        ));
        status.push_span(Span::raw(" · "));
        status.push_span(Span::styled(
            format!("{} servfail", summary.servfail),
            Style::new().fg(th.error),
        ));
        status.push_span(Span::raw(" · "));
        status.push_span(Span::styled(
            format!("{} err", summary.errors),
            Style::new().fg(th.error),
        ));
        status.push_span(Span::raw(" · "));
        status.push_span(Span::styled(
            format!("{} answer group(s)", summary.groups),
            if summary.groups > 1 {
                Style::new().fg(th.differ)
            } else {
                th.muted.style()
            },
        ));
        if summary.ecs_blind > 0 {
            status.push_span(Span::raw(" · "));
            status.push_span(Span::styled(
                format!("{} no-ecs", summary.ecs_blind),
                th.muted.style(),
            ));
        }
    }
    let ecs_hint = if app.ecs_list.is_empty() {
        ""
    } else {
        " · Ctrl+N ecs"
    };
    let keys = Line::from(Span::styled(
        format!(
            " type to edit · ←/→ move cursor (⌥/Ctrl word, ⌘/Home/End ends) · Enter query+watch · Ctrl+R watch on/off · Ctrl+S sort · Ctrl+O globe/map · Tab record type{ecs_hint} · ↑/↓ select · + add resolver · Ctrl+X remove · Esc quit"
        ),
        th.muted.style(),
    ));
    if let Some(advisory) = advisory {
        let advisory_line = Line::from(vec![
            Span::styled(" ℹ ", Style::new().fg(th.accent)),
            Span::styled(advisory, th.muted.style().italic()),
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

/// The `+` add-resolver dialog, centered over whatever is behind it. One
/// line per field; the focused field carries the accent and a cursor.
fn draw_resolver_form(frame: &mut Frame, form: &ResolverForm, area: Rect) {
    let th = theme::active();
    // Fields + error line + hint, borders, and a blank line above each of
    // the two trailing sections.
    let height = (ResolverForm::LABELS.len() as u16 + 6).min(area.height);
    let width = 46.min(area.width);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let mut lines = Vec::new();
    for (i, label) in ResolverForm::LABELS.iter().enumerate() {
        let value = &form.fields[i];
        let mut spans = vec![Span::styled(
            format!(" {label:<9} "),
            if i == form.focus {
                Style::new().fg(th.accent).bold()
            } else {
                th.muted.style()
            },
        )];
        if i == form.focus {
            let (before, after) = value.split_at(form.cursor.min(value.len()));
            spans.push(Span::styled(before, Style::new().bold()));
            spans.push(Span::styled("▏", Style::new().fg(th.accent)));
            spans.push(Span::styled(after, Style::new().bold()));
        } else {
            spans.push(Span::raw(value.as_str()));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::default());
    lines.push(match &form.error {
        Some(error) => Line::from(Span::styled(format!(" {error}"), Style::new().fg(th.error))),
        // Which fields may stay empty, where an error would otherwise sit.
        None => Line::from(Span::styled(
            " location and lat/lon are optional",
            th.muted.style().italic(),
        )),
    });
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        " Enter add · Tab/↑/↓ field · Esc cancel",
        th.muted.style(),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(th.accent))
        .title(" Add resolver ")
        .title_style(Style::new().bold());
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_rows_track_what_the_info_box_will_show() {
        let mut app = App::new("example.com".into());
        let mut summary = app.summary();

        // Nothing queried yet: just borders + legend.
        assert_eq!(info_rows(&app, &summary, false), 4);

        // Mid-round: the "waiting for all resolvers…" note needs two rows.
        app.begin_query().unwrap();
        assert_eq!(info_rows(&app, &summary, false), 6);

        // Settled round: blank + heading + one row per majority value…
        app.rows = vec![RowState::Idle; app.rows.len()];
        summary.majority_values = vec!["192.0.2.1".into(), "192.0.2.2".into()];
        assert_eq!(info_rows(&app, &summary, true), 8);

        // …capped so a many-valued record doesn't crush the globe.
        summary.majority_values = vec!["v".into(); 30];
        assert_eq!(info_rows(&app, &summary, true), 26);
    }
}
