//! The working screen: the ticket in progress, its pipeline and the live
//! implementer output on the left; resources, the queue and recent
//! outcomes on the right.

use std::time::Duration;

use anvil_tui::layout::{duration_label, fit_text, pad_between, short};
use anvil_tui::widgets::{progress_bar, status_token};
use anvil_tui::wrap::wrap;
use anvil_tui::{Shade, Theme};
use factory_core::board::{Stage, StageView, TailLine, TicketView, UsageView};
use factory_core::{Status, Tokens};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::fmt;
use super::parts::{Ctx, Want, draw_lines, draw_panel, draw_panel_lr, draw_recent, stack};

/// Below this width the two columns stack into one.
pub const STACK_BELOW: u16 = 100;

/// Rows of the context chart.
const CHART_ROWS: u16 = 3;

/// Rows of the resources panel with and without the chart.
const RESOURCES_FULL: u16 = 2 + 5 + CHART_ROWS + 1;
const RESOURCES_COMPACT: u16 = 2 + 5;

/// Columns the tool name takes in an implementer line.
const NAME_WIDTH: usize = 10;

/// The narrowest a pipeline column gets, so short names still get a rule.
const MIN_COLUMN: usize = 7;

/// Most queue rows shown.
const QUEUE_ROWS: u16 = 5;

/// Draws the working screen's body.
pub fn draw(frame: &mut Frame, area: Rect, status: &Status, ticket: &TicketView, ctx: &Ctx) {
    if area.width < STACK_BELOW {
        draw_stacked(frame, area, status, ticket, ctx);
        return;
    }
    let [left, _, right] = Layout::horizontal([
        Constraint::Fill(3),
        Constraint::Length(1),
        Constraint::Fill(2),
    ])
    .areas(area);
    let parts = stack(left, &[Want::rows(6), Want::rows(6), Want::fill(4)]);
    if let Some(rect) = parts[0] {
        draw_ticket(frame, rect, status, ticket, ctx);
    }
    if let Some(rect) = parts[1] {
        draw_pipeline(frame, rect, ticket, ctx);
    }
    if let Some(rect) = parts[2] {
        draw_implementer(frame, rect, status, ctx.theme);
    }

    let queue_rows = queue_height(status);
    let with_chart = RESOURCES_FULL + queue_rows + 4 <= right.height;
    let resources = if with_chart {
        RESOURCES_FULL
    } else {
        RESOURCES_COMPACT
    };
    let parts = stack(
        right,
        &[Want::rows(resources), Want::rows(queue_rows), Want::fill(4)],
    );
    if let Some(rect) = parts[0] {
        draw_resources(frame, rect, status, with_chart, ctx.theme);
    }
    if let Some(rect) = parts[1] {
        draw_queue(frame, rect, status, ctx.theme);
    }
    if let Some(rect) = parts[2] {
        draw_recent(frame, rect, status, ctx);
    }
}

/// One column for narrow terminals: the resources shrink to one line and
/// the queue goes when the implementer would get too few rows.
fn draw_stacked(frame: &mut Frame, area: Rect, status: &Status, ticket: &TicketView, ctx: &Ctx) {
    let parts = stack(
        area,
        &[
            Want::rows(6),
            Want::rows(6),
            Want::fill(6),
            Want::rows(1),
            Want::rows(queue_height(status)),
        ],
    );
    if let Some(rect) = parts[0] {
        draw_ticket(frame, rect, status, ticket, ctx);
    }
    if let Some(rect) = parts[1] {
        draw_pipeline(frame, rect, ticket, ctx);
    }
    if let Some(rect) = parts[2] {
        draw_implementer(frame, rect, status, ctx.theme);
    }
    if let Some(rect) = parts[3] {
        draw_lines(frame, rect, vec![resources_summary(status, ctx.theme)]);
    }
    if let Some(rect) = parts[4] {
        draw_queue(frame, rect, status, ctx.theme);
    }
}

/// The ticket panel: id, kind and claim time in the border; title,
/// symptom and the two budget bars inside.
fn draw_ticket(frame: &mut Frame, area: Rect, status: &Status, ticket: &TicketView, ctx: &Ctx) {
    let theme = ctx.theme;
    let title = vec![
        Span::styled("working", theme.bold(Shade::Accent)),
        Span::styled(
            format!("  {}  {} · {}", ticket.id, ticket.kind, ticket.priority),
            theme.fg(Shade::Muted),
        ),
    ];
    let claimed = ticket
        .claimed_at
        .map(|at| vec![Span::raw(format!("claimed {}", fmt::clock(at, ctx.tz)))])
        .unwrap_or_default();
    let inner = draw_panel_lr(frame, area, title, claimed, theme);
    let width = usize::from(inner.width);

    let minutes = ticket
        .claimed_at
        .map(|at| (ctx.now - at).num_minutes().max(0))
        .unwrap_or(0) as u64;
    let budget_minutes = ticket.budget.wall_clock_minutes;
    let tokens = status
        .usage
        .as_ref()
        .map(|usage| {
            // The budget counts weighted tokens (cached input at a tenth), so the bar must too.
            Tokens {
                input: usage.input,
                output: usage.output,
                cached: usage.cached,
            }
            .weighted()
        })
        .unwrap_or(0);
    let lines = vec![
        Line::styled(fit_text(&ticket.title, width), theme.bold(Shade::Text)),
        Line::styled(fit_text(&ticket.symptom, width), theme.fg(Shade::Secondary)),
        bar_line(
            "wall clock",
            format!("{minutes} / {budget_minutes} min"),
            ratio(minutes, budget_minutes),
            Shade::Accent,
            width,
            theme,
        ),
        bar_line(
            "tokens",
            format!("{} / {}", short(tokens), short(ticket.budget.tokens)),
            ratio(tokens, ticket.budget.tokens),
            Shade::Accent2,
            width,
            theme,
        ),
    ];
    draw_lines(frame, inner, lines);
}

/// `used / limit`, or zero without a limit.
fn ratio(used: u64, limit: u64) -> f64 {
    if limit == 0 {
        0.0
    } else {
        used as f64 / limit as f64
    }
}

/// A labelled budget bar: label, value, then the bar to the edge.
fn bar_line(
    label: &str,
    value: String,
    ratio: f64,
    shade: Shade,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let mut spans = vec![
        Span::styled(format!("{label:<11}"), theme.fg(Shade::Muted)),
        Span::styled(format!("{value:<15}"), theme.fg(Shade::Text)),
    ];
    spans.extend(progress_bar(ratio, width.saturating_sub(26), shade, theme));
    Line::from(spans)
}

/// The pipeline panel: a glyph per stage joined by rules, the names under
/// them, then each finished stage's length and the running stage's live
/// time, anvil's state and the run and review counters.
fn draw_pipeline(frame: &mut Frame, area: Rect, ticket: &TicketView, ctx: &Ctx) {
    let theme = ctx.theme;
    let inner = draw_panel(frame, area, vec![Span::raw("pipeline")], theme);
    if inner.height == 0 {
        return;
    }
    let stages: Vec<StageView> = Stage::ALL
        .iter()
        .map(|stage| {
            ticket
                .stages
                .iter()
                .find(|view| view.stage == *stage)
                .cloned()
                .unwrap_or(StageView {
                    stage: *stage,
                    started_at: None,
                    ended_at: None,
                })
        })
        .collect();
    let below: Vec<String> = stages
        .iter()
        .map(|view| match (view.started_at, view.ended_at) {
            (Some(start), Some(end)) => fmt::stage_length((end - start).num_seconds()),
            _ => String::new(),
        })
        .collect();
    let columns = column_starts(&stages, &below, inner.width);

    let buffer = frame.buffer_mut();
    for (index, view) in stages.iter().enumerate() {
        let x = inner.x + columns[index];
        let (glyph, shade) = match (view.started_at, view.ended_at) {
            (Some(_), Some(_)) => ("●", Shade::Success),
            (Some(_), None) => ("◔", Shade::Accent),
            _ => ("○", Shade::Faint),
        };
        let next = columns.get(index + 1).map(|start| inner.x + start);
        let room = next.unwrap_or(inner.right()).saturating_sub(x);
        buffer.set_stringn(x, inner.y, glyph, 1, theme.fg(shade));
        if let Some(next) = next {
            let rule = usize::from(next.saturating_sub(x + 3));
            buffer.set_stringn(x + 2, inner.y, "─".repeat(rule), rule, theme.border());
        }
        let label_style = if shade == Shade::Accent {
            theme.bold(shade)
        } else if shade == Shade::Success {
            theme.fg(Shade::Secondary)
        } else {
            theme.fg(Shade::Faint)
        };
        if inner.height > 1 {
            buffer.set_stringn(
                x,
                inner.y + 1,
                view.stage.label(),
                usize::from(room.saturating_sub(1)),
                label_style,
            );
        }
        if inner.height > 2 && !below[index].is_empty() {
            buffer.set_stringn(
                x,
                inner.y + 2,
                &below[index],
                usize::from(room.saturating_sub(1)),
                theme.fg(Shade::Faint),
            );
        }
    }

    let running = stages
        .iter()
        .position(|view| view.started_at.is_some() && view.ended_at.is_none());
    if let Some(index) = running {
        let x = inner.x + columns[index];
        let room = usize::from(inner.right().saturating_sub(x));
        let started = stages[index].started_at.unwrap_or(ctx.now);
        let mut live = fmt::running_clock((ctx.now - started).num_seconds());
        if let Some(detail) = &ticket.detail {
            live.push_str(&format!(" · {detail}"));
        }
        if inner.height > 2 {
            buffer.set_stringn(x, inner.y + 2, &live, room, theme.fg(Shade::Accent));
        }
        if inner.height > 3 {
            let counters = format!(
                "run {} of {} · review round {} of {}",
                ticket.implementer_run,
                ticket.implementer_runs_max,
                ticket.review_round,
                ticket.review_rounds_max
            );
            let x = x.min(inner.right().saturating_sub(counters.width() as u16));
            buffer.set_stringn(
                x.max(inner.x),
                inner.y + 3,
                &counters,
                usize::from(inner.width),
                theme.fg(Shade::Muted),
            );
        }
    }
}

/// Where each stage's column starts: each wide enough for its name and
/// its length, spread over the panel when there is room, equal otherwise.
fn column_starts(stages: &[StageView], below: &[String], width: u16) -> Vec<u16> {
    let wants: Vec<u16> = stages
        .iter()
        .zip(below)
        .map(|(view, below)| {
            let text = view.stage.label().width().max(below.width());
            u16::try_from((text + 2).max(MIN_COLUMN)).unwrap_or(u16::MAX)
        })
        .collect();
    let total: u16 = wants.iter().sum();
    let count = wants.len() as u16;
    let widths: Vec<u16> = if total <= width {
        let extra = (width - total) / count;
        wants.iter().map(|want| want + extra).collect()
    } else {
        vec![width / count; wants.len()]
    };
    let mut starts = Vec::with_capacity(widths.len());
    let mut x = 0;
    for width in widths {
        starts.push(x);
        x += width;
    }
    starts
}

/// The implementer panel: the newest output lines, bottom-anchored.
fn draw_implementer(frame: &mut Frame, area: Rect, status: &Status, theme: &Theme) {
    let container = status
        .container
        .as_ref()
        .map(|container| vec![Span::raw(format!("container {}", container.id))])
        .unwrap_or_default();
    let inner = draw_panel_lr(
        frame,
        area,
        vec![Span::raw("implementer")],
        container,
        theme,
    );
    let width = usize::from(inner.width);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for line in &status.implementer_tail {
        lines.extend(tail_lines(line, width, theme));
    }
    let skip = lines.len().saturating_sub(usize::from(inner.height));
    draw_lines(frame, inner, lines.split_off(skip));
}

/// One output line in the transcript style: status token, tool name,
/// argument summary and the duration at the right; model text in primary.
fn tail_lines(line: &TailLine, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    match line {
        TailLine::Text { text } => wrap(text, width)
            .into_iter()
            .map(|row| Line::styled(row, theme.fg(Shade::Text)))
            .collect(),
        TailLine::Tool {
            status,
            name,
            args,
            millis,
            running,
        } => {
            let shade = if *running {
                Shade::Accent
            } else if status == "ok" {
                Shade::Success
            } else {
                Shade::Danger
            };
            let right = if *running {
                Span::styled("working", theme.fg(Shade::Accent))
            } else {
                Span::styled(
                    millis
                        .map(|ms| duration_label(Duration::from_millis(ms)))
                        .unwrap_or_default(),
                    theme.fg(Shade::Muted),
                )
            };
            let used = 7 + NAME_WIDTH + 1 + right.content.width() + 1;
            let left = vec![
                status_token(status, shade, theme),
                Span::styled(
                    format!("{:<NAME_WIDTH$} ", fit_text(name, NAME_WIDTH)),
                    theme.fg(Shade::Secondary),
                ),
                Span::styled(
                    fit_text(args, width.saturating_sub(used)),
                    theme.fg(Shade::Muted),
                ),
            ];
            vec![Line::from(pad_between(left, vec![right], width))]
        }
    }
}

/// The share of input served from the cache, in percent.
fn cache_hit(usage: &UsageView) -> Option<u64> {
    let read = usage.input + usage.cached;
    (read > 0).then(|| (usage.cached * 100 + read / 2) / read)
}

/// The color of a cache hit rate: success above 70%, danger below 30%.
fn cache_shade(pct: u64) -> Shade {
    if pct > 70 {
        Shade::Success
    } else if pct < 30 {
        Shade::Danger
    } else {
        Shade::Text
    }
}

/// The resources panel: cost, tokens, cache hit, the context chart,
/// container cpu and memory, and egress.
fn draw_resources(frame: &mut Frame, area: Rect, status: &Status, chart: bool, theme: &Theme) {
    use anvil_tui::widgets::kv_row;
    let inner = draw_panel(frame, area, vec![Span::raw("resources")], theme);
    let width = usize::from(inner.width);
    let usage = status.usage.clone().unwrap_or_default();
    let text = theme.fg(Shade::Secondary);
    let mut lines = vec![
        kv_row(
            "cost so far",
            vec![Span::styled(
                usage
                    .cost_usd
                    .map(fmt::money)
                    .unwrap_or_else(|| "–".to_owned()),
                theme.bold(Shade::Text),
            )],
            width,
            theme,
        ),
        kv_row(
            "in · out · cached",
            vec![Span::styled(
                format!(
                    "{} · {} · {}",
                    short(usage.input),
                    short(usage.output),
                    short(usage.cached)
                ),
                text,
            )],
            width,
            theme,
        ),
        kv_row(
            "cache hit",
            match cache_hit(&usage) {
                Some(pct) => vec![Span::styled(format!("{pct}%"), theme.fg(cache_shade(pct)))],
                None => vec![Span::styled("–", text)],
            },
            width,
            theme,
        ),
    ];
    let chart_top = inner.y + lines.len() as u16;
    if chart {
        lines.extend((0..CHART_ROWS).map(|_| Line::raw("")));
        lines.push(kv_line(
            "context tokens per turn",
            &format!("turn {}", usage.context_per_turn.len()),
            width,
            theme,
        ));
    }
    let container = status.container.clone().unwrap_or_default();
    let cpu = container
        .cpu_pct
        .map(|pct| format!("{pct:.0}%"))
        .unwrap_or_else(|| "–".to_owned());
    let mem = container
        .mem_bytes
        .map(fmt::bytes)
        .unwrap_or_else(|| "–".to_owned());
    lines.push(kv_row(
        "container cpu · mem",
        vec![Span::styled(format!("{cpu} · {mem}"), text)],
        width,
        theme,
    ));
    let egress = status.egress.clone().unwrap_or_default();
    let denied_shade = if egress.denied > 0 {
        Shade::Danger
    } else {
        Shade::Secondary
    };
    lines.push(kv_row(
        "egress allowed · denied",
        vec![
            Span::styled(format!("{} · ", egress.allowed), text),
            Span::styled(egress.denied.to_string(), theme.fg(denied_shade)),
        ],
        width,
        theme,
    ));
    draw_lines(frame, inner, lines);
    if chart {
        let area = Rect::new(inner.x, chart_top, inner.width, CHART_ROWS).intersection(inner);
        draw_context_chart(frame, area, &usage, theme);
    }
}

/// A faint caption with a value at the right.
fn kv_line(label: &str, value: &str, width: usize, theme: &Theme) -> Line<'static> {
    let faint = theme.fg(Shade::Faint);
    Line::from(pad_between(
        vec![Span::styled(label.to_owned(), faint)],
        vec![Span::styled(value.to_owned(), faint)],
        width,
    ))
}

/// Context tokens per turn as bars, one cell per turn, scaled to the
/// context limit; the current turn in the accent.
fn draw_context_chart(frame: &mut Frame, area: Rect, usage: &UsageView, theme: &Theme) {
    const LEVELS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let turns = &usage.context_per_turn;
    let shown = turns.len().min(usize::from(area.width));
    let window = &turns[turns.len() - shown..];
    let limit = usage
        .context_limit
        .filter(|limit| *limit > 0)
        .unwrap_or_else(|| window.iter().copied().max().unwrap_or(1).max(1));
    let eighths_tall = u64::from(area.height) * 8;
    let buffer = frame.buffer_mut();
    for (index, tokens) in window.iter().enumerate() {
        let eighths = ((tokens * eighths_tall + limit / 2) / limit).min(eighths_tall);
        let shade = if index + 1 == window.len() {
            Shade::Accent
        } else {
            Shade::Accent2
        };
        let x = area.x + index as u16;
        for row in 0..area.height {
            let from_bottom = u64::from(row) * 8;
            let level = eighths.saturating_sub(from_bottom).min(8) as usize;
            let y = area.bottom() - 1 - row;
            buffer.set_stringn(x, y, LEVELS[level], 1, theme.fg(shade));
        }
    }
}

/// The resources on one line, for narrow terminals.
fn resources_summary(status: &Status, theme: &Theme) -> Line<'static> {
    let usage = status.usage.clone().unwrap_or_default();
    let mut parts = vec![
        format!(
            "cost {}",
            usage
                .cost_usd
                .map(fmt::money)
                .unwrap_or_else(|| "–".to_owned())
        ),
        format!(
            "{} tokens",
            short(usage.input + usage.output + usage.cached)
        ),
    ];
    if let Some(pct) = cache_hit(&usage) {
        parts.push(format!("cache {pct}%"));
    }
    if let Some(container) = &status.container {
        if let Some(cpu) = container.cpu_pct {
            parts.push(format!("cpu {cpu:.0}%"));
        }
        if let Some(mem) = container.mem_bytes {
            parts.push(fmt::bytes(mem));
        }
    }
    let egress = status.egress.clone().unwrap_or_default();
    let mut spans = vec![Span::styled(
        format!(" {} · egress {} / ", parts.join(" · "), egress.allowed),
        theme.fg(Shade::Muted),
    )];
    let denied_shade = if egress.denied > 0 {
        Shade::Danger
    } else {
        Shade::Muted
    };
    spans.push(Span::styled(
        egress.denied.to_string(),
        theme.fg(denied_shade),
    ));
    Line::from(spans)
}

/// Rows the queue panel wants.
fn queue_height(status: &Status) -> u16 {
    2 + u16::try_from(status.queue.len())
        .unwrap_or(QUEUE_ROWS)
        .clamp(1, QUEUE_ROWS)
}

/// The queue panel: priority, short id and title per open ticket.
fn draw_queue(frame: &mut Frame, area: Rect, status: &Status, theme: &Theme) {
    let title = format!("queue · {} open", status.queue.len());
    let inner = draw_panel(frame, area, vec![Span::raw(title)], theme);
    let width = usize::from(inner.width);
    let faint = theme.fg(Shade::Faint);
    let lines = if status.queue.is_empty() {
        vec![Line::styled("empty", faint)]
    } else {
        status
            .queue
            .iter()
            .map(|ticket| {
                Line::from(vec![
                    Span::styled(format!("{:<3} ", ticket.priority), faint),
                    Span::styled(format!("{:<9} ", fmt::short_id(&ticket.id)), faint),
                    Span::styled(
                        fit_text(&ticket.title, width.saturating_sub(14)),
                        theme.fg(Shade::Secondary),
                    ),
                ])
            })
            .collect()
    };
    draw_lines(frame, inner, lines);
}
