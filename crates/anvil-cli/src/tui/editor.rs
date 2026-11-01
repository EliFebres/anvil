//! The multi-line input editor: one to six visible lines, history and
//! slash-command completion.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::theme::Theme;
use super::wrap::segments;

/// The most visible lines the editor takes.
pub const MAX_HEIGHT: u16 = 6;

/// An editing key, already mapped from the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKey {
    /// Insert a character at the cursor.
    Char(char),
    /// Delete the character before the cursor, joining lines at a line start.
    Backspace,
    /// Delete the character under the cursor, joining lines at a line end.
    Delete,
    /// Move the cursor left, wrapping to the previous line.
    Left,
    /// Move the cursor right, wrapping to the next line.
    Right,
    /// Move the cursor up one logical line.
    Up,
    /// Move the cursor down one logical line.
    Down,
    /// Move the cursor to the start of the line.
    Home,
    /// Move the cursor to the end of the line.
    End,
    /// Split the line at the cursor.
    Newline,
}

/// A Tab completion in progress.
#[derive(Debug, Clone)]
struct Completion {
    matches: Vec<String>,
    index: usize,
}

/// The input buffer, its cursor and its history.
#[derive(Debug, Default)]
pub struct Editor {
    /// Logical lines; never empty.
    lines: Vec<String>,
    /// Cursor line.
    row: usize,
    /// Cursor position in characters within the line.
    col: usize,
    history: Vec<String>,
    /// Where in the history the user is, if browsing it.
    history_pos: Option<usize>,
    /// What was being typed before browsing the history.
    draft: Option<Vec<String>>,
    completion: Option<Completion>,
}

impl Editor {
    /// An empty editor.
    ///
    /// # Returns
    ///
    /// The editor.
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            ..Self::default()
        }
    }

    /// The buffer as one string with `\n` between lines.
    ///
    /// # Returns
    ///
    /// The text.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Whether the buffer holds only whitespace.
    ///
    /// # Returns
    ///
    /// True when there is nothing to send.
    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|line| line.trim().is_empty())
    }

    /// Whether the cursor is on the first logical line.
    ///
    /// # Returns
    ///
    /// True on the first line.
    pub fn at_first_line(&self) -> bool {
        self.row == 0
    }

    /// Whether the cursor is on the last logical line.
    ///
    /// # Returns
    ///
    /// True on the last line.
    pub fn at_last_line(&self) -> bool {
        self.row + 1 >= self.lines.len()
    }

    /// Takes the text out and resets the editor, keeping the history.
    ///
    /// # Returns
    ///
    /// The text.
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.set_lines(vec![String::new()]);
        self.history_pos = None;
        self.draft = None;
        text
    }

    /// Applies an editing key.
    ///
    /// # Arguments
    ///
    /// * `key` - The key.
    pub fn key(&mut self, key: EditKey) {
        self.completion = None;
        match key {
            EditKey::Char(ch) => {
                let at = self.byte_at(self.row, self.col);
                self.lines[self.row].insert(at, ch);
                self.col += 1;
            }
            EditKey::Backspace => {
                if self.col > 0 {
                    self.col -= 1;
                    let at = self.byte_at(self.row, self.col);
                    self.lines[self.row].remove(at);
                } else if self.row > 0 {
                    let tail = self.lines.remove(self.row);
                    self.row -= 1;
                    self.col = self.line_len(self.row);
                    self.lines[self.row].push_str(&tail);
                }
            }
            EditKey::Delete => {
                if self.col < self.line_len(self.row) {
                    let at = self.byte_at(self.row, self.col);
                    self.lines[self.row].remove(at);
                } else if self.row + 1 < self.lines.len() {
                    let tail = self.lines.remove(self.row + 1);
                    self.lines[self.row].push_str(&tail);
                }
            }
            EditKey::Left => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.line_len(self.row);
                }
            }
            EditKey::Right => {
                if self.col < self.line_len(self.row) {
                    self.col += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            EditKey::Up => {
                if self.row > 0 {
                    self.row -= 1;
                    self.col = self.col.min(self.line_len(self.row));
                }
            }
            EditKey::Down => {
                if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = self.col.min(self.line_len(self.row));
                }
            }
            EditKey::Home => self.col = 0,
            EditKey::End => self.col = self.line_len(self.row),
            EditKey::Newline => {
                let at = self.byte_at(self.row, self.col);
                let tail = self.lines[self.row].split_off(at);
                self.lines.insert(self.row + 1, tail);
                self.row += 1;
                self.col = 0;
            }
        }
    }

    /// Remembers a sent line, skipping a repeat of the last one.
    ///
    /// # Arguments
    ///
    /// * `line` - The line.
    pub fn push_history(&mut self, line: &str) {
        if self.history.last().is_some_and(|last| last == line) {
            return;
        }
        self.history.push(line.to_owned());
    }

    /// Replaces the buffer with the previous history entry, keeping the
    /// current text as a draft on the first step back.
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => {
                self.draft = Some(self.lines.clone());
                self.history.len() - 1
            }
            Some(0) => return,
            Some(pos) => pos - 1,
        };
        self.history_pos = Some(pos);
        self.set_lines(split_lines(&self.history[pos]));
    }

    /// Replaces the buffer with the next history entry, or restores the
    /// draft past the newest one.
    pub fn history_next(&mut self) {
        let Some(pos) = self.history_pos else {
            return;
        };
        if pos + 1 < self.history.len() {
            self.history_pos = Some(pos + 1);
            self.set_lines(split_lines(&self.history[pos + 1]));
        } else {
            self.history_pos = None;
            let draft = self.draft.take().unwrap_or_else(|| vec![String::new()]);
            self.set_lines(draft);
        }
    }

    /// Completes a slash command, cycling through the matches on repeated
    /// calls. Only a single-line buffer starting with `/` completes.
    ///
    /// # Arguments
    ///
    /// * `candidates` - Every command, each starting with `/`.
    pub fn complete(&mut self, candidates: &[String]) {
        if let Some(completion) = &mut self.completion {
            completion.index = (completion.index + 1) % completion.matches.len();
            let chosen = completion.matches[completion.index].clone();
            self.set_lines(vec![chosen]);
            return;
        }
        if self.lines.len() != 1 || !self.lines[0].starts_with('/') {
            return;
        }
        let prefix = self.lines[0].clone();
        let mut matches: Vec<String> = candidates
            .iter()
            .filter(|candidate| candidate.starts_with(&prefix))
            .cloned()
            .collect();
        matches.sort();
        matches.dedup();
        if matches.is_empty() {
            return;
        }
        self.set_lines(vec![matches[0].clone()]);
        self.completion = Some(Completion { matches, index: 0 });
    }

    /// How many rows the editor needs at this width, between one and
    /// [`MAX_HEIGHT`].
    ///
    /// # Arguments
    ///
    /// * `width` - The input area width.
    /// * `prefix_width` - Columns taken by the prompt prefix.
    ///
    /// # Returns
    ///
    /// The row count.
    pub fn height(&self, width: u16, prefix_width: usize) -> u16 {
        let avail = usize::from(width).saturating_sub(prefix_width).max(1);
        let rows: usize = self
            .lines
            .iter()
            .map(|line| segments(line, avail).len())
            .sum();
        u16::try_from(rows.max(1))
            .unwrap_or(MAX_HEIGHT)
            .min(MAX_HEIGHT)
    }

    /// The visible rows and the cursor position within them.
    ///
    /// When the buffer needs more than [`MAX_HEIGHT`] rows, the window
    /// follows the cursor.
    ///
    /// # Arguments
    ///
    /// * `width` - The input area width.
    /// * `prefix` - The prompt prefix for the first row.
    /// * `theme` - Styles.
    ///
    /// # Returns
    ///
    /// The rows and the cursor as (column, row) within the area.
    pub fn render(
        &self,
        width: u16,
        prefix: &str,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, (u16, u16)) {
        let prefix_width = prefix.width();
        let avail = usize::from(width).saturating_sub(prefix_width).max(1);
        let mut rows: Vec<String> = Vec::new();
        let mut cursor = (0usize, 0usize);
        for (row, line) in self.lines.iter().enumerate() {
            let parts = segments(line, avail);
            if row == self.row {
                let index = parts
                    .iter()
                    .rposition(|(start, _)| *start <= self.col)
                    .unwrap_or(0);
                let (start, text) = &parts[index];
                let before: String = text.chars().take(self.col - start).collect();
                cursor = (prefix_width + before.width(), rows.len() + index);
            }
            rows.extend(parts.into_iter().map(|(_, text)| text));
        }
        let shown = rows.len().min(usize::from(MAX_HEIGHT));
        let first = cursor.1.saturating_sub(shown.saturating_sub(1));
        let lines = rows
            .into_iter()
            .enumerate()
            .skip(first)
            .take(shown)
            .map(|(index, text)| {
                let lead = if index == 0 {
                    Span::styled(prefix.to_owned(), theme.prompt)
                } else {
                    Span::styled(" ".repeat(prefix_width), Style::default())
                };
                Line::from(vec![lead, Span::raw(text)])
            })
            .collect();
        let cursor_row = cursor.1.saturating_sub(first);
        (
            lines,
            (
                u16::try_from(cursor.0).unwrap_or(u16::MAX),
                u16::try_from(cursor_row).unwrap_or(u16::MAX),
            ),
        )
    }

    /// Replaces the buffer and puts the cursor at its end.
    fn set_lines(&mut self, lines: Vec<String>) {
        self.lines = if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        };
        self.row = self.lines.len() - 1;
        self.col = self.line_len(self.row);
    }

    /// Characters in a line.
    fn line_len(&self, row: usize) -> usize {
        self.lines[row].chars().count()
    }

    /// The byte offset of a character position.
    fn byte_at(&self, row: usize, col: usize) -> usize {
        self.lines[row]
            .char_indices()
            .nth(col)
            .map_or(self.lines[row].len(), |(at, _)| at)
    }
}

/// Splits text into logical lines.
fn split_lines(text: &str) -> Vec<String> {
    text.split('\n').map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(text: &str) -> Editor {
        let mut editor = Editor::new();
        for ch in text.chars() {
            if ch == '\n' {
                editor.key(EditKey::Newline);
            } else {
                editor.key(EditKey::Char(ch));
            }
        }
        editor
    }

    #[test]
    fn typing_newlines_and_backspace_join_lines() {
        let mut editor = typed("ab\ncd");
        assert_eq!(editor.text(), "ab\ncd");
        assert!(!editor.at_first_line());
        editor.key(EditKey::Home);
        editor.key(EditKey::Backspace);
        assert_eq!(editor.text(), "abcd");
        editor.key(EditKey::Left);
        editor.key(EditKey::Delete);
        assert_eq!(editor.text(), "acd");
    }

    #[test]
    fn height_is_clamped_between_one_and_six() {
        assert_eq!(Editor::new().height(80, 8), 1);
        let editor = typed(&"x".repeat(200));
        assert_eq!(editor.height(28, 8), 6);
        assert_eq!(typed("a\nb\nc").height(80, 8), 3);
    }

    #[test]
    fn history_restores_the_draft() {
        let mut editor = Editor::new();
        editor.push_history("first");
        editor.push_history("second");
        editor.push_history("second");
        editor.key(EditKey::Char('d'));
        editor.history_prev();
        assert_eq!(editor.text(), "second");
        editor.history_prev();
        assert_eq!(editor.text(), "first");
        editor.history_prev();
        assert_eq!(editor.text(), "first");
        editor.history_next();
        editor.history_next();
        assert_eq!(editor.text(), "d");
    }

    #[test]
    fn completion_cycles_through_matches() {
        let candidates = vec!["/plan".to_owned(), "/plain".to_owned(), "/build".to_owned()];
        let mut editor = typed("/pl");
        editor.complete(&candidates);
        assert_eq!(editor.text(), "/plain");
        editor.complete(&candidates);
        assert_eq!(editor.text(), "/plan");
        editor.complete(&candidates);
        assert_eq!(editor.text(), "/plain");
        editor.key(EditKey::Char('x'));
        editor.complete(&candidates);
        assert_eq!(editor.text(), "/plainx");
    }

    #[test]
    fn render_follows_the_cursor_past_six_rows() {
        let editor = typed("1\n2\n3\n4\n5\n6\n7\n8");
        let (lines, cursor) = editor.render(40, "> ", &Theme::mono());
        assert_eq!(lines.len(), 6);
        assert_eq!(cursor, (3, 5));
        let (lines, cursor) = typed("héllo").render(40, "> ", &Theme::mono());
        assert_eq!(lines.len(), 1);
        assert_eq!(cursor, (7, 0));
    }
}
