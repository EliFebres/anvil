//! Fakes for every port, for this crate's tests and factory-cli's.
//!
//! Always compiled: they are small, pull in nothing, and the service's end
//! to end test in factory-cli needs them.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anvil_types::Ticket;

use crate::error::{FactoryError, Result};
use crate::notify::{Event, Notifier};
use crate::ports::{BoxFuture, CiStatus, CodeHost, Outcome, Progress, Worker};
use crate::tickets::TicketQueue;

/// Locks a mutex, recovering from a poisoned one.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Keeps every event it is given.
#[derive(Debug, Default)]
pub struct RecordingNotifier {
    events: Mutex<Vec<Event>>,
}

impl RecordingNotifier {
    /// The events so far, oldest first.
    pub fn events(&self) -> Vec<Event> {
        lock(&self.events).clone()
    }
}

impl Notifier for RecordingNotifier {
    fn send<'a>(&'a self, event: &'a Event) -> BoxFuture<'a, Result<()>> {
        lock(&self.events).push(event.clone());
        Box::pin(std::future::ready(Ok(())))
    }
}

/// A code host whose `main` CI status is scripted.
#[derive(Debug)]
pub struct FakeHost {
    ci: Mutex<CiStatus>,
}

impl FakeHost {
    /// A host whose `main` is green.
    pub fn green() -> Self {
        Self {
            ci: Mutex::new(CiStatus::Green),
        }
    }

    /// Changes what `main_ci` answers.
    ///
    /// # Arguments
    ///
    /// * `status` - The new answer.
    pub fn set_main_ci(&self, status: CiStatus) {
        *lock(&self.ci) = status;
    }
}

impl CodeHost for FakeHost {
    fn main_ci(&self) -> BoxFuture<'_, Result<CiStatus>> {
        let status = lock(&self.ci).clone();
        Box::pin(std::future::ready(Ok(status)))
    }
}

/// A worker that answers from a script and counts its calls.
///
/// Each call pops the next entry; when the script is empty it answers
/// `Merged { pr: 1 }`. An `Err` entry becomes a [`FactoryError::Sandbox`].
#[derive(Debug, Default)]
pub struct ScriptedWorker {
    script: Mutex<VecDeque<std::result::Result<Outcome, String>>>,
    calls: AtomicUsize,
    stages: Mutex<Vec<String>>,
}

impl ScriptedWorker {
    /// A worker that answers these outcomes in order.
    ///
    /// # Arguments
    ///
    /// * `script` - `Ok(outcome)` or `Err(message)` per call.
    pub fn scripted(script: Vec<std::result::Result<Outcome, String>>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            calls: AtomicUsize::new(0),
            stages: Mutex::new(Vec::new()),
        }
    }

    /// How many tickets were handed to this worker.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Every stage reported, oldest first.
    pub fn stages(&self) -> Vec<String> {
        lock(&self.stages).clone()
    }
}

impl Worker for ScriptedWorker {
    fn work<'a>(
        &'a self,
        _ticket: &'a Ticket,
        _queue: &'a TicketQueue,
        progress: Progress,
    ) -> BoxFuture<'a, Result<Outcome>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        progress("scripted");
        lock(&self.stages).push("scripted".to_owned());
        let next = lock(&self.script).pop_front();
        let result = match next {
            Some(Ok(outcome)) => Ok(outcome),
            Some(Err(message)) => Err(FactoryError::Sandbox(message)),
            None => Ok(Outcome::Merged {
                pr: 1,
                cost_usd: None,
            }),
        };
        Box::pin(std::future::ready(result))
    }
}
