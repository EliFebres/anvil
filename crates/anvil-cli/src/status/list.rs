//! `anvil status`: one line (or JSON object) per session, from the
//! `status.json` files.
//!
//! `stale` exists only here: a session the file calls `working` or
//! `needs_input` whose process no longer holds the liveness lock died
//! without saying so.

use std::io::Write;
use std::path::{Path, PathBuf};

use anvil_types::{LoopState, SessionSpend, SessionStatus};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::file::{lock_is_free, read_status, registered_projects, session_dirs};

/// A session as `anvil status` shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    /// The format version of the file the row came from.
    pub schema: u32,
    /// The session id.
    pub session_id: String,
    /// The project directory.
    pub project: String,
    /// The first line of the task.
    pub task: String,
    /// The recorded state, or `stale` for a live state whose process is gone.
    pub state: String,
    /// The loop step.
    pub loop_state: Option<LoopState>,
    /// One line saying more.
    pub detail: String,
    /// Tokens and cost.
    pub spend: SessionSpend,
    /// When the session started.
    pub started_at: DateTime<Utc>,
    /// When the file was last written.
    pub updated_at: DateTime<Utc>,
}

impl Row {
    /// The row for a status and whether its process is gone.
    ///
    /// # Arguments
    ///
    /// * `status` - The file's content.
    /// * `lock_free` - Whether nobody holds the session's liveness lock.
    ///
    /// # Returns
    ///
    /// The row; `state` is `stale` for `working` or `needs_input` with a free lock.
    pub fn new(status: SessionStatus, lock_free: bool) -> Self {
        let state = if status.state.is_live() && lock_free {
            "stale"
        } else {
            status.state.name()
        };
        Self {
            schema: status.schema,
            session_id: status.session_id,
            project: status.project,
            task: status.task,
            state: state.to_owned(),
            loop_state: status.loop_state,
            detail: status.detail,
            spend: status.spend,
            started_at: status.started_at,
            updated_at: status.updated_at,
        }
    }
}

/// The project roots to list.
///
/// # Arguments
///
/// * `cwd` - The current project.
/// * `home` - The anvil home, when known.
/// * `all` - Whether to include every project that ran a session.
///
/// # Returns
///
/// The current project, then (with `all`) the registered ones, once each.
pub fn projects(cwd: &Path, home: Option<&Path>, all: bool) -> Vec<PathBuf> {
    let mut projects = vec![cwd.to_path_buf()];
    if all && let Some(home) = home {
        for project in registered_projects(home) {
            if !projects.contains(&project) {
                projects.push(project);
            }
        }
    }
    projects
}

/// Reads every session of the projects, newest first. Sessions without a
/// valid `status.json` (older sessions) are skipped.
///
/// # Arguments
///
/// * `projects` - Project roots.
///
/// # Returns
///
/// The rows, ordered by start time, newest first; ties by session id.
pub fn collect(projects: &[PathBuf]) -> Vec<Row> {
    let mut rows: Vec<Row> = projects
        .iter()
        .flat_map(|project| session_dirs(project))
        .filter_map(|dir| read_status(&dir).map(|status| Row::new(status, lock_is_free(&dir))))
        .collect();
    rows.sort_by(|a, b| (b.started_at, &b.session_id).cmp(&(a.started_at, &a.session_id)));
    rows
}

/// Prints the sessions of the current project, or of every project.
///
/// # Arguments
///
/// * `cwd` - The current project.
/// * `home` - The anvil home, when known.
/// * `all` - Whether to include every project that ran a session.
/// * `json` - Print a JSON array instead of lines.
/// * `now` - The time ages are measured to.
/// * `out` - Where to print.
///
/// # Errors
///
/// When writing to `out` fails.
pub fn run(
    cwd: &Path,
    home: Option<&Path>,
    all: bool,
    json: bool,
    now: DateTime<Utc>,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let rows = collect(&projects(cwd, home, all));
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(&rows)?)?;
    } else if rows.is_empty() {
        writeln!(
            out,
            "no sessions with a status file under {}",
            cwd.display()
        )?;
    } else {
        for row in &rows {
            writeln!(out, "{}", line(row, now, all))?;
        }
    }
    Ok(())
}

/// One session as one line: state, loop state, age, spend, task, id.
fn line(row: &Row, now: DateTime<Utc>, show_project: bool) -> String {
    let loop_state = row.loop_state.map_or("-", LoopState::name);
    let mut line = format!(
        "{:<11} {:<7} {:>4}  {:<16} {}  {}",
        row.state,
        loop_state,
        age(now - row.updated_at),
        spend(&row.spend),
        row.task,
        row.session_id,
    );
    if show_project {
        line.push_str(&format!("  {}", row.project));
    }
    line
}

/// A duration as `45s`, `12m`, `3h` or `2d`.
fn age(elapsed: chrono::Duration) -> String {
    let seconds = elapsed.num_seconds().max(0);
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// Tokens and cost, such as `12.3k tok $0.42`.
fn spend(spend: &SessionSpend) -> String {
    let tokens = spend.total_tokens();
    let tokens = if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1e6)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1e3)
    } else {
        tokens.to_string()
    };
    match spend.cost_usd {
        Some(cost) => format!("{tokens} tok ${cost:.2}"),
        None => format!("{tokens} tok"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::file::{StatusFile, register_project};
    use anvil_core::context::session::SESSIONS_DIR;
    use anvil_types::SessionState;
    use anvil_types::status::SCHEMA_VERSION;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("time")
            .with_timezone(&Utc)
    }

    fn status(
        id: &str,
        project: &Path,
        state: SessionState,
        started: &str,
        updated: &str,
    ) -> SessionStatus {
        SessionStatus {
            schema: SCHEMA_VERSION,
            session_id: id.to_owned(),
            project: project.display().to_string(),
            task: format!("task of {id}"),
            state,
            loop_state: Some(LoopState::Build),
            detail: "detail".to_owned(),
            spend: SessionSpend {
                input_tokens: 10_000,
                output_tokens: 2_300,
                cached_tokens: 0,
                cost_usd: Some(0.4242),
            },
            started_at: at(started),
            updated_at: at(updated),
        }
    }

    /// Writes a session's status file; returns the lock when the session is "alive".
    fn session(project: &Path, status: &SessionStatus, alive: bool) -> Option<StatusFile> {
        let dir = project.join(SESSIONS_DIR).join(&status.session_id);
        let file = StatusFile::open(&dir).expect("open");
        file.write(status).expect("write");
        alive.then_some(file)
    }

    fn render(cwd: &Path, home: Option<&Path>, all: bool, json: bool) -> String {
        let mut out = Vec::new();
        run(cwd, home, all, json, at("2026-11-01T12:00:00Z"), &mut out).expect("run");
        String::from_utf8(out).expect("utf8").replace(
            &cwd.display().to_string().replace('\\', "\\\\"),
            "<project>",
        )
    }

    #[test]
    fn json_is_newest_first_and_marks_dead_live_sessions_stale() {
        let dir = tempfile::tempdir().expect("dir");
        let project = dir.path();
        let working = status(
            "01B",
            project,
            SessionState::Working,
            "2026-11-01T11:00:00Z",
            "2026-11-01T11:59:30Z",
        );
        let dead = status(
            "01A",
            project,
            SessionState::Working,
            "2026-11-01T09:00:00Z",
            "2026-11-01T09:10:00Z",
        );
        let done = status(
            "01C",
            project,
            SessionState::Done,
            "2026-11-01T10:00:00Z",
            "2026-11-01T10:30:00Z",
        );
        let _alive = session(project, &working, true);
        let _ = session(project, &dead, false);
        let _ = session(project, &done, false);
        // A session from before status files existed is skipped.
        std::fs::create_dir_all(project.join(SESSIONS_DIR).join("00OLD")).expect("old");
        let json = render(project, None, false, true);
        insta::assert_snapshot!("status_json", json);
        let rows: Vec<Row> = collect(&[project.to_path_buf()]);
        let order: Vec<_> = rows.iter().map(|row| row.session_id.as_str()).collect();
        assert_eq!(order, ["01B", "01C", "01A"]);
        let states: Vec<_> = rows.iter().map(|row| row.state.as_str()).collect();
        assert_eq!(states, ["working", "done", "stale"]);
    }

    #[test]
    fn lines_show_state_loop_age_spend_task_and_id() {
        let dir = tempfile::tempdir().expect("dir");
        let project = dir.path();
        let blocked = status(
            "01B",
            project,
            SessionState::Blocked,
            "2026-11-01T11:00:00Z",
            "2026-11-01T11:59:30Z",
        );
        let _ = session(project, &blocked, false);
        let text = render(project, None, false, false);
        insta::assert_snapshot!("status_lines", text);
    }

    #[test]
    fn all_covers_every_registered_project_once() {
        let home = tempfile::tempdir().expect("home");
        let one = tempfile::tempdir().expect("one");
        let two = tempfile::tempdir().expect("two");
        let first = status(
            "01A",
            one.path(),
            SessionState::Done,
            "2026-11-01T08:00:00Z",
            "2026-11-01T08:05:00Z",
        );
        let second = status(
            "01B",
            two.path(),
            SessionState::Failed,
            "2026-11-01T09:00:00Z",
            "2026-11-01T09:05:00Z",
        );
        let _ = session(one.path(), &first, false);
        let _ = session(two.path(), &second, false);
        register_project(home.path(), one.path()).expect("register");
        register_project(home.path(), two.path()).expect("register");

        let here = collect(&projects(one.path(), Some(home.path()), false));
        assert_eq!(here.len(), 1);
        let everywhere = collect(&projects(one.path(), Some(home.path()), true));
        let ids: Vec<_> = everywhere
            .iter()
            .map(|row| row.session_id.as_str())
            .collect();
        assert_eq!(ids, ["01B", "01A"]);
        let projects: Vec<_> = everywhere.iter().map(|row| row.project.clone()).collect();
        assert_eq!(
            projects,
            [
                two.path().display().to_string(),
                one.path().display().to_string()
            ]
        );
    }

    #[test]
    fn an_empty_project_says_so() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(render(dir.path(), None, false, false).starts_with("no sessions"));
        assert_eq!(render(dir.path(), None, false, true).trim(), "[]");
    }

    #[test]
    fn ages_and_spend_read_short() {
        assert_eq!(age(chrono::Duration::seconds(5)), "5s");
        assert_eq!(age(chrono::Duration::seconds(125)), "2m");
        assert_eq!(age(chrono::Duration::seconds(7_300)), "2h");
        assert_eq!(age(chrono::Duration::seconds(200_000)), "2d");
        assert_eq!(age(chrono::Duration::seconds(-9)), "0s");
        let mut value = SessionSpend::default();
        assert_eq!(spend(&value), "0 tok");
        value.input_tokens = 12_340;
        value.cost_usd = Some(0.416);
        assert_eq!(spend(&value), "12.3k tok $0.42");
        value.output_tokens = 2_000_000;
        assert_eq!(spend(&value), "2.0M tok $0.42");
    }
}
