//! The key map: one place that turns terminal key events into actions.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::editor::EditKey;

/// What a key asks the UI to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Send the input, or toggle the selected tool call when it is empty.
    Submit,
    /// Insert a line break in the input.
    Newline,
    /// Ctrl+C: cancel the turn, or quit when pressed twice.
    Cancel,
    /// Ctrl+L: clear the transcript view.
    ClearView,
    /// Scroll the transcript up one page.
    PageUp,
    /// Scroll the transcript down one page.
    PageDown,
    /// Move up in the input, or back through history on its first line.
    Up,
    /// Move down in the input, or forward through history on its last line.
    Down,
    /// Ctrl+Up: select the previous tool call.
    SelectPrev,
    /// Ctrl+Down: select the next tool call.
    SelectNext,
    /// Tab: complete a slash command.
    Complete,
    /// Esc: close a prompt with its safe answer, or drop the selection.
    Escape,
    /// `?` on an empty input: show the key help.
    Help,
    /// Edit the input.
    Edit(EditKey),
    /// Nothing; also every key release, which Windows reports.
    Ignore,
}

/// Maps a key event to an action.
///
/// Shift+Enter, Alt+Enter and Ctrl+J all insert a newline because many
/// terminals cannot tell Shift+Enter from Enter.
///
/// # Arguments
///
/// * `key` - The event.
/// * `editor_empty` - Whether the input is empty, which lets `?` mean help.
///
/// # Returns
///
/// The action.
pub fn map(key: KeyEvent, editor_empty: bool) -> KeyAction {
    if key.kind == KeyEventKind::Release {
        return KeyAction::Ignore;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Enter if shift || alt => KeyAction::Newline,
        KeyCode::Enter => KeyAction::Submit,
        KeyCode::Char('j' | 'J') if ctrl => KeyAction::Newline,
        KeyCode::Char('c' | 'C') if ctrl => KeyAction::Cancel,
        KeyCode::Char('l' | 'L') if ctrl => KeyAction::ClearView,
        KeyCode::Char('a' | 'A') if ctrl => KeyAction::Edit(EditKey::Home),
        KeyCode::Char('e' | 'E') if ctrl => KeyAction::Edit(EditKey::End),
        KeyCode::PageUp => KeyAction::PageUp,
        KeyCode::PageDown => KeyAction::PageDown,
        KeyCode::Up if ctrl => KeyAction::SelectPrev,
        KeyCode::Down if ctrl => KeyAction::SelectNext,
        KeyCode::Up => KeyAction::Up,
        KeyCode::Down => KeyAction::Down,
        KeyCode::Tab => KeyAction::Complete,
        KeyCode::Esc => KeyAction::Escape,
        KeyCode::Char('?') if editor_empty && !ctrl && !alt => KeyAction::Help,
        KeyCode::Left => KeyAction::Edit(EditKey::Left),
        KeyCode::Right => KeyAction::Edit(EditKey::Right),
        KeyCode::Home => KeyAction::Edit(EditKey::Home),
        KeyCode::End => KeyAction::Edit(EditKey::End),
        KeyCode::Backspace => KeyAction::Edit(EditKey::Backspace),
        KeyCode::Delete => KeyAction::Edit(EditKey::Delete),
        KeyCode::Char(ch) if !ctrl && !alt => KeyAction::Edit(EditKey::Char(ch)),
        _ => KeyAction::Ignore,
    }
}

/// The key help, as key and meaning pairs.
pub const HELP: &[(&str, &str)] = &[
    (
        "Enter",
        "send; on an empty input, expand or collapse a tool call",
    ),
    ("Shift+Enter, Alt+Enter, Ctrl+J", "new line"),
    ("Up, Down", "history (on the first or last line)"),
    (
        "Ctrl+Up, Ctrl+Down",
        "select the previous or next tool call",
    ),
    ("Tab", "complete a slash command"),
    (
        "PageUp, PageDown, wheel",
        "scroll the transcript; typing jumps back down",
    ),
    ("click", "expand or collapse the tool call under the mouse"),
    ("Ctrl+L", "clear the view (the session keeps everything)"),
    ("Ctrl+C", "cancel the turn; twice within 2 s quits"),
    (
        "Esc",
        "close a prompt with its safe answer; drop the selection",
    ),
    ("?", "this help, when the input is empty"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn enter_variants() {
        assert_eq!(
            map(key(KeyCode::Enter, KeyModifiers::NONE), true),
            KeyAction::Submit
        );
        assert_eq!(
            map(key(KeyCode::Enter, KeyModifiers::SHIFT), true),
            KeyAction::Newline
        );
        assert_eq!(
            map(key(KeyCode::Enter, KeyModifiers::ALT), true),
            KeyAction::Newline
        );
        assert_eq!(
            map(key(KeyCode::Char('j'), KeyModifiers::CONTROL), true),
            KeyAction::Newline
        );
    }

    #[test]
    fn ctrl_arrows_select_and_plain_arrows_move() {
        assert_eq!(
            map(key(KeyCode::Up, KeyModifiers::CONTROL), true),
            KeyAction::SelectPrev
        );
        assert_eq!(
            map(key(KeyCode::Down, KeyModifiers::CONTROL), true),
            KeyAction::SelectNext
        );
        assert_eq!(
            map(key(KeyCode::Up, KeyModifiers::NONE), true),
            KeyAction::Up
        );
    }

    #[test]
    fn releases_are_ignored_and_question_mark_needs_an_empty_input() {
        let mut release = key(KeyCode::Char('a'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(map(release, true), KeyAction::Ignore);
        assert_eq!(
            map(key(KeyCode::Char('?'), KeyModifiers::NONE), true),
            KeyAction::Help
        );
        assert_eq!(
            map(key(KeyCode::Char('?'), KeyModifiers::NONE), false),
            KeyAction::Edit(EditKey::Char('?'))
        );
    }
}
