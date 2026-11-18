//! Optional reporting to herdr, a terminal multiplexer for coding agents
//! (https://herdr.dev/tui/docs/add-herdr-support/).
//!
//! Every report is a detached child process run on a background
//! task. Only the latest state is sent, and every failure is ignored: a
//! missing or broken `herdr` binary must never touch the session.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anvil_types::SessionState;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// How long one `herdr` command may run before it is killed.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

/// The last `--seq` sent by this process.
static LAST_SEQ: AtomicU64 = AtomicU64::new(0);

/// Where the session runs: the pane herdr gave it and the binary to call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrEnv {
    /// `HERDR_BIN_PATH`.
    pub bin: PathBuf,
    /// `HERDR_PANE_ID`.
    pub pane: String,
}

impl HerdrEnv {
    /// Reads the environment herdr gives every process in its panes.
    ///
    /// # Arguments
    ///
    /// * `enabled` - The `[integrations] herdr` setting.
    /// * `get` - Reads an environment variable.
    ///
    /// # Returns
    ///
    /// The target, only when the setting is on, `HERDR_ENV` is `1`, and
    /// `HERDR_PANE_ID` and `HERDR_BIN_PATH` are set and not empty.
    pub fn detect(enabled: bool, get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        if !enabled || get("HERDR_ENV")?.trim() != "1" {
            return None;
        }
        let non_empty = |name: &str| get(name).filter(|value| !value.trim().is_empty());
        Some(Self {
            bin: PathBuf::from(non_empty("HERDR_BIN_PATH")?),
            pane: non_empty("HERDR_PANE_ID")?,
        })
    }
}

/// What herdr is told about the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HerdrState {
    /// Nothing running.
    Idle,
    /// Busy.
    Working,
    /// Cannot go on without the owner.
    Blocked,
}

impl HerdrState {
    fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

/// One thing to tell herdr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// The agent's state, with a message that explains a block.
    State(HerdrState, Option<String>),
    /// The session ended; drop the agent.
    Release,
}

/// Maps a session state to what herdr is told.
///
/// # Arguments
///
/// * `state` - The session state.
/// * `detail` - The state's detail, which explains a block.
///
/// # Returns
///
/// `working` stays `working`; `needs_input` and `blocked` are `blocked`
/// with the detail as message; `done` and `failed` are `idle`.
pub fn report_for(state: SessionState, detail: &str) -> Report {
    match state {
        SessionState::Working => Report::State(HerdrState::Working, None),
        SessionState::NeedsInput | SessionState::Blocked => {
            Report::State(HerdrState::Blocked, Some(detail.to_owned()))
        }
        SessionState::Done | SessionState::Failed => Report::State(HerdrState::Idle, None),
    }
}

/// The arguments of `herdr pane report-agent`.
///
/// # Arguments
///
/// * `pane` - The pane id.
/// * `state` - The state to report.
/// * `message` - Explains a block.
/// * `seq` - The sequence number, higher than any before.
///
/// # Returns
///
/// The arguments, without the program.
pub fn report_args(pane: &str, state: HerdrState, message: Option<&str>, seq: u64) -> Vec<String> {
    let mut args = ["pane", "report-agent", pane]
        .map(str::to_owned)
        .into_iter()
        .chain(agent_flags())
        .collect::<Vec<_>>();
    args.extend(["--state".to_owned(), state.name().to_owned()]);
    if let Some(message) = message {
        args.extend(["--message".to_owned(), message.to_owned()]);
    }
    args.extend(["--seq".to_owned(), seq.to_string()]);
    args
}

/// The arguments of `herdr pane release-agent`.
///
/// # Arguments
///
/// * `pane` - The pane id.
/// * `seq` - The sequence number, higher than any before.
///
/// # Returns
///
/// The arguments, without the program.
pub fn release_args(pane: &str, seq: u64) -> Vec<String> {
    let mut args = ["pane", "release-agent", pane]
        .map(str::to_owned)
        .into_iter()
        .chain(agent_flags())
        .collect::<Vec<_>>();
    args.extend(["--seq".to_owned(), seq.to_string()]);
    args
}

fn agent_flags() -> [String; 4] {
    ["--source", "anvil", "--agent", "anvil"].map(str::to_owned)
}

/// The next `--seq`: the current time in milliseconds, or one more than the
/// last value when the clock has not moved past it.
///
/// # Arguments
///
/// * `last` - The last value sent by this process.
/// * `now_ms` - Unix time in milliseconds.
///
/// # Returns
///
/// A value greater than `last`; it is stored in `last`.
pub fn next_seq(last: &AtomicU64, now_ms: u64) -> u64 {
    let mut previous = last.load(Ordering::SeqCst);
    loop {
        let next = now_ms.max(previous.saturating_add(1));
        match last.compare_exchange(previous, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return next,
            Err(current) => previous = current,
        }
    }
}

/// Sends reports to herdr from a background task.
#[derive(Debug)]
pub struct HerdrReporter {
    tx: mpsc::UnboundedSender<Report>,
    task: JoinHandle<()>,
}

impl HerdrReporter {
    /// Starts the background task.
    ///
    /// # Arguments
    ///
    /// * `env` - The pane and the binary.
    /// * `runtime` - The runtime to run the task on.
    ///
    /// # Returns
    ///
    /// The reporter.
    pub fn spawn(env: HerdrEnv, runtime: &tokio::runtime::Handle) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = runtime.spawn(run(env, rx));
        Self { tx, task }
    }

    /// Queues a report without waiting for it.
    ///
    /// # Arguments
    ///
    /// * `report` - What to say.
    pub fn send(&self, report: Report) {
        let _ = self.tx.send(report);
    }

    /// Sends the release, then waits a short while for it to go out.
    pub async fn release(self) {
        let _ = self.tx.send(Report::Release);
        let wait = COMMAND_TIMEOUT * 2;
        if tokio::time::timeout(wait, self.task).await.is_err() {
            tracing::debug!("herdr did not finish in time");
        }
    }
}

/// Runs queued reports, sending only the latest state when several are waiting.
async fn run(env: HerdrEnv, mut rx: mpsc::UnboundedReceiver<Report>) {
    while let Some(first) = rx.recv().await {
        let mut state = None;
        let mut release = false;
        for report in std::iter::once(first).chain(std::iter::from_fn(|| rx.try_recv().ok())) {
            match report {
                Report::State(..) => state = Some(report),
                Report::Release => release = true,
            }
        }
        if let Some(Report::State(herdr_state, message)) = state {
            let seq = next_seq(&LAST_SEQ, now_ms());
            command(
                &env,
                report_args(&env.pane, herdr_state, message.as_deref(), seq),
            )
            .await;
        }
        if release {
            let seq = next_seq(&LAST_SEQ, now_ms());
            command(&env, release_args(&env.pane, seq)).await;
            return;
        }
    }
}

/// Runs `herdr` with the arguments; a failure is logged at debug level and dropped.
async fn command(env: &HerdrEnv, args: Vec<String>) {
    let spawned = tokio::process::Command::new(&env.bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            tracing::debug!(%error, bin = %env.bin.display(), "cannot run herdr");
            return;
        }
    };
    match tokio::time::timeout(COMMAND_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) if !status.success() => tracing::debug!(%status, "herdr failed"),
        Ok(_) => {}
        Err(_) => {
            let _ = child.kill().await;
        }
    }
}

/// Unix time in milliseconds; zero if the clock is before the epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    const FULL: [(&str, &str); 3] = [
        ("HERDR_ENV", "1"),
        ("HERDR_PANE_ID", "w1-p2"),
        ("HERDR_BIN_PATH", "/usr/bin/herdr"),
    ];

    #[test]
    fn nothing_is_reported_unless_the_setting_and_the_environment_agree() {
        let target = HerdrEnv::detect(true, env_of(&FULL)).expect("target");
        assert_eq!(target.pane, "w1-p2");
        assert_eq!(target.bin, PathBuf::from("/usr/bin/herdr"));
        assert_eq!(HerdrEnv::detect(false, env_of(&FULL)), None);
        assert_eq!(HerdrEnv::detect(true, env_of(&[])), None);
        let mut off = FULL;
        off[0] = ("HERDR_ENV", "0");
        assert_eq!(HerdrEnv::detect(true, env_of(&off)), None);
        for missing in 1..3 {
            let mut pairs = FULL;
            pairs[missing].1 = "  ";
            assert_eq!(HerdrEnv::detect(true, env_of(&pairs)), None);
            let without: Vec<_> = FULL
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != missing)
                .map(|(_, pair)| *pair)
                .collect();
            assert_eq!(HerdrEnv::detect(true, env_of(&without)), None);
        }
    }

    #[test]
    fn states_map_to_herdr_states() {
        use HerdrState::{Blocked, Idle, Working};
        let table = [
            (SessionState::Working, Report::State(Working, None)),
            (
                SessionState::NeedsInput,
                Report::State(Blocked, Some("why".to_owned())),
            ),
            (
                SessionState::Blocked,
                Report::State(Blocked, Some("why".to_owned())),
            ),
            (SessionState::Done, Report::State(Idle, None)),
            (SessionState::Failed, Report::State(Idle, None)),
        ];
        for (state, report) in table {
            assert_eq!(report_for(state, "why"), report, "{state:?}");
        }
    }

    #[test]
    fn argument_lists_match_herdr_s_documented_shape() {
        assert_eq!(
            report_args("p1", HerdrState::Working, None, 7),
            [
                "pane",
                "report-agent",
                "p1",
                "--source",
                "anvil",
                "--agent",
                "anvil",
                "--state",
                "working",
                "--seq",
                "7"
            ]
        );
        assert_eq!(
            report_args("p1", HerdrState::Blocked, Some("needs a plan answer"), 8),
            [
                "pane",
                "report-agent",
                "p1",
                "--source",
                "anvil",
                "--agent",
                "anvil",
                "--state",
                "blocked",
                "--message",
                "needs a plan answer",
                "--seq",
                "8"
            ]
        );
        assert_eq!(report_args("p1", HerdrState::Idle, None, 9)[8], "idle");
        assert_eq!(
            release_args("p1", 10),
            [
                "pane",
                "release-agent",
                "p1",
                "--source",
                "anvil",
                "--agent",
                "anvil",
                "--seq",
                "10"
            ]
        );
    }

    #[test]
    fn seq_only_grows() {
        let last = AtomicU64::new(0);
        assert_eq!(next_seq(&last, 1_000), 1_000);
        assert_eq!(next_seq(&last, 1_000), 1_001);
        assert_eq!(next_seq(&last, 900), 1_002);
        assert_eq!(next_seq(&last, 5_000), 5_000);
    }

    #[tokio::test]
    async fn a_missing_binary_is_ignored() {
        let env = HerdrEnv {
            bin: PathBuf::from("/definitely/not/herdr"),
            pane: "p".to_owned(),
        };
        let reporter = HerdrReporter::spawn(env, &tokio::runtime::Handle::current());
        reporter.send(Report::State(HerdrState::Working, None));
        reporter.send(Report::State(HerdrState::Idle, None));
        reporter.release().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_binary_is_ignored_and_the_latest_state_is_sent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("dir");
        let log = dir.path().join("calls.txt");
        let script = dir.path().join("herdr");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho \"$@\" >> {}\nexit 3\n", log.display()),
        )
        .expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let env = HerdrEnv {
            bin: script,
            pane: "p".to_owned(),
        };
        let reporter = HerdrReporter::spawn(env, &tokio::runtime::Handle::current());
        reporter.send(Report::State(HerdrState::Idle, None));
        reporter.release().await;
        let calls = std::fs::read_to_string(&log).expect("log");
        assert!(calls.contains("--state idle"), "{calls}");
        assert!(calls.contains("release-agent"), "{calls}");
    }
}
