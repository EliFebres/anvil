//! The startup screen: the logo, what the session is, the standing rules
//! and the last few sessions. It is the first transcript entry, so it
//! scrolls away as the session proceeds.

use std::time::{Duration, Instant};

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::theme::{Shade, Theme};
use super::wrap::wrap;
use super::{PermissionMode, RecentSession, SessionInfo};

/// The word ANVIL in figlet's slant font.
pub const LOGO: [&str; 5] = [
    "    ___    _   ___    ________ ",
    "   /   |  / | / / |  / /  _/ / ",
    "  / /| | /  |/ /| | / // // /  ",
    " / ___ |/ /|  / | |/ // // /___",
    "/_/  |_/_/ |_/  |___/___/_____/",
];

/// The tagline under the version.
const TAGLINE: &str = "Eli's personal coding agent that constantly improves itself";

/// How long after the previous row the next logo row appears.
const ROW_DELAY: Duration = Duration::from_millis(80);

/// How long a row takes to brighten from muted to the accent.
const BRIGHTEN: Duration = Duration::from_millis(160);

/// Columns between the logo and the text beside it.
const GAP: usize = 3;

/// What the startup screen shows.
#[derive(Debug, Clone)]
pub struct Welcome {
    version: String,
    project: String,
    coder: String,
    reviewer: Option<String>,
    mode: PermissionMode,
    rules: Vec<(String, String)>,
    skills: usize,
    mcp_servers: usize,
    sessions: Vec<RecentSession>,
    started: Instant,
    animate: bool,
}

impl Welcome {
    /// The startup screen for a session.
    ///
    /// # Arguments
    ///
    /// * `info` - The session.
    /// * `now` - When the screen first drew, for the animation.
    ///
    /// # Returns
    ///
    /// The screen.
    pub fn new(info: &SessionInfo, now: Instant) -> Self {
        Self {
            version: info.version.clone(),
            project: info.cwd.display().to_string(),
            coder: info.coder.clone(),
            reviewer: info.reviewer.clone(),
            mode: info.permission_mode,
            rules: info.rules.clone(),
            skills: info.skills,
            mcp_servers: info.mcp_servers,
            sessions: info.recent_sessions.clone(),
            started: now,
            animate: info.animate,
        }
    }

    /// Whether the logo is still drawing in, so the entry must redraw.
    ///
    /// # Arguments
    ///
    /// * `now` - The current time.
    ///
    /// # Returns
    ///
    /// True while a row is still brightening.
    pub fn is_animating(&self, now: Instant) -> bool {
        self.animate && now.saturating_duration_since(self.started) < self.finished_after()
    }

    /// The lines at a width.
    ///
    /// # Arguments
    ///
    /// * `width` - Columns.
    /// * `theme` - Styles.
    /// * `now` - The current time, for the animation.
    ///
    /// # Returns
    ///
    /// The lines.
    pub fn lines(&self, width: usize, theme: &Theme, now: Instant) -> Vec<Line<'static>> {
        let logo_width = LOGO.iter().map(|row| row.width()).max().unwrap_or(0);
        let right_width = width.saturating_sub(logo_width + GAP).max(20);
        let right = self.header(right_width, theme);
        let rows = right.len().max(LOGO.len());
        let logo_top = rows.saturating_sub(LOGO.len()) / 2;
        let mut lines = Vec::new();
        for row in 0..rows {
            let mut spans = Vec::new();
            let logo_row = row.checked_sub(logo_top).filter(|r| *r < LOGO.len());
            match logo_row.and_then(|r| self.logo_shade(r, now).map(|shade| (r, shade))) {
                Some((r, shade)) => spans.push(Span::styled(
                    format!("{:<logo_width$}", LOGO[r]),
                    theme.bold(shade),
                )),
                None => spans.push(Span::raw(" ".repeat(logo_width))),
            }
            spans.push(Span::raw(" ".repeat(GAP)));
            if let Some(line) = right.get(row) {
                spans.extend(line.spans.iter().cloned());
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::default());
        lines.push(Line::styled("standing rules", theme.fg(Shade::Muted)));
        if self.rules.is_empty() {
            lines.push(Line::styled(
                "  none yet; `rule: <text>` adds one",
                theme.fg(Shade::Faint),
            ));
        }
        let id_width = self
            .rules
            .iter()
            .map(|(id, _)| id.width())
            .max()
            .unwrap_or(0);
        for (id, text) in &self.rules {
            let lead = format!("  {id:<id_width$}  ");
            for (index, part) in wrap(text, width.saturating_sub(lead.width()).max(1))
                .into_iter()
                .enumerate()
            {
                let head = if index == 0 {
                    Span::styled(lead.clone(), theme.fg(Shade::Faint))
                } else {
                    Span::raw(" ".repeat(lead.width()))
                };
                lines.push(Line::from(vec![
                    head,
                    Span::styled(part, theme.fg(Shade::Secondary)),
                ]));
            }
        }
        lines.push(Line::default());
        lines.push(Line::styled("recent sessions", theme.fg(Shade::Muted)));
        if self.sessions.is_empty() {
            lines.push(Line::styled(
                "  none in this project",
                theme.fg(Shade::Faint),
            ));
        }
        for session in &self.sessions {
            let status_shade = match session.ok {
                Some(true) => Shade::Success,
                Some(false) => Shade::Danger,
                None => Shade::Muted,
            };
            let used =
                2 + session.id.width() + 2 + session.status.width() + 2 + session.when.width() + 2;
            let task = fit(&session.task, width.saturating_sub(used).max(8));
            lines.push(Line::from(vec![
                Span::styled(format!("  {}  ", session.id), theme.fg(Shade::Faint)),
                Span::styled(session.status.clone(), theme.fg(status_shade)),
                Span::styled(format!("  {task}  "), theme.fg(Shade::Text)),
                Span::styled(session.when.clone(), theme.fg(Shade::Faint)),
            ]));
        }
        lines
    }

    /// The block beside the logo: version, tagline and the key-value lines.
    fn header(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = vec![Line::styled(
            format!("anvil {}", self.version),
            theme.bold(Shade::Text),
        )];
        lines.extend(
            wrap(TAGLINE, width)
                .into_iter()
                .map(|line| Line::styled(line, theme.fg(Shade::Muted))),
        );
        lines.push(Line::default());
        let pair = |key: &str, value: String| {
            Line::from(vec![
                Span::styled(format!("{key:<9}"), theme.fg(Shade::Muted)),
                Span::styled(value, theme.fg(Shade::Secondary)),
            ])
        };
        lines.push(pair(
            "project",
            fit(&self.project, width.saturating_sub(9).max(8)),
        ));
        lines.push(pair("coder", self.coder.clone()));
        lines.push(pair(
            "reviewer",
            self.reviewer.clone().unwrap_or_else(|| "none".to_owned()),
        ));
        lines.push(Line::from(vec![
            Span::styled(format!("{:<9}", "mode"), theme.fg(Shade::Muted)),
            Span::styled(self.mode.label().to_owned(), theme.fg(Shade::Secondary)),
            Span::styled("  shift+tab to cycle", theme.fg(Shade::Faint)),
        ]));
        lines.push(pair(
            "loaded",
            format!(
                "{} · {} · {}",
                count(self.rules.len(), "rule"),
                count(self.skills, "skill"),
                count(self.mcp_servers, "mcp server")
            ),
        ));
        lines
    }

    /// The shade of a logo row at this moment, or `None` before it appears.
    fn logo_shade(&self, row: usize, now: Instant) -> Option<Shade> {
        if !self.animate {
            return Some(Shade::Accent);
        }
        let appears = ROW_DELAY.saturating_mul(u32::try_from(row).unwrap_or(u32::MAX));
        let age = now
            .saturating_duration_since(self.started)
            .checked_sub(appears)?;
        if age < BRIGHTEN / 2 {
            Some(Shade::Muted)
        } else if age < BRIGHTEN {
            Some(Shade::Secondary)
        } else {
            Some(Shade::Accent)
        }
    }

    /// How long after the start the last row reaches the accent.
    fn finished_after(&self) -> Duration {
        ROW_DELAY.saturating_mul(u32::try_from(LOGO.len() - 1).unwrap_or(u32::MAX)) + BRIGHTEN
    }
}

/// `1 rule` or `3 rules`.
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Cuts text to `width` columns with an ellipsis.
fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use anvil_core::agent::State;

    use super::*;
    use crate::tui::{SessionKind, animate_logo};

    fn info(animate: bool) -> SessionInfo {
        SessionInfo {
            kind: SessionKind::Chat,
            cwd: PathBuf::from("/work/anvil"),
            model: "claude-sonnet-5-5".to_owned(),
            context_window: 200_000,
            price: None,
            permission_mode: PermissionMode::Ask,
            initial_state: State::Build,
            slash_commands: Vec::new(),
            version: "0.1.0".to_owned(),
            coder: "main (claude-sonnet-5-5)".to_owned(),
            reviewer: Some("reviewer (gpt-5)".to_owned()),
            rules: vec![(
                "no-unwrap".to_owned(),
                "No unwrap outside tests.".to_owned(),
            )],
            skills: 2,
            mcp_servers: 0,
            recent_sessions: vec![RecentSession {
                id: "01J".to_owned(),
                status: "done".to_owned(),
                ok: Some(true),
                task: "add a flag".to_owned(),
                when: "2026-11-01 09:30".to_owned(),
            }],
            animate,
        }
    }

    #[test]
    fn animation_is_skipped_off_a_tty_or_when_asked() {
        assert!(animate_logo(true, false));
        assert!(!animate_logo(false, false));
        assert!(!animate_logo(true, true));
    }

    #[test]
    fn rows_appear_one_by_one_and_brighten() {
        let start = Instant::now();
        let welcome = Welcome::new(&info(true), start);
        assert!(welcome.is_animating(start));
        assert_eq!(welcome.logo_shade(0, start), Some(Shade::Muted));
        assert_eq!(welcome.logo_shade(1, start), None);
        assert_eq!(
            welcome.logo_shade(0, start + Duration::from_millis(100)),
            Some(Shade::Secondary)
        );
        assert_eq!(
            welcome.logo_shade(1, start + Duration::from_millis(100)),
            Some(Shade::Muted)
        );
        let done = start + Duration::from_secs(1);
        for row in 0..LOGO.len() {
            assert_eq!(welcome.logo_shade(row, done), Some(Shade::Accent));
        }
        assert!(!welcome.is_animating(done));
        let still = Welcome::new(&info(false), start);
        assert!(!still.is_animating(start));
        assert_eq!(still.logo_shade(4, start), Some(Shade::Accent));
    }

    #[test]
    fn the_logo_sits_beside_the_header_and_sections_follow() {
        let start = Instant::now();
        let welcome = Welcome::new(&info(false), start);
        let lines = welcome.lines(100, &Theme::mono(), start);
        let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert!(
            text[0].starts_with("                                  anvil 0.1.0"),
            "{:?}",
            text[0]
        );
        assert!(text[1].ends_with(TAGLINE));
        assert!(
            text[3].contains("project  /work/anvil") || text[3].contains("project  \\work\\anvil"),
            "{:?}",
            text[3]
        );
        assert!(
            text[6].contains("mode     ask  shift+tab to cycle"),
            "{:?}",
            text[6]
        );
        assert!(
            text[7].contains("1 rule · 2 skills · 0 mcp servers"),
            "{:?}",
            text[7]
        );
        let logo_rows: Vec<&String> = text
            .iter()
            .filter(|line| line.starts_with(LOGO[0]) || line.starts_with(LOGO[4]))
            .collect();
        assert_eq!(logo_rows.len(), 2);
        assert!(text.iter().any(|line| line == "standing rules"));
        assert!(
            text.iter()
                .any(|line| line.starts_with("  no-unwrap  No unwrap outside tests."))
        );
        assert!(text.iter().any(|line| line == "recent sessions"));
        assert!(
            text.iter()
                .any(|line| line.starts_with("  01J  done  add a flag  2026-11-01 09:30"))
        );
    }
}
