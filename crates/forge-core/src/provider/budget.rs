//! The per-session token budget and the decorator that enforces it.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use futures_util::stream;

use super::error::ProviderError;
use super::types::{CompletionRequest, Event};
use super::{EventStream, Provider, lock_or_recover};

/// Tokens used against a limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBudget {
    /// Tokens consumed so far.
    used: u64,
    /// Tokens the session may consume.
    limit: u64,
}

/// A budget shared by every provider in a session.
pub type SharedBudget = Arc<Mutex<TokenBudget>>;

impl TokenBudget {
    /// Creates an unused budget.
    ///
    /// # Arguments
    ///
    /// * `limit` - Tokens the session may consume.
    ///
    /// # Returns
    ///
    /// The budget.
    pub fn new(limit: u64) -> Self {
        Self { used: 0, limit }
    }

    /// Creates an unused budget wrapped for sharing.
    ///
    /// # Arguments
    ///
    /// * `limit` - Tokens the session may consume.
    ///
    /// # Returns
    ///
    /// The shared budget.
    pub fn shared(limit: u64) -> SharedBudget {
        Arc::new(Mutex::new(Self::new(limit)))
    }

    /// Fails once the budget is spent.
    ///
    /// # Returns
    ///
    /// `Ok(())` while `used < limit`.
    ///
    /// # Errors
    ///
    /// [`ProviderError::BudgetExceeded`] otherwise.
    pub fn check(&self) -> Result<(), ProviderError> {
        if self.used >= self.limit {
            return Err(ProviderError::BudgetExceeded {
                used: self.used,
                limit: self.limit,
            });
        }
        Ok(())
    }

    /// Records consumed tokens.
    ///
    /// # Arguments
    ///
    /// * `tokens` - How many to add; saturates at `u64::MAX`.
    pub fn debit(&mut self, tokens: u64) {
        self.used = self.used.saturating_add(tokens);
    }

    /// Tokens consumed so far.
    ///
    /// # Returns
    ///
    /// The count.
    pub fn used(&self) -> u64 {
        self.used
    }

    /// The limit this budget was created with.
    ///
    /// # Returns
    ///
    /// The limit.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Tokens left before the budget is spent.
    ///
    /// # Returns
    ///
    /// `limit - used`, or zero once exceeded.
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.used)
    }
}

/// A provider that refuses requests once the shared budget is spent and
/// debits every [`Event::Usage`] it sees.
pub struct Budgeted<P> {
    /// The provider doing the work.
    inner: P,
    /// The session budget.
    budget: SharedBudget,
}

impl<P> Budgeted<P> {
    /// Wraps a provider.
    ///
    /// # Arguments
    ///
    /// * `inner` - The provider doing the work.
    /// * `budget` - The session budget to check and debit.
    ///
    /// # Returns
    ///
    /// The wrapper.
    pub fn new(inner: P, budget: SharedBudget) -> Self {
        Self { inner, budget }
    }

    /// The budget this wrapper enforces.
    ///
    /// # Returns
    ///
    /// A reference to the shared budget.
    pub fn budget(&self) -> &SharedBudget {
        &self.budget
    }
}

impl<P: Provider> Provider for Budgeted<P> {
    fn complete(&self, request: CompletionRequest) -> EventStream {
        if let Err(error) = lock_or_recover(&self.budget).check() {
            return Box::pin(stream::iter([Err(error)]));
        }
        let budget = Arc::clone(&self.budget);
        let events = self.inner.complete(request).map(move |item| {
            if let Ok(Event::Usage(usage)) = &item {
                lock_or_recover(&budget).debit(usage.total());
            }
            item
        });
        Box::pin(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::fake::FakeProvider;
    use crate::provider::types::{StopReason, Usage};

    fn usage_script(input: u64, output: u64, cached: u64) -> Vec<Result<Event, ProviderError>> {
        vec![
            Ok(Event::Usage(Usage {
                input,
                output,
                cached,
            })),
            Ok(Event::Done {
                stop_reason: StopReason::EndTurn,
            }),
        ]
    }

    async fn drain(mut events: EventStream) -> Vec<Result<Event, ProviderError>> {
        let mut out = Vec::new();
        while let Some(item) = events.next().await {
            out.push(item);
        }
        out
    }

    #[test]
    fn check_fails_when_used_reaches_limit() {
        let mut budget = TokenBudget::new(10);
        budget.debit(9);
        assert_eq!(budget.check(), Ok(()));
        assert_eq!(budget.remaining(), 1);
        budget.debit(1);
        assert_eq!(
            budget.check(),
            Err(ProviderError::BudgetExceeded {
                used: 10,
                limit: 10
            })
        );
        assert_eq!(budget.remaining(), 0);
        assert_eq!(budget.limit(), 10);
    }

    #[tokio::test]
    async fn budgeted_blocks_before_calling_inner() {
        let budget = TokenBudget::shared(0);
        let fake = FakeProvider::new(vec![usage_script(1, 1, 0)]);
        let provider = Budgeted::new(fake, budget);
        let items = drain(provider.complete(CompletionRequest::default())).await;
        assert_eq!(
            items,
            vec![Err(ProviderError::BudgetExceeded { used: 0, limit: 0 })]
        );
        assert_eq!(provider.inner.calls(), 0);
    }

    #[tokio::test]
    async fn budgeted_debits_usage_total() {
        let budget = TokenBudget::shared(100);
        let fake = FakeProvider::new(vec![usage_script(10, 5, 3)]);
        let provider = Budgeted::new(fake, Arc::clone(&budget));
        let items = drain(provider.complete(CompletionRequest::default())).await;
        assert_eq!(items.len(), 2);
        assert_eq!(lock_or_recover(provider.budget()).used(), 18);
    }

    #[tokio::test]
    async fn budgeted_shares_across_wrappers() {
        let budget = TokenBudget::shared(20);
        let first = Budgeted::new(
            FakeProvider::new(vec![usage_script(15, 0, 0)]),
            Arc::clone(&budget),
        );
        let second = Budgeted::new(
            FakeProvider::new(vec![usage_script(15, 0, 0)]),
            Arc::clone(&budget),
        );
        drain(first.complete(CompletionRequest::default())).await;
        drain(second.complete(CompletionRequest::default())).await;
        assert_eq!(lock_or_recover(&budget).used(), 30);
        let items = drain(second.complete(CompletionRequest::default())).await;
        assert_eq!(
            items,
            vec![Err(ProviderError::BudgetExceeded {
                used: 30,
                limit: 20
            })]
        );
    }

    #[test]
    fn poisoned_lock_recovers() {
        let budget = TokenBudget::shared(5);
        let poison = Arc::clone(&budget);
        let result = std::thread::spawn(move || {
            let _guard = poison.lock().expect("fresh lock");
            panic!("poison the mutex");
        })
        .join();
        assert!(result.is_err());
        assert!(budget.is_poisoned());
        assert_eq!(lock_or_recover(&budget).limit(), 5);
    }
}
