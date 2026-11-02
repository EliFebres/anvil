//! Text fitting, labels and rectangles: the small helpers every screen
//! needs to lay text out by display width.

use std::time::Duration;

use ratatui::layout::Rect;
use ratatui::text::Span;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Cuts text to `width` columns, ending in an ellipsis when it was cut.
///
/// # Arguments
///
/// * `text` - The text.
/// * `width` - Columns available.
///
/// # Returns
///
/// The text unchanged when it fits; otherwise its longest prefix that
/// fits with `…` appended, or nothing when fewer than two columns remain.
pub fn fit_text(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width < 2 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// A count with its noun: `1 file` or `3 files`.
///
/// # Arguments
///
/// * `n` - The count.
/// * `noun` - The singular noun; the plural adds `s`.
///
/// # Returns
///
/// The label.
pub fn count_label(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// A token count with a `k` or `M` suffix past three digits.
///
/// # Arguments
///
/// * `n` - The count.
///
/// # Returns
///
/// `999`, `1.5k` or `2.0M`.
pub fn short(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// Seconds as a clock.
///
/// # Arguments
///
/// * `secs` - The seconds.
///
/// # Returns
///
/// `mm:ss`, or `h:mm:ss` past an hour.
pub fn elapsed(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// A short duration for a tool call or a stage.
///
/// # Arguments
///
/// * `duration` - The duration.
///
/// # Returns
///
/// `340ms`, `1.2s` or `2m05s`.
pub fn duration_label(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1_000 {
        format!("{millis}ms")
    } else if millis < 60_000 {
        format!("{:.1}s", duration.as_secs_f64())
    } else {
        let secs = duration.as_secs();
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// The columns a run of spans takes.
///
/// # Arguments
///
/// * `spans` - The spans.
///
/// # Returns
///
/// The display width.
pub fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

/// Puts `right` against the right edge of a `width`-column line that
/// starts with `left`.
///
/// # Arguments
///
/// * `left` - The spans on the left.
/// * `right` - The spans on the right.
/// * `width` - The line's width.
///
/// # Returns
///
/// `left`, the padding, then `right`; with no padding when they do not fit.
pub fn pad_between(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
) -> Vec<Span<'static>> {
    let used = spans_width(&left) + spans_width(&right);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    spans.extend(right);
    spans
}

/// Lays three parts out over `width` columns: the middle centered in the
/// gap, dropped when it does not fit, and the left trimmed after that.
///
/// # Arguments
///
/// * `left` - The spans on the left.
/// * `middle` - The spans in the middle.
/// * `right` - The spans on the right.
/// * `width` - The line's width.
///
/// # Returns
///
/// Spans exactly `width` columns wide.
pub fn fit_parts(
    left: Vec<Span<'static>>,
    middle: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
) -> Vec<Span<'static>> {
    let (lw, mw, rw) = (
        spans_width(&left),
        spans_width(&middle),
        spans_width(&right),
    );
    if lw + mw + rw + 2 <= width {
        let spare = width - lw - mw - rw;
        let gap1 = spare / 2;
        let gap2 = spare - gap1;
        let mut spans = left;
        spans.push(Span::raw(" ".repeat(gap1)));
        spans.extend(middle);
        spans.push(Span::raw(" ".repeat(gap2)));
        spans.extend(right);
        return spans;
    }
    if lw + rw < width {
        let mut spans = left;
        spans.push(Span::raw(" ".repeat(width - lw - rw)));
        spans.extend(right);
        return spans;
    }
    let mut spans = Vec::new();
    let mut used = 0;
    for span in left.into_iter().chain([Span::raw(" ")]).chain(right) {
        let mut text = String::new();
        for ch in span.content.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > width {
                break;
            }
            text.push(ch);
            used += w;
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, span.style));
        }
    }
    spans.push(Span::raw(" ".repeat(width - used)));
    spans
}

/// A rectangle of the given size in the middle of `area`.
///
/// # Arguments
///
/// * `area` - The outer area.
/// * `width` - The wanted width; clamped to the area.
/// * `height` - The wanted height; clamped to the area.
///
/// # Returns
///
/// The centered rectangle.
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_text_and_counts() {
        assert_eq!(fit_text("abcdef", 4), "abc…");
        assert_eq!(fit_text("abc", 4), "abc");
        assert_eq!(fit_text("abc", 1), "");
        assert_eq!(count_label(1, "rule"), "1 rule");
        assert_eq!(count_label(3, "rule"), "3 rules");
    }

    #[test]
    fn short_elapsed_and_durations() {
        assert_eq!(short(999), "999");
        assert_eq!(short(1_500), "1.5k");
        assert_eq!(short(2_000_000), "2.0M");
        assert_eq!(elapsed(59), "00:59");
        assert_eq!(elapsed(201), "03:21");
        assert_eq!(elapsed(3_661), "1:01:01");
        assert_eq!(duration_label(Duration::from_millis(340)), "340ms");
        assert_eq!(duration_label(Duration::from_millis(1_250)), "1.2s");
        assert_eq!(duration_label(Duration::from_secs(125)), "2m05s");
    }

    #[test]
    fn pads_and_fits_parts() {
        let spans = pad_between(vec![Span::raw("ab")], vec![Span::raw("cd")], 8);
        assert_eq!(spans_width(&spans), 8);
        assert_eq!(spans[1].content, "    ");
        let spans = fit_parts(
            vec![Span::raw("left")],
            vec![Span::raw("mid")],
            vec![Span::raw("right")],
            10,
        );
        assert_eq!(spans_width(&spans), 10);
        assert!(!spans.iter().any(|span| span.content == "mid"));
    }

    #[test]
    fn centers_and_clamps() {
        let area = Rect::new(0, 0, 100, 30);
        assert_eq!(centered(area, 60, 10), Rect::new(20, 10, 60, 10));
        assert_eq!(centered(area, 200, 50), area);
    }
}
