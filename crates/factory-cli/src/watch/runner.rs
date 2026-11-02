//! The only part of `factory watch` that touches the terminal: it rereads
//! the status file every two seconds, feeds keys to the [`App`] and runs
//! what the keys ask for.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anvil_tui::Theme;
use anyhow::Context;
use chrono::{FixedOffset, Local, Offset, Utc};
use factory_core::{FactoryHome, StatusReader, service};
use ratatui::DefaultTerminal;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{self, Event};

use super::{Action, App, PAUSE_REASON, REFRESH_SECS, WatchMsg};

/// How long one wait for a key lasts; the clock redraws after each.
const KEY_WAIT: Duration = Duration::from_millis(500);

/// The size `--once` draws at when stdout is not a terminal.
const ONCE_SIZE: (u16, u16) = (120, 38);

/// How `factory watch` runs.
#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// The factory home: the status file and the pause flag live here.
    pub home: FactoryHome,
    /// A status file to show instead of the service's. The pause keys then
    /// only change the screen and never write the real flag.
    pub file: Option<PathBuf>,
    /// Print the screen once as plain text and exit.
    pub once: bool,
}

impl WatchOptions {
    /// The status file to read.
    fn status_file(&self) -> PathBuf {
        self.file.clone().unwrap_or_else(|| self.home.status_file())
    }
}

/// Runs the dashboard until `q`, or prints it once with `--once`.
///
/// # Arguments
///
/// * `options` - Where to read from and how to run.
///
/// # Errors
///
/// When the terminal cannot be drawn to or the pause flag cannot be
/// written.
pub fn watch(options: &WatchOptions) -> anyhow::Result<()> {
    if options.once {
        print!("{}", render_once(options));
        return Ok(());
    }
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, options);
    ratatui::restore();
    result
}

/// Draws the screen once in monochrome and returns it as plain text.
fn render_once(options: &WatchOptions) -> String {
    let (width, height) = if std::io::stdout().is_terminal() {
        ratatui::crossterm::terminal::size().unwrap_or(ONCE_SIZE)
    } else {
        ONCE_SIZE
    };
    let mut reader = StatusReader::new();
    let mut app = App::new(Theme::mono(), tz_for(options), Utc::now());
    let msg = read(&mut reader, options, None);
    let skew = fixture_skew(&msg, options);
    app.handle(msg);
    app.handle(WatchMsg::Tick(Utc::now() + skew));
    screen_text(&app, width, height)
}

/// The screen as plain text, one line per row with trailing spaces cut.
///
/// # Arguments
///
/// * `app` - The dashboard.
/// * `width` - Columns.
/// * `height` - Rows.
///
/// # Returns
///
/// The text, each row ending in a newline.
pub fn screen_text(app: &App, width: u16, height: u16) -> String {
    let Ok(mut terminal) = Terminal::new(TestBackend::new(width, height));
    if terminal.draw(|frame| app.draw(frame)).is_err() {
        return String::new();
    }
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        out.push_str(row.trim_end());
        out.push('\n');
    }
    out
}

/// The loop: reread, tick, draw, wait for a key, act on it.
fn run(terminal: &mut DefaultTerminal, options: &WatchOptions) -> anyhow::Result<()> {
    let refresh = Duration::from_secs(REFRESH_SECS);
    let mut reader = StatusReader::new();
    let mut app = App::new(Theme::detect(), tz_for(options), Utc::now());
    let mut fixture_pause: Option<String> = None;
    let mut skew = chrono::Duration::zero();
    let mut last_read: Option<Instant> = None;
    loop {
        if last_read.is_none_or(|at| at.elapsed() >= refresh) {
            let msg = read(&mut reader, options, fixture_pause.clone());
            if last_read.is_none() {
                skew = fixture_skew(&msg, options);
            }
            app.handle(msg);
            last_read = Some(Instant::now());
        }
        app.handle(WatchMsg::Tick(Utc::now() + skew));
        terminal.draw(|frame| app.draw(frame))?;
        if !event::poll(KEY_WAIT)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        let Some(action) = app.handle(WatchMsg::Key(key)) else {
            continue;
        };
        match action {
            Action::Quit => return Ok(()),
            Action::Pause if options.file.is_some() => {
                fixture_pause = Some(PAUSE_REASON.to_owned());
            }
            Action::Resume if options.file.is_some() => fixture_pause = None,
            Action::Pause => {
                service::pause(&options.home, PAUSE_REASON).context("writing the pause flag")?
            }
            Action::Resume => service::resume(&options.home).context("clearing the pause flag")?,
            Action::OpenLog => {
                let mut command = Command::new("journalctl");
                command.args(["--user", "-u", "factory.service", "-e"]);
                if let Some(notice) = suspend(terminal, command, false) {
                    app.handle(WatchMsg::Notice(notice));
                }
            }
            Action::OpenTicket(id) => match ticket_file(&options.home, &id) {
                Some(path) => {
                    if let Some(notice) = suspend(terminal, pager(&path), false) {
                        app.handle(WatchMsg::Notice(notice));
                    }
                }
                None => {
                    app.handle(WatchMsg::Notice(format!("no ticket file for {id}")));
                }
            },
            Action::OpenPr(url) => {
                if !open_in_browser(&url) {
                    app.handle(WatchMsg::Notice(url));
                }
            }
            Action::Doctor => {
                let mut command = Command::new("anvil");
                command.arg("doctor");
                if let Some(notice) = suspend(terminal, command, true) {
                    app.handle(WatchMsg::Notice(notice));
                }
            }
        }
        last_read = None;
    }
}

/// One read of the status file and the pause flag.
fn read(
    reader: &mut StatusReader,
    options: &WatchOptions,
    fixture_pause: Option<String>,
) -> WatchMsg {
    let status = reader.poll(&options.status_file()).cloned().map(Box::new);
    let pause_flag = if options.file.is_some() {
        fixture_pause
    } else {
        service::pause_reason(&options.home)
    };
    WatchMsg::Status {
        status,
        pause_flag,
        read_error: reader.last_error().map(str::to_owned),
    }
}

/// Leaves the full screen, runs a command in the plain terminal and comes
/// back.
///
/// # Returns
///
/// A notice when the command could not run.
fn suspend(terminal: &mut DefaultTerminal, mut command: Command, wait: bool) -> Option<String> {
    ratatui::restore();
    let program = command.get_program().to_string_lossy().into_owned();
    let notice = match command.status() {
        Ok(_) => None,
        Err(error) => Some(format!("cannot run {program}: {error}")),
    };
    if wait && notice.is_none() {
        println!("\npress enter to go back to the dashboard");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
    *terminal = ratatui::init();
    let _ = terminal.clear();
    notice
}

/// `$PAGER` (or `less`) on a file.
fn pager(path: &Path) -> Command {
    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".to_owned());
    let mut words = pager.split_whitespace();
    let mut command = Command::new(words.next().unwrap_or("less"));
    command.args(words).arg(path);
    command
}

/// The ticket's file in whichever state folder holds it.
fn ticket_file(home: &FactoryHome, id: &str) -> Option<PathBuf> {
    anvil_types::ticket::dirs::ALL
        .iter()
        .map(|dir| home.state_dir().join(dir).join(format!("{id}.toml")))
        .find(|path| path.is_file())
}

/// Opens a URL with `xdg-open` when there is a display to open it on.
///
/// # Returns
///
/// Whether it opened; over ssh there is no display, so the caller shows
/// the URL instead.
fn open_in_browser(url: &str) -> bool {
    let has_display =
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    has_display
        && Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

/// The time zone times are shown in: this machine's, or UTC for a
/// fixture file so it reads the same on every machine.
fn tz_for(options: &WatchOptions) -> FixedOffset {
    if options.file.is_some() {
        Utc.fix()
    } else {
        Local::now().offset().fix()
    }
}

/// For a fixture file, how far its clock is from ours, so the screen runs
/// as if it were the moment the fixture was written; zero for the
/// service's own file.
fn fixture_skew(msg: &WatchMsg, options: &WatchOptions) -> chrono::Duration {
    match msg {
        WatchMsg::Status {
            status: Some(status),
            ..
        } if options.file.is_some() => status
            .written_at
            .map(|written| written - Utc::now())
            .unwrap_or_else(chrono::Duration::zero),
        _ => chrono::Duration::zero(),
    }
}
