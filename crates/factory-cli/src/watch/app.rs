//! The dashboard's state: the last snapshot, the pause flag and the clock.
//! It turns messages into actions and draws one of the two screens.

use anvil_tui::layout::centered;
use anvil_tui::{Shade, Theme};
use chrono::{DateTime, FixedOffset, Utc};
use factory_core::Status;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::parts::{Ctx, key_line, top_bar};
use super::{Action, PAUSE_REASON, WatchMsg, idle, working};

/// How long a footer notice stays, in seconds.
const NOTICE_SECS: i64 = 8;

/// The dashboard.
#[derive(Debug)]
pub struct App {
    theme: Theme,
    tz: FixedOffset,
    now: DateTime<Utc>,
    status: Option<Box<Status>>,
    pause_flag: Option<String>,
    read_error: Option<String>,
    notice: Option<(String, DateTime<Utc>)>,
}

impl App {
    /// A dashboard with nothing read yet.
    ///
    /// # Arguments
    ///
    /// * `theme` - The palette.
    /// * `tz` - The time zone times are shown in.
    /// * `now` - The current time.
    ///
    /// # Returns
    ///
    /// The app.
    pub fn new(theme: Theme, tz: FixedOffset, now: DateTime<Utc>) -> Self {
        Self {
            theme,
            tz,
            now,
            status: None,
            pause_flag: None,
            read_error: None,
            notice: None,
        }
    }

    /// Takes in one message.
    ///
    /// # Arguments
    ///
    /// * `msg` - A status read, a key, a tick or a notice.
    ///
    /// # Returns
    ///
    /// What the runner should do, if anything. `p` and `r` change the top
    /// bar at once, before the service has seen the flag.
    pub fn handle(&mut self, msg: WatchMsg) -> Option<Action> {
        match msg {
            WatchMsg::Status {
                status,
                pause_flag,
                read_error,
            } => {
                if status.is_some() {
                    self.status = status;
                }
                self.pause_flag = pause_flag;
                self.read_error = read_error;
                None
            }
            WatchMsg::Tick(now) => {
                self.now = now;
                if self
                    .notice
                    .as_ref()
                    .is_some_and(|(_, at)| (now - *at).num_seconds() >= NOTICE_SECS)
                {
                    self.notice = None;
                }
                None
            }
            WatchMsg::Notice(text) => {
                self.notice = Some((text, self.now));
                None
            }
            WatchMsg::Key(key) => self.key(key),
        }
    }

    /// Maps a key to an action.
    fn key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> Option<Action> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let ticket = self
            .status
            .as_ref()
            .and_then(|status| status.ticket.as_ref());
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::Quit)
            }
            KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
            KeyCode::Char('p') if self.pause_flag.is_none() => {
                self.pause_flag = Some(PAUSE_REASON.to_owned());
                Some(Action::Pause)
            }
            KeyCode::Char('r') if self.pause_flag.is_some() => {
                self.pause_flag = None;
                Some(Action::Resume)
            }
            KeyCode::Char('l') => Some(Action::OpenLog),
            KeyCode::Enter => {
                let id = match ticket {
                    Some(ticket) => Some(ticket.id.clone()),
                    None => self
                        .status
                        .as_ref()
                        .and_then(|status| status.recent.first())
                        .map(|record| record.id.clone()),
                };
                id.map(Action::OpenTicket)
            }
            KeyCode::Char('o') => {
                let ticket = ticket?;
                match ticket.pr {
                    Some(pr) => {
                        let repo = self
                            .status
                            .as_ref()
                            .map(|status| status.repo.clone())
                            .unwrap_or_default();
                        Some(Action::OpenPr(format!(
                            "https://github.com/{repo}/pull/{pr}"
                        )))
                    }
                    None => {
                        self.notice = Some(("no pull request yet".to_owned(), self.now));
                        None
                    }
                }
            }
            KeyCode::Char('d') if ticket.is_none() => Some(Action::Doctor),
            _ => None,
        }
    }

    /// Draws the whole screen: top bar, the working or idle screen, keys.
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to draw into.
    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(Block::new().style(self.theme.on_bg(Style::new())), area);
        let [top, body, bottom] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(area);
        let ctx = Ctx {
            theme: &self.theme,
            now: self.now,
            tz: self.tz,
            pause_flag: self.pause_flag.as_deref(),
        };
        let width = usize::from(area.width);
        let notice = self.notice.as_ref().map(|(text, _)| text.as_str());
        let Some(status) = self.status.as_deref() else {
            self.draw_waiting(frame, top, body);
            frame.render_widget(
                Paragraph::new(key_line(&[("q", "quit")], notice, &ctx, width)),
                bottom,
            );
            return;
        };
        frame.render_widget(Paragraph::new(top_bar(status, &ctx, width)), top);
        let pause_key = if self.pause_flag.is_some() {
            ("r", "resume")
        } else if status.ticket.is_some() {
            ("p", "pause after this ticket")
        } else {
            ("p", "pause")
        };
        let keys: Vec<(&str, &str)> = match &status.ticket {
            Some(ticket) => {
                working::draw(frame, body, status, ticket, &ctx);
                vec![
                    ("q", "quit"),
                    pause_key,
                    ("l", "full log"),
                    ("enter", "open the ticket"),
                    ("o", "open PR in browser"),
                ]
            }
            None => {
                idle::draw(frame, body, status, &ctx);
                vec![
                    ("q", "quit"),
                    pause_key,
                    ("l", "full log"),
                    ("enter", "open a recent ticket"),
                    ("d", "run doctor"),
                ]
            }
        };
        frame.render_widget(Paragraph::new(key_line(&keys, notice, &ctx, width)), bottom);
    }

    /// The screen before the first good read of the status file.
    fn draw_waiting(&self, frame: &mut Frame, top: Rect, body: Rect) {
        let theme = &self.theme;
        let bar = Line::from(vec![
            Span::raw(" "),
            Span::styled("factory", theme.bold(Shade::Accent)),
            Span::styled(" · no status yet", theme.fg(Shade::Muted)),
        ])
        .style(theme.on_surface(Style::new()));
        frame.render_widget(Paragraph::new(bar), top);
        let mut lines = vec![Line::styled(
            "waiting for the service to write its status file",
            theme.fg(Shade::Secondary),
        )];
        if let Some(error) = &self.read_error {
            lines.push(Line::styled(error.clone(), theme.fg(Shade::Faint)));
        }
        let height = u16::try_from(lines.len()).unwrap_or(2);
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            centered(body, body.width, height),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyEvent;

    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(format!("status-{name}.json"))
    }

    fn load(name: &str) -> Status {
        let text = std::fs::read_to_string(fixture(name)).expect("fixture");
        serde_json::from_str(&text).expect("fixture parses")
    }

    fn app_for(name: &str) -> App {
        let status = load(name);
        let now = status.written_at.expect("written_at");
        // The runner reads the flag file; a fixture carries its reason.
        let pause_flag = status.service.reason.clone();
        let mut app = App::new(Theme::mono(), FixedOffset::east_opt(0).expect("utc"), now);
        app.handle(WatchMsg::Status {
            status: Some(Box::new(status)),
            pause_flag,
            read_error: None,
        });
        app
    }

    fn draw(app: &App, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        terminal
    }

    fn rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect()
            })
            .collect()
    }

    fn key(code: KeyCode) -> WatchMsg {
        WatchMsg::Key(KeyEvent::from(code))
    }

    #[test]
    fn working_120() {
        insta::assert_snapshot!(draw(&app_for("working"), 120, 38).backend());
    }

    #[test]
    fn working_80() {
        insta::assert_snapshot!(draw(&app_for("working"), 80, 24).backend());
    }

    #[test]
    fn idle_120() {
        insta::assert_snapshot!(draw(&app_for("idle"), 120, 38).backend());
    }

    #[test]
    fn idle_80() {
        insta::assert_snapshot!(draw(&app_for("idle"), 80, 24).backend());
    }

    #[test]
    fn paused_120() {
        insta::assert_snapshot!(draw(&app_for("paused"), 120, 38).backend());
    }

    #[test]
    fn paused_80() {
        insta::assert_snapshot!(draw(&app_for("paused"), 80, 24).backend());
    }

    #[test]
    fn stopped_120() {
        insta::assert_snapshot!(draw(&app_for("stopped"), 120, 38).backend());
    }

    #[test]
    fn stopped_80() {
        insta::assert_snapshot!(draw(&app_for("stopped"), 80, 24).backend());
    }

    #[test]
    fn an_idle_snapshot_long_past_its_poll_says_not_responding() {
        let status = load("idle");
        let next = status.next_poll_at.expect("next_poll_at");
        let fresh = rows(&draw(&app_for("idle"), 120, 38));
        assert!(fresh[0].contains("idle"), "{}", fresh[0]);
        assert!(fresh[0].contains("next poll in"), "{}", fresh[0]);

        let mut app = App::new(
            Theme::mono(),
            FixedOffset::east_opt(0).expect("utc"),
            next + chrono::Duration::minutes(30),
        );
        app.handle(WatchMsg::Status {
            status: Some(Box::new(status)),
            pause_flag: None,
            read_error: None,
        });
        let top = &rows(&draw(&app, 120, 38))[0];
        assert!(top.contains("not responding · last update"), "{top}");
        assert!(!top.contains("next poll"), "{top}");
        assert!(!top.contains("idle"), "{top}");
    }

    #[test]
    fn p_on_the_working_screen_pauses_after_the_ticket_at_once() {
        let mut app = app_for("working");
        assert!(!rows(&draw(&app, 120, 38))[0].contains("pausing"));
        assert_eq!(app.handle(key(KeyCode::Char('p'))), Some(Action::Pause));
        let top = rows(&draw(&app, 120, 38)).remove(0);
        assert!(top.contains("running · pausing after this ticket"), "{top}");
        let footer = rows(&draw(&app, 120, 38)).pop().expect("footer");
        assert!(footer.contains("r resume"), "{footer}");
        assert_eq!(app.handle(key(KeyCode::Char('p'))), None, "already paused");
        assert_eq!(app.handle(key(KeyCode::Char('r'))), Some(Action::Resume));
        assert!(!rows(&draw(&app, 120, 38))[0].contains("pausing"));
    }

    #[test]
    fn p_on_the_idle_screen_shows_paused_and_r_resumes() {
        let mut app = app_for("idle");
        assert_eq!(app.handle(key(KeyCode::Char('p'))), Some(Action::Pause));
        let top = rows(&draw(&app, 120, 38)).remove(0);
        assert!(top.contains(&format!("paused · {PAUSE_REASON}")), "{top}");
        assert_eq!(app.handle(key(KeyCode::Char('r'))), Some(Action::Resume));
        assert!(rows(&draw(&app, 120, 38))[0].contains("idle"));
    }

    #[test]
    fn keys_map_to_actions() {
        let mut app = app_for("working");
        assert_eq!(app.handle(key(KeyCode::Char('q'))), Some(Action::Quit));
        assert_eq!(app.handle(key(KeyCode::Char('l'))), Some(Action::OpenLog));
        assert_eq!(
            app.handle(key(KeyCode::Enter)),
            Some(Action::OpenTicket("01M6D210H709VGC2CFEG4BTQ7M".to_owned()))
        );
        assert_eq!(app.handle(key(KeyCode::Char('o'))), None);
        assert!(
            rows(&draw(&app, 120, 38))
                .pop()
                .expect("footer")
                .contains("no pull request yet")
        );
        assert_eq!(
            app.handle(key(KeyCode::Char('d'))),
            None,
            "no doctor mid-ticket"
        );

        let mut app = app_for("idle");
        assert_eq!(app.handle(key(KeyCode::Char('d'))), Some(Action::Doctor));
        assert_eq!(app.handle(key(KeyCode::Char('o'))), None);
    }

    #[test]
    fn a_failed_read_keeps_the_last_snapshot() {
        let mut app = app_for("working");
        app.handle(WatchMsg::Status {
            status: None,
            pause_flag: None,
            read_error: Some("EOF while parsing".to_owned()),
        });
        assert!(rows(&draw(&app, 120, 38))[1].contains("working"));
    }

    #[test]
    fn before_the_first_read_it_says_it_is_waiting() {
        let app = App::new(
            Theme::mono(),
            FixedOffset::east_opt(0).expect("utc"),
            Utc::now(),
        );
        let text = rows(&draw(&app, 80, 24)).join("\n");
        assert!(text.contains("waiting for the service"), "{text}");
    }
}
