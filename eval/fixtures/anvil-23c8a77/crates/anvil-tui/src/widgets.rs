//! Small building blocks for dashboards: panels, label-value rows,
//! progress bars and status tokens, all styled from the [`Theme`].

use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType};

use crate::layout::{fit_text, pad_between, spans_width};
use crate::theme::{Shade, Theme};

/// The columns a status token takes, so names line up after it.
pub const STATUS_WIDTH: usize = 6;

/// A bordered panel with a muted title, the frame of every dashboard box.
///
/// # Arguments
///
/// * `title` - The title spans, drawn in the top border.
/// * `theme` - The palette.
///
/// # Returns
///
/// The block; callers render content into its `inner` area.
pub fn panel(title: Vec<Span<'static>>, theme: &Theme) -> Block<'static> {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border());
    if title.is_empty() {
        return block;
    }
    let mut spans = vec![Span::raw(" ")];
    spans.extend(title);
    spans.push(Span::raw(" "));
    block.title(Line::from(spans).style(theme.fg(Shade::Muted)))
}

/// A label on the left and its value against the right edge.
///
/// # Arguments
///
/// * `label` - The label, in secondary text.
/// * `value` - The value spans; their styles are kept.
/// * `width` - The line's width.
/// * `theme` - The palette.
///
/// # Returns
///
/// The line; the label is cut first when both do not fit.
pub fn kv_row(
    label: &str,
    value: Vec<Span<'static>>,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let room = width.saturating_sub(spans_width(&value) + 1);
    let left = vec![Span::styled(
        fit_text(label, room),
        theme.fg(Shade::Secondary),
    )];
    Line::from(pad_between(left, value, width))
}

/// A horizontal bar: `ratio` of `width` filled with `█` in `shade`, the
/// rest a `░` track in the border color, so it reads without color too.
///
/// # Arguments
///
/// * `ratio` - How full, clamped to 0..=1.
/// * `width` - The bar's width in columns.
/// * `shade` - The fill's role.
/// * `theme` - The palette.
///
/// # Returns
///
/// Two spans: the filled run and the empty run.
pub fn progress_bar(ratio: f64, width: usize, shade: Shade, theme: &Theme) -> Vec<Span<'static>> {
    let ratio = if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = ((ratio * width as f64).round() as usize).min(width);
    vec![
        Span::styled("█".repeat(filled), theme.fg(shade)),
        Span::styled("░".repeat(width - filled), theme.border()),
    ]
}

/// A status word such as `ok` or `..`, padded to [`STATUS_WIDTH`] plus a
/// space.
///
/// # Arguments
///
/// * `text` - The word.
/// * `shade` - Its role.
/// * `theme` - The palette.
///
/// # Returns
///
/// The span.
pub fn status_token(text: &str, shade: Shade, theme: &Theme) -> Span<'static> {
    Span::styled(format!("{text:<STATUS_WIDTH$} "), theme.fg(shade))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_fill_by_ratio_and_clamp() {
        let theme = Theme::mono();
        let bar = progress_bar(0.25, 8, Shade::Accent, &theme);
        assert_eq!(bar[0].content, "██");
        assert_eq!(bar[1].content.chars().count(), 6);
        assert_eq!(
            progress_bar(3.0, 4, Shade::Accent, &theme)[0].content,
            "████"
        );
        assert_eq!(
            progress_bar(f64::NAN, 4, Shade::Accent, &theme)[0].content,
            ""
        );
    }

    #[test]
    fn rows_put_the_value_on_the_right() {
        let theme = Theme::mono();
        let line = kv_row("cost so far", vec![Span::raw("$0.84")], 20, &theme);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text, "cost so far    $0.84");
        let line = kv_row("a very long label", vec![Span::raw("12345")], 12, &theme);
        assert_eq!(spans_width(&line.spans), 12);
    }

    #[test]
    fn status_tokens_line_up() {
        let theme = Theme::mono();
        assert_eq!(
            status_token("ok", Shade::Success, &theme).content,
            "ok     "
        );
    }
}
