//! The idle screen, also shown while paused: the logo watermark with the
//! queue state and the last seven days on the left; health, pull requests
//! waiting on a human and recent outcomes on the right.

use anvil_tui::layout::fit_text;
use anvil_tui::widgets::kv_row;
use anvil_tui::{Depth, LOGO, Shade, Theme};
use factory_core::Status;
use factory_core::board::{RunState, WeekView};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Padding, Paragraph};

use super::fmt;
use super::parts::{Ctx, Want, draw_lines, draw_panel, draw_recent, stack};
use super::working::STACK_BELOW;

/// Rows of the center panel without the logo: borders and three lines.
const CENTER_COMPACT: u16 = 2 + 3;

/// Rows of the center panel with the logo: a blank above and below, the
/// logo, a gap, then the three lines.
const CENTER_FULL: u16 = 2 + 1 + LOGO.len() as u16 + 1 + 3 + 1;

/// Rows of the health panel.
const HEALTH_ROWS: u16 = 2 + 7;

/// Rows of the week panel at its smallest: numbers, labels, two bar rows
/// and the day names.
const WEEK_MIN: u16 = 2 + 2 + 2 + 1;

/// The tallest a day's bar gets.
const BAR_ROWS: u16 = 6;

/// Columns of one day's bar and the gap after it.
const BAR_WIDTH: u16 = 5;
const BAR_GAP: u16 = 3;

/// Draws the idle screen's body.
pub fn draw(frame: &mut Frame, area: Rect, status: &Status, ctx: &Ctx) {
    if area.width < STACK_BELOW {
        let logo = CENTER_FULL + WEEK_MIN + HEALTH_ROWS <= area.height;
        let center = if logo { CENTER_FULL } else { CENTER_COMPACT };
        let parts = stack(
            area,
            &[
                Want::rows(center),
                Want::fill(WEEK_MIN),
                Want::rows(HEALTH_ROWS),
                Want::rows(waiting_height(status)),
            ],
        );
        if let Some(rect) = parts[0] {
            draw_center(frame, rect, status, logo, ctx.theme);
        }
        if let Some(rect) = parts[1] {
            draw_week(frame, rect, status, ctx);
        }
        if let Some(rect) = parts[2] {
            draw_health(frame, rect, status, ctx);
        }
        if let Some(rect) = parts[3] {
            draw_waiting(frame, rect, status, ctx.theme);
        }
        return;
    }
    let [left, _, right] = Layout::horizontal([
        Constraint::Fill(3),
        Constraint::Length(1),
        Constraint::Fill(2),
    ])
    .areas(area);
    let logo = CENTER_FULL + WEEK_MIN <= left.height;
    let center = if logo { CENTER_FULL } else { CENTER_COMPACT };
    let parts = stack(left, &[Want::rows(center), Want::fill(WEEK_MIN)]);
    if let Some(rect) = parts[0] {
        draw_center(frame, rect, status, logo, ctx.theme);
    }
    if let Some(rect) = parts[1] {
        draw_week(frame, rect, status, ctx);
    }
    let parts = stack(
        right,
        &[
            Want::rows(HEALTH_ROWS),
            Want::rows(waiting_height(status)),
            Want::fill(4),
        ],
    );
    if let Some(rect) = parts[0] {
        draw_health(frame, rect, status, ctx);
    }
    if let Some(rect) = parts[1] {
        draw_waiting(frame, rect, status, ctx.theme);
    }
    if let Some(rect) = parts[2] {
        draw_recent(frame, rect, status, ctx);
    }
}

/// The center panel: the logo as a watermark, the queue state, the submit
/// hint and one line on main, the baseline and the sandbox.
fn draw_center(frame: &mut Frame, area: Rect, status: &Status, logo: bool, theme: &Theme) {
    let block = anvil_tui::widgets::panel(Vec::new(), theme).padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = usize::from(inner.width);
    let mut lines = Vec::new();
    if logo {
        lines.push(Line::raw(""));
        lines.extend(LOGO.iter().map(|row| Line::styled(*row, theme.border())));
        lines.push(Line::raw(""));
    }
    let empty = if status.service.state == RunState::Stopped {
        format!(
            "{} open · the factory is stopped; systemctl --user start factory",
            status.queue.len()
        )
    } else if status.queue.is_empty() {
        "queue is empty · waiting for tickets".to_owned()
    } else {
        format!("{} open · waiting for the next poll", status.queue.len())
    };
    lines.push(Line::styled(empty, theme.fg(Shade::Secondary)));
    lines.push(Line::from(vec![
        Span::styled("submit one from any machine with ", theme.fg(Shade::Faint)),
        Span::styled("anvil ticket submit", theme.fg(Shade::Muted)),
    ]));
    lines.push(health_line(status, width, theme));
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
}

/// `main green at b260133 · baseline 20/20 on claude-opus-5-5 · sandbox ready`.
fn health_line(status: &Status, width: usize, theme: &Theme) -> Line<'static> {
    let line = health_spans(status, true, theme);
    let used: usize = line.iter().map(|span| span.content.chars().count()).sum();
    if used <= width {
        return Line::from(line);
    }
    let spans = health_spans(status, false, theme);
    let used: usize = spans.iter().map(|span| span.content.chars().count()).sum();
    if used <= width {
        return Line::from(spans);
    }
    let text: String = spans.iter().map(|span| span.content.as_ref()).collect();
    Line::styled(fit_text(&text, width), theme.fg(Shade::Faint))
}

/// The pieces of the health line; the baseline's model only `with_model`.
fn health_spans(status: &Status, with_model: bool, theme: &Theme) -> Vec<Span<'static>> {
    let faint = theme.fg(Shade::Faint);
    let mut spans = Vec::new();
    if let Some(ci) = &status.health.main_ci {
        let shade = match ci.state.as_str() {
            "green" => Shade::Success,
            "red" => Shade::Danger,
            _ => Shade::Accent,
        };
        spans.push(Span::styled("main ", faint));
        spans.push(Span::styled(ci.state.clone(), theme.fg(shade)));
        if let Some(sha) = &ci.sha {
            spans.push(Span::styled(format!(" at {sha}"), faint));
        }
    }
    if let Some(baseline) = &status.health.baseline {
        let mut text = format!(" · baseline {}/{}", baseline.passed, baseline.total);
        if let Some(model) = baseline.model.as_ref().filter(|_| with_model) {
            text.push_str(&format!(" on {model}"));
        }
        spans.push(Span::styled(text, faint));
    }
    match status.health.sandbox_ready {
        Some(true) => spans.push(Span::styled(" · sandbox ready", faint)),
        Some(false) => spans.push(Span::styled(
            " · sandbox not ready",
            theme.fg(Shade::Danger),
        )),
        None => {}
    }
    spans
}

/// The week panel: five big numbers, then a stacked bar per day.
fn draw_week(frame: &mut Frame, area: Rect, status: &Status, ctx: &Ctx) {
    let theme = ctx.theme;
    let inner = draw_panel(frame, area, vec![Span::raw("last 7 days")], theme);
    let week = status.week.clone().unwrap_or_default();
    let numbers = [
        (week.totals.merged.to_string(), "merged"),
        (week.totals.rejected.to_string(), "rejected"),
        (week.waiting.to_string(), "waiting on you"),
        (fmt::money(week.totals.cost_usd), "spent"),
        (
            week.median_secs
                .map(|secs| format!("{} min", (secs + 30) / 60))
                .unwrap_or_else(|| "–".to_owned()),
            "median ticket",
        ),
    ];
    let mut values = Vec::new();
    let mut labels = Vec::new();
    for (value, label) in numbers {
        let column = value.chars().count().max(label.len()) + 4;
        values.push(Span::styled(
            format!("{value:<column$}"),
            theme.bold(Shade::Text),
        ));
        labels.push(Span::styled(
            format!("{label:<column$}"),
            theme.fg(Shade::Faint),
        ));
    }
    draw_lines(frame, inner, vec![Line::from(values), Line::from(labels)]);
    if inner.height > 4 {
        let bars = Rect::new(inner.x, inner.y + 3, inner.width, inner.height - 3);
        draw_day_bars(frame, bars, &week, theme);
    }
}

/// A bar per day, merged in success under rejected in danger, with the
/// day names under the bars and the legend at the right.
fn draw_day_bars(frame: &mut Frame, area: Rect, week: &WeekView, theme: &Theme) {
    let rows = area.height.saturating_sub(1).min(BAR_ROWS);
    if rows == 0 {
        return;
    }
    let most = week
        .days
        .iter()
        .map(|day| day.merged + day.rejected)
        .max()
        .unwrap_or(0)
        .max(1);
    let scale = |count: u32| -> u16 {
        u16::try_from((u32::from(rows) * count + most / 2) / most).unwrap_or(rows)
    };
    // Without color the two stacked parts need different shapes.
    let rejected_glyph = if theme.depth() == Depth::Mono {
        "▒"
    } else {
        "█"
    };
    let buffer = frame.buffer_mut();
    let mut x = area.x;
    for day in &week.days {
        if x + BAR_WIDTH > area.right() {
            break;
        }
        let merged = scale(day.merged);
        let total = scale(day.merged + day.rejected).max(merged);
        for row in 0..total.min(rows) {
            let (shade, glyph) = if row < merged {
                (Shade::Success, "█")
            } else {
                (Shade::Danger, rejected_glyph)
            };
            let y = area.y + rows - 1 - row;
            let cells = glyph.repeat(usize::from(BAR_WIDTH));
            buffer.set_stringn(x, y, cells, usize::from(BAR_WIDTH), theme.fg(shade));
        }
        let name = day.day.format("%a").to_string();
        buffer.set_stringn(
            x + 1,
            area.y + rows,
            name,
            usize::from(BAR_WIDTH),
            theme.fg(Shade::Faint),
        );
        x += BAR_WIDTH + BAR_GAP;
    }
    let legend_x = x + 1;
    if legend_x + 30 <= area.right() {
        let faint = theme.fg(Shade::Faint);
        let line = Line::from(vec![
            Span::styled("tickets per day · ", faint),
            Span::styled("merged", theme.fg(Shade::Success)),
            Span::styled(" ", faint),
            Span::styled("rejected", theme.fg(Shade::Danger)),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(legend_x, area.y, area.right() - legend_x, 1),
        );
    }
}

/// The health panel.
fn draw_health(frame: &mut Frame, area: Rect, status: &Status, ctx: &Ctx) {
    let theme = ctx.theme;
    let inner = draw_panel(frame, area, vec![Span::raw("health")], theme);
    let width = usize::from(inner.width);
    let health = &status.health;
    let text = theme.fg(Shade::Secondary);
    let dash = || "–".to_owned();
    let stopped = status.service.state == RunState::Stopped;
    let since = if stopped {
        "stopped".to_owned()
    } else {
        status
            .started_at
            .map(|at| {
                let day = at.with_timezone(&ctx.tz).format("%b %-d");
                format!("running since {day}")
            })
            .unwrap_or_else(dash)
    };
    let service_shade = if stopped {
        Shade::Danger
    } else {
        Shade::Success
    };
    let last_poll = status
        .last_poll
        .as_ref()
        .map(|poll| format!("{} · {:.1}s", fmt::clock(poll.at, ctx.tz), poll.secs))
        .unwrap_or_else(dash);
    let providers = if health.providers.is_empty() {
        dash()
    } else {
        health
            .providers
            .iter()
            .map(|provider| {
                let millis = provider
                    .millis
                    .map(fmt::latency)
                    .unwrap_or_else(|| "down".to_owned());
                format!("{} {millis}", provider.name)
            })
            .collect::<Vec<_>>()
            .join(" · ")
    };
    let image = health
        .image
        .as_ref()
        .map(|image| {
            let age = fmt::age((ctx.now - image.created).num_seconds());
            format!("{} {} · {age}", image.name, image.id)
        })
        .unwrap_or_else(dash);
    let disk = format!(
        "{} free · {}",
        health.disk_free_bytes.map(fmt::bytes).unwrap_or_else(dash),
        health
            .cargo_cache_bytes
            .map(fmt::bytes)
            .unwrap_or_else(dash)
    );
    let errors_shade = match health.errors_in_a_row {
        0 => Shade::Success,
        1 => Shade::Accent,
        _ => Shade::Danger,
    };
    let row = |label: &str, value: String, shade: Shade| {
        kv_row(
            label,
            vec![Span::styled(value, theme.fg(shade))],
            width,
            theme,
        )
    };
    let lines = vec![
        row("service", since, service_shade),
        kv_row(
            "last poll",
            vec![Span::styled(last_poll, text)],
            width,
            theme,
        ),
        row(
            "providers",
            fit_text(&providers, width.saturating_sub(11)),
            Shade::Secondary,
        ),
        row("image", image, Shade::Secondary),
        row(
            "binary",
            health.binary_version.clone().unwrap_or_else(dash),
            Shade::Secondary,
        ),
        row("disk · cargo cache", disk, Shade::Secondary),
        row(
            "errors in a row",
            health.errors_in_a_row.to_string(),
            errors_shade,
        ),
    ];
    draw_lines(frame, inner, lines);
}

/// Rows the waiting panel wants.
fn waiting_height(status: &Status) -> u16 {
    2 + u16::try_from(status.needs_human.len())
        .unwrap_or(3)
        .clamp(1, 3)
}

/// Pull requests labelled for a human, with what they touch.
fn draw_waiting(frame: &mut Frame, area: Rect, status: &Status, theme: &Theme) {
    let title = format!("waiting on you · {}", status.needs_human.len());
    let inner = draw_panel(frame, area, vec![Span::raw(title)], theme);
    let width = usize::from(inner.width);
    let lines = if status.needs_human.is_empty() {
        vec![Line::styled("nothing", theme.fg(Shade::Faint))]
    } else {
        status
            .needs_human
            .iter()
            .map(|pr| {
                let touches = format!("touches {}", pr.touches.join(", "));
                let number = format!("#{} ", pr.pr);
                let room = width.saturating_sub(number.len() + touches.chars().count() + 2);
                Line::from(anvil_tui::layout::pad_between(
                    vec![
                        Span::styled(number, theme.fg(Shade::Accent2)),
                        Span::styled(fit_text(&pr.title, room), theme.fg(Shade::Secondary)),
                    ],
                    vec![Span::styled(
                        fit_text(&touches, width / 2),
                        theme.fg(Shade::Faint),
                    )],
                    width,
                ))
            })
            .collect()
    };
    draw_lines(frame, inner, lines);
}
