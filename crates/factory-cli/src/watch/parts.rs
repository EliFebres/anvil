//! Pieces both screens share: the drawing context, the vertical stacking
//! that drops panels which do not fit, the top bar, the key line and the
//! recent panel.

use anvil_tui::layout::{fit_parts, fit_text, pad_between, spans_width};
use anvil_tui::widgets::panel;
use anvil_tui::{Shade, Theme};
use chrono::{DateTime, FixedOffset, Utc};
use factory_core::board::RunState;
use factory_core::{OutcomeKind, OutcomeRecord, Status};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Padding, Paragraph};

use super::fmt;

/// A snapshot older than this while a ticket is in progress is stale.
const STALE_WORKING_SECS: i64 = 10;

/// A poll this late is stale.
const STALE_POLL_SECS: i64 = 120;

/// Below this width the bars drop their longer words.
const NARROW: usize = 100;

/// Everything a screen needs besides the snapshot.
pub struct Ctx<'a> {
    /// The palette.
    pub theme: &'a Theme,
    /// The current time.
    pub now: DateTime<Utc>,
    /// The time zone times are shown in.
    pub tz: FixedOffset,
    /// The pause flag's reason when the flag is set.
    pub pause_flag: Option<&'a str>,
}

/// One panel's claim on a column's height.
#[derive(Debug, Clone, Copy)]
pub struct Want {
    /// Rows it takes; for the filling panel, the fewest it may take.
    pub rows: u16,
    /// Whether it takes every row left over.
    pub fill: bool,
}

impl Want {
    /// A panel of fixed height.
    pub fn rows(rows: u16) -> Self {
        Self { rows, fill: false }
    }

    /// A panel that takes the rest, but at least `rows`.
    pub fn fill(rows: u16) -> Self {
        Self { rows, fill: true }
    }
}

/// Splits a column top to bottom. Panels are taken in order while they
/// fit; one that does not fit is left out, so put the most important first.
/// The filling panel gets every row nobody else took.
pub fn stack(area: Rect, wants: &[Want]) -> Vec<Option<Rect>> {
    let mut left = area.height;
    let kept: Vec<bool> = wants
        .iter()
        .map(|want| {
            let fits = want.rows <= left;
            if fits {
                left -= want.rows;
            }
            fits
        })
        .collect();
    let mut y = area.y;
    wants
        .iter()
        .zip(kept)
        .map(|(want, kept)| {
            if !kept {
                return None;
            }
            let mut rows = want.rows;
            if want.fill {
                rows += left;
                left = 0;
            }
            let rect = Rect::new(area.x, y, area.width, rows);
            y += rows;
            Some(rect)
        })
        .collect()
}

/// Draws a titled panel and returns its padded inside.
pub fn draw_panel(frame: &mut Frame, area: Rect, title: Vec<Span<'static>>, theme: &Theme) -> Rect {
    let block = panel(title, theme).padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// Draws a panel with a title at the left and another at the right of its
/// top border, and returns its padded inside.
pub fn draw_panel_lr(
    frame: &mut Frame,
    area: Rect,
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    theme: &Theme,
) -> Rect {
    let mut right_spans = vec![Span::raw(" ")];
    right_spans.extend(right);
    right_spans.push(Span::raw(" "));
    let block = panel(left, theme)
        .padding(Padding::horizontal(1))
        .title_top(
            Line::from(right_spans)
                .style(theme.fg(Shade::Muted))
                .right_aligned(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// Draws lines into an area, cutting what does not fit.
pub fn draw_lines(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    frame.render_widget(Paragraph::new(lines), area);
}

/// The top bar on the raised surface: name, host, state, uptime, the next
/// poll, and today's totals at the right. A stopped service, or one whose
/// snapshot has gone stale, shows that in place of the state, with no
/// uptime or countdown: neither means anything once nothing is running.
pub fn top_bar(status: &Status, ctx: &Ctx, width: usize) -> Line<'static> {
    let theme = ctx.theme;
    let muted = theme.fg(Shade::Muted);
    let narrow = width < NARROW;
    let today = format!(
        "{}{} merged · {} rejected · {} ",
        if narrow { "" } else { "today: " },
        status.today.merged,
        status.today.rejected,
        fmt::money(status.today.cost_usd)
    );
    let mut left = vec![
        Span::raw(" "),
        Span::styled("factory", theme.bold(Shade::Accent)),
        Span::styled(format!(" · {} · ", status.host), muted),
    ];
    let state_room = width.saturating_sub(spans_width(&left) + today.chars().count() + 2);
    let down = down_spans(status, ctx, state_room);
    let alive = down.is_none();
    left.extend(down.unwrap_or_else(|| state_spans(status, ctx, state_room)));
    let mut middle = Vec::new();
    if let Some(started) = status.started_at.filter(|_| alive) {
        middle.push(Span::styled(
            format!("uptime {}", fmt::uptime((ctx.now - started).num_seconds())),
            muted,
        ));
    }
    if let Some(next) = status.next_poll_at.filter(|_| alive) {
        middle.push(Span::styled(
            format!(
                "   next poll in {}",
                fmt::countdown((next - ctx.now).num_seconds())
            ),
            muted,
        ));
    }
    let right = vec![Span::styled(today, muted)];
    let spans = fit_parts(left, middle, right, width);
    Line::from(spans).style(theme.on_surface(Style::new()))
}

/// How old the snapshot is, when it is older than it should be: the
/// service rewrites it every two seconds while working a ticket, and at
/// every poll otherwise.
fn stale_secs(status: &Status, now: DateTime<Utc>) -> Option<i64> {
    let age = (now - status.written_at?).num_seconds();
    let late = if status.ticket.is_some() {
        age > STALE_WORKING_SECS
    } else {
        status
            .next_poll_at
            .is_some_and(|next| (now - next).num_seconds() > STALE_POLL_SECS)
    };
    late.then_some(age)
}

/// `stopped · since <time>` when the service wrote that it shut down, or
/// `not responding · last update <time> (<age> ago)` when its snapshot is
/// stale (a crash or a hang writes nothing); `None` while it is alive.
fn down_spans(status: &Status, ctx: &Ctx, room: usize) -> Option<Vec<Span<'static>>> {
    let written = status.written_at?;
    let text = if status.service.state == RunState::Stopped {
        format!("stopped · since {}", fmt::clock(written, ctx.tz))
    } else {
        let age = stale_secs(status, ctx.now)?;
        format!(
            "not responding · last update {} ({} ago)",
            fmt::clock(written, ctx.tz),
            fmt::stage_length(age)
        )
    };
    Some(vec![Span::styled(
        fit_text(&text, room.max(7)),
        ctx.theme.bold(Shade::Danger),
    )])
}

/// The state word and what goes with it: `running`, `running · pausing
/// after this ticket`, `idle`, or `paused · reason`.
fn state_spans(status: &Status, ctx: &Ctx, room: usize) -> Vec<Span<'static>> {
    let theme = ctx.theme;
    if status.ticket.is_some() {
        let mut spans = vec![Span::styled("running", theme.fg(Shade::Success))];
        if ctx.pause_flag.is_some() {
            spans.push(Span::styled(
                " · pausing after this ticket",
                theme.fg(Shade::Accent),
            ));
        }
        return spans;
    }
    // The flag file is the truth: the service only rereads it on its next
    // poll, minutes away.
    match ctx.pause_flag {
        Some("") => {
            vec![Span::styled("paused", theme.fg(Shade::Accent))]
        }
        Some(reason) => vec![Span::styled(
            fit_text(&format!("paused · {reason}"), room.max(6)),
            theme.fg(Shade::Accent),
        )],
        None => vec![Span::styled("idle", theme.fg(Shade::Accent2))],
    }
}

/// The key line: each key in secondary, its meaning faint, a notice when
/// there is one, and the refresh note with the clock at the right.
pub fn key_line(
    keys: &[(&str, &str)],
    notice: Option<&str>,
    ctx: &Ctx,
    width: usize,
) -> Line<'static> {
    let theme = ctx.theme;
    let faint = theme.fg(Shade::Faint);
    let clock = fmt::clock(ctx.now, ctx.tz);
    let right_text = if width < NARROW {
        format!("{clock} ")
    } else {
        format!("refreshes every {}s · {clock}", super::REFRESH_SECS)
    };
    let right = vec![Span::styled(right_text, faint)];
    let room = width.saturating_sub(spans_width(&right) + 1);
    let mut left = vec![Span::raw(" ")];
    if let Some(notice) = notice {
        left.push(Span::styled(
            fit_text(notice, room.saturating_sub(1)),
            theme.fg(Shade::Accent),
        ));
        return Line::from(pad_between(left, right, width));
    }
    let mut used = 1;
    for (index, (key, meaning)) in keys.iter().enumerate() {
        let gap = if index == 0 { "" } else { "  " };
        let piece = gap.len() + key.len() + 1 + meaning.len();
        if used + piece > room {
            break;
        }
        left.push(Span::styled(gap, faint));
        left.push(Span::styled(key.to_string(), theme.fg(Shade::Secondary)));
        left.push(Span::styled(format!(" {meaning}"), faint));
        used += piece;
    }
    Line::from(pad_between(left, right, width))
}

/// The word and color for an outcome on the dashboard.
pub fn outcome_word(kind: OutcomeKind) -> (&'static str, Shade) {
    match kind {
        OutcomeKind::Merged => ("merged", Shade::Success),
        OutcomeKind::Rejected => ("rejected", Shade::Danger),
        OutcomeKind::Reopened => ("paused", Shade::Accent),
        OutcomeKind::Waiting => ("waiting", Shade::Accent2),
    }
}

/// What the recent panel shows at the right of an outcome: the pull
/// request for a merge, with its cost when `with_cost`; the short reason
/// for anything else.
fn outcome_detail(record: &OutcomeRecord, with_cost: bool) -> String {
    match record.kind {
        OutcomeKind::Merged => {
            let pr = record
                .pr
                .map(|pr| format!("#{pr}"))
                .unwrap_or_else(|| record.detail.clone());
            match record.cost_usd.filter(|_| with_cost) {
                Some(cost) => format!("{pr} · {}", fmt::money(cost)),
                None => pr,
            }
        }
        OutcomeKind::Waiting => "needs-human".to_owned(),
        OutcomeKind::Rejected | OutcomeKind::Reopened => {
            fmt::short_reason(&record.detail).to_owned()
        }
    }
}

/// The recent panel: time, outcome, title, and the detail at the right.
pub fn draw_recent(frame: &mut Frame, area: Rect, status: &Status, ctx: &Ctx) {
    let theme = ctx.theme;
    let inner = draw_panel(frame, area, vec![Span::raw("recent")], theme);
    let width = usize::from(inner.width);
    let lines = status
        .recent
        .iter()
        .take(usize::from(inner.height))
        .map(|record| {
            let (word, shade) = outcome_word(record.kind);
            let left_fixed = 10 + 9;
            // The detail never takes more than a third of the row, so a long
            // rejection reason cannot push the title out.
            let detail_cap = (width / 3).max(6);
            let room_with = |detail: &str| {
                let shown = detail.chars().count().min(detail_cap);
                width.saturating_sub(left_fixed + shown + 2)
            };
            let mut detail = outcome_detail(record, true);
            if room_with(&detail) < record.title.chars().count() {
                detail = outcome_detail(record, false);
            }
            let room = room_with(&detail);
            let detail = fit_text(&detail, detail_cap);
            let left = vec![
                Span::styled(
                    format!("{:<9} ", fmt::when(record.finished_at, ctx.now, ctx.tz)),
                    theme.fg(Shade::Faint),
                ),
                Span::styled(format!("{word:<8} "), theme.fg(shade)),
                Span::styled(fit_text(&record.title, room), theme.fg(Shade::Secondary)),
            ];
            let right = vec![Span::styled(detail, theme.fg(Shade::Faint))];
            Line::from(pad_between(left, right, width))
        })
        .collect();
    draw_lines(frame, inner, lines);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_drops_what_does_not_fit_and_fills_the_rest() {
        let area = Rect::new(0, 0, 10, 20);
        let rects = stack(
            area,
            &[Want::rows(7), Want::fill(6), Want::rows(9), Want::rows(2)],
        );
        assert_eq!(rects[0], Some(Rect::new(0, 0, 10, 7)));
        assert_eq!(rects[1], Some(Rect::new(0, 7, 10, 11)));
        assert_eq!(rects[2], None);
        assert_eq!(rects[3], Some(Rect::new(0, 18, 10, 2)));
    }
}
