//! The pure state machine behind `status.json`: it maps what a session
//! observes (loop events, prompts waiting and answered, how the run ended)
//! to a [`Snapshot`]. No files, no terminal, no clock.

use anvil_core::agent::{IncompleteReason, LoopEvent, State, Status};
use anvil_types::{LoopState, Price, SessionSpend, SessionState};

/// Longest `detail` kept, in characters.
const DETAIL_LIMIT: usize = 200;

/// What the status file says at one moment, apart from its timestamps.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// What the session is doing.
    pub state: SessionState,
    /// The loop step, once the loop has entered one.
    pub loop_state: Option<LoopState>,
    /// One line saying more.
    pub detail: String,
    /// Tokens and cost so far.
    pub spend: SessionSpend,
}

/// Prices used to estimate cost; a turn in the review state is charged at
/// the reviewer's price, every other turn at the coder's.
#[derive(Debug, Clone, Copy, Default)]
pub struct Prices {
    /// The coder's price, when configured.
    pub coder: Option<Price>,
    /// The reviewer's price, when configured.
    pub reviewer: Option<Price>,
}

/// Follows one session from its first event to its end.
#[derive(Debug)]
pub struct Tracker {
    prices: Prices,
    loop_state: Option<LoopState>,
    spend: SessionSpend,
    /// Where the session rests when nothing waits: working, or its end.
    resting: (SessionState, String),
    /// One entry per prompt waiting for the owner, oldest first.
    waiting: Vec<String>,
}

impl Tracker {
    /// A tracker for a session that has just started working.
    ///
    /// # Arguments
    ///
    /// * `prices` - For the cost estimate.
    ///
    /// # Returns
    ///
    /// The tracker, in the `working` state.
    pub fn new(prices: Prices) -> Self {
        Self {
            prices,
            loop_state: None,
            spend: SessionSpend::default(),
            resting: (SessionState::Working, "starting".to_owned()),
            waiting: Vec::new(),
        }
    }

    /// What the file should say now.
    ///
    /// # Returns
    ///
    /// `needs_input` while any prompt waits, otherwise the resting state.
    pub fn snapshot(&self) -> Snapshot {
        let (state, detail) = match self.waiting.last() {
            Some(detail) => (SessionState::NeedsInput, detail.clone()),
            None => self.resting.clone(),
        };
        Snapshot {
            state,
            loop_state: self.loop_state,
            detail,
            spend: self.spend,
        }
    }

    /// Takes in one loop event.
    ///
    /// # Arguments
    ///
    /// * `event` - What the loop did.
    pub fn observe(&mut self, event: &LoopEvent) {
        match event {
            LoopEvent::StateEntered { state, .. } | LoopEvent::Transition { to: state, .. } => {
                self.loop_state = loop_state(*state).or(self.loop_state);
                self.set_working(format!("{state} step"));
            }
            LoopEvent::TurnFinished { state, usage, .. } => {
                if let Some(usage) = usage {
                    let price = match state {
                        State::Review => self.prices.reviewer,
                        _ => self.prices.coder,
                    };
                    self.add_spend(usage.input, usage.output, usage.cached, price);
                }
            }
            LoopEvent::Compacted { usage, .. } => {
                if let Some(usage) = usage {
                    self.add_spend(usage.input, usage.output, usage.cached, self.prices.coder);
                }
            }
            LoopEvent::ToolStarted { call } => self.set_working(format!("running {}", call.name)),
            LoopEvent::CheckStarted { command } => {
                self.set_working(format!("checking: {command}"));
            }
            LoopEvent::PlanWritten { .. } => self.set_working("plan written".to_owned()),
            // The run's end is reported by `finish_run`, after the memory step
            // that follows the loop; text, reviews and the rest change nothing.
            LoopEvent::TextDelta(_)
            | LoopEvent::ReasoningDelta(_)
            | LoopEvent::ToolFinished { .. }
            | LoopEvent::PlanUpdated { .. }
            | LoopEvent::ReviewVerdict { .. }
            | LoopEvent::CheckFinished { .. }
            | LoopEvent::Warning(_)
            | LoopEvent::ProposedEdit { .. }
            | LoopEvent::Finished { .. } => {}
        }
    }

    /// Notes that a prompt now waits for the owner.
    ///
    /// # Arguments
    ///
    /// * `what` - What is asked, such as `permission: shell cargo test`.
    pub fn prompt_waiting(&mut self, what: &str) {
        self.waiting.push(one_line(what));
    }

    /// Notes that the oldest waiting prompt was answered.
    pub fn prompt_answered(&mut self) {
        if !self.waiting.is_empty() {
            self.waiting.remove(0);
        }
    }

    /// Ends the session the way the loop ended.
    ///
    /// # Arguments
    ///
    /// * `status` - The loop's final status.
    pub fn finish_run(&mut self, status: &Status) {
        let (state, detail) = end_state(status);
        self.finish(state, &detail);
    }

    /// Ends the session in a given state, for `done` after a chat and
    /// `failed` after an error.
    ///
    /// # Arguments
    ///
    /// * `state` - The final state.
    /// * `detail` - Why, in one line.
    pub fn finish(&mut self, state: SessionState, detail: &str) {
        debug_assert!(!state.is_live(), "a session cannot end in {state:?}");
        self.resting = (state, one_line(detail));
    }

    /// Whether the session has ended.
    ///
    /// # Returns
    ///
    /// True once [`Tracker::finish`] or [`Tracker::finish_run`] was called.
    pub fn has_ended(&self) -> bool {
        !self.resting.0.is_live()
    }

    fn set_working(&mut self, detail: String) {
        if !self.has_ended() {
            self.resting = (SessionState::Working, one_line(&detail));
        }
    }

    fn add_spend(&mut self, input: u64, output: u64, cached: u64, price: Option<Price>) {
        self.spend.input_tokens = self.spend.input_tokens.saturating_add(input);
        self.spend.output_tokens = self.spend.output_tokens.saturating_add(output);
        self.spend.cached_tokens = self.spend.cached_tokens.saturating_add(cached);
        if let Some(price) = price {
            let cost = price.cost(input, output, cached);
            self.spend.cost_usd = Some(self.spend.cost_usd.unwrap_or(0.0) + cost);
        }
    }
}

/// Maps how a loop ended to the final state and its detail.
///
/// # Arguments
///
/// * `status` - The loop's final status.
///
/// # Returns
///
/// `done` for a finished task; `blocked` with the reason for everything
/// else the loop reports.
pub fn end_state(status: &Status) -> (SessionState, String) {
    match status {
        Status::Done => (SessionState::Done, "finished".to_owned()),
        Status::Incomplete { reason } => (SessionState::Blocked, reason_text(reason)),
        Status::Denied { reason } => (
            SessionState::Blocked,
            format!("a tool call was denied; {}", reason_text(reason)),
        ),
        Status::ProviderError { error } => (
            SessionState::Blocked,
            format!("the provider cannot be reached: {error}"),
        ),
    }
}

/// The reason a loop stopped, as one line.
fn reason_text(reason: &IncompleteReason) -> String {
    match reason {
        IncompleteReason::TurnBudget { .. }
        | IncompleteReason::TokenBudget { .. }
        | IncompleteReason::SessionBudget { .. } => format!("budget exhausted: {reason}"),
        IncompleteReason::ReviewRoundsExhausted
        | IncompleteReason::VerifyRoundsExhausted
        | IncompleteReason::PlanRejected
        | IncompleteReason::Unexpected { .. } => reason.to_string(),
    }
}

/// The status-file name of a loop state; `Done` is no step.
fn loop_state(state: State) -> Option<LoopState> {
    match state {
        State::Plan => Some(LoopState::Plan),
        State::Build => Some(LoopState::Build),
        State::Review => Some(LoopState::Review),
        State::Verify => Some(LoopState::Verify),
        State::Done => None,
    }
}

/// Collapses text to one line of bounded length.
pub fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= DETAIL_LIMIT {
        return joined;
    }
    let mut short: String = joined.chars().take(DETAIL_LIMIT - 1).collect();
    short.push('…');
    short
}

#[cfg(test)]
mod tests {
    use super::*;
    use anvil_core::provider::{ProviderError, StopReason, Usage};

    fn entered(state: State) -> LoopEvent {
        LoopEvent::StateEntered { state, visit: 1 }
    }

    #[test]
    fn a_permission_prompt_goes_working_then_needs_input_then_working() {
        let mut tracker = Tracker::new(Prices::default());
        tracker.observe(&entered(State::Build));
        assert_eq!(tracker.snapshot().state, SessionState::Working);
        tracker.prompt_waiting("permission: shell cargo test");
        let waiting = tracker.snapshot();
        assert_eq!(waiting.state, SessionState::NeedsInput);
        assert_eq!(waiting.detail, "permission: shell cargo test");
        assert_eq!(waiting.loop_state, Some(LoopState::Build));
        tracker.prompt_answered();
        assert_eq!(tracker.snapshot().state, SessionState::Working);
    }

    #[test]
    fn a_plan_waiting_for_approval_needs_input_until_answered() {
        let mut tracker = Tracker::new(Prices::default());
        tracker.observe(&entered(State::Plan));
        tracker.prompt_waiting("plan approval");
        // Events keep arriving while the owner reads the plan.
        tracker.observe(&LoopEvent::PlanWritten {
            path: "plan.md".into(),
        });
        assert_eq!(tracker.snapshot().state, SessionState::NeedsInput);
        assert_eq!(tracker.snapshot().loop_state, Some(LoopState::Plan));
        tracker.prompt_answered();
        assert_eq!(tracker.snapshot().state, SessionState::Working);
    }

    #[test]
    fn two_prompts_need_two_answers() {
        let mut tracker = Tracker::new(Prices::default());
        tracker.prompt_waiting("a");
        tracker.prompt_waiting("b");
        tracker.prompt_answered();
        assert_eq!(tracker.snapshot().state, SessionState::NeedsInput);
        tracker.prompt_answered();
        assert_eq!(tracker.snapshot().state, SessionState::Working);
        tracker.prompt_answered();
        assert_eq!(tracker.snapshot().state, SessionState::Working);
    }

    #[test]
    fn the_end_statuses_map_to_states() {
        let reason = |reason| Status::Incomplete { reason };
        let cases = [
            (Status::Done, SessionState::Done, "finished"),
            (
                reason(IncompleteReason::SessionBudget { used: 9, limit: 8 }),
                SessionState::Blocked,
                "budget exhausted: the session used its token budget (9 of 8 weighted tokens)",
            ),
            (
                reason(IncompleteReason::TurnBudget {
                    state: State::Build,
                }),
                SessionState::Blocked,
                "budget exhausted: the build state used its turn budget",
            ),
            (
                reason(IncompleteReason::PlanRejected),
                SessionState::Blocked,
                "the plan was rejected",
            ),
            (
                Status::Denied {
                    reason: IncompleteReason::ReviewRoundsExhausted,
                },
                SessionState::Blocked,
                "a tool call was denied; the reviewer requested changes 3 times; giving up",
            ),
        ];
        for (status, state, detail) in cases {
            assert_eq!(end_state(&status), (state, detail.to_owned()));
        }
        let (state, detail) = end_state(&Status::ProviderError {
            error: ProviderError::MissingApiKey {
                env: "KEY".to_owned(),
            },
        });
        assert_eq!(state, SessionState::Blocked);
        assert!(
            detail.starts_with("the provider cannot be reached"),
            "{detail}"
        );
    }

    #[test]
    fn a_normal_finish_is_done_and_stays_done() {
        let mut tracker = Tracker::new(Prices::default());
        tracker.observe(&entered(State::Verify));
        tracker.finish_run(&Status::Done);
        tracker.observe(&entered(State::Build));
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.state, SessionState::Done);
        assert_eq!(snapshot.loop_state, Some(LoopState::Build));
        assert!(tracker.has_ended());
    }

    #[test]
    fn a_prompt_after_the_loop_ended_returns_to_the_end_state() {
        let mut tracker = Tracker::new(Prices::default());
        tracker.finish_run(&Status::Done);
        tracker.prompt_waiting("memory: keep this fact?");
        assert_eq!(tracker.snapshot().state, SessionState::NeedsInput);
        tracker.prompt_answered();
        assert_eq!(tracker.snapshot().state, SessionState::Done);
    }

    #[test]
    fn a_failure_is_failed_with_its_detail_on_one_line() {
        let mut tracker = Tracker::new(Prices::default());
        tracker.finish(SessionState::Failed, "cannot start\nno such file");
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.state, SessionState::Failed);
        assert_eq!(snapshot.detail, "cannot start no such file");
    }

    #[test]
    fn turns_add_tokens_and_cost_at_the_state_price() {
        let price = |input| Price {
            input,
            output: 0.0,
            cached: None,
        };
        let mut tracker = Tracker::new(Prices {
            coder: Some(price(1.0)),
            reviewer: Some(price(10.0)),
        });
        let turn = |state| LoopEvent::TurnFinished {
            state,
            stop_reason: StopReason::EndTurn,
            usage: Some(Usage {
                input: 1_000_000,
                output: 5,
                cached: 0,
            }),
            latency_ms: 1,
            retries: 0,
        };
        tracker.observe(&turn(State::Build));
        tracker.observe(&turn(State::Review));
        let spend = tracker.snapshot().spend;
        assert_eq!(spend.input_tokens, 2_000_000);
        assert_eq!(spend.output_tokens, 10);
        assert_eq!(spend.cost_usd, Some(11.0));
    }

    #[test]
    fn cost_is_unknown_without_a_price() {
        let mut tracker = Tracker::new(Prices::default());
        tracker.observe(&LoopEvent::Compacted {
            dropped_messages: 1,
            summary_path: "s.md".into(),
            usage: Some(Usage {
                input: 3,
                output: 4,
                cached: 5,
            }),
        });
        let spend = tracker.snapshot().spend;
        assert_eq!(spend.total_tokens(), 12);
        assert_eq!(spend.cost_usd, None);
    }

    #[test]
    fn detail_is_bounded() {
        assert_eq!(one_line(&"x".repeat(500)).chars().count(), DETAIL_LIMIT);
    }
}
