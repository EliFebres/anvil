//! `factory reopen`: put rejected (or stuck claimed) tickets back in `open/`.
//!
//! The move goes through the same `Transition::Reopen` the service uses, so
//! the ticket loses its claim, pull request and note, its `<id>.notes.md`
//! goes with it, and the change is one commit pushed to `factory/state`.

use std::path::Path;

use anvil_types::Ticket;
use factory_core::{FactoryHome, ServiceState, TicketQueue, Transition};
use ulid::Ulid;

use crate::wiring;

/// Which tickets to reopen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// One ticket, in any folder but `open/` and `merged/`.
    One(Ulid),
    /// Every ticket in `rejected/`.
    AllRejected,
}

/// Reopens the target tickets on `factory/state` and pushes.
///
/// # Arguments
///
/// * `root` - The clone of the monorepo; its token reaches the remote.
/// * `home` - The factory home, for the state worktree and `state.json`.
/// * `target` - Which tickets.
/// * `service_active` - Whether the factory service is running now.
///
/// # Returns
///
/// The reopened tickets, in the order they were moved.
///
/// # Errors
///
/// When the state branch cannot be refreshed or pushed, a ticket is
/// unknown, merged or already open, or the ticket is the one a running
/// service is working.
pub async fn reopen(
    root: &Path,
    home: &FactoryHome,
    target: Target,
    service_active: bool,
) -> anyhow::Result<Vec<Ticket>> {
    let queue = TicketQueue::open(&wiring::repo(root), &home.state_dir(), wiring::REMOTE).await?;
    queue.refresh().await?;
    let ids = match target {
        Target::One(id) => vec![id],
        Target::AllRejected => queue.rejected_ids()?,
    };
    let mut state = ServiceState::load(&home.state_file())?;
    let mut reopened = Vec::with_capacity(ids.len());
    for id in ids {
        let (_, ticket) = queue.find(id)?;
        if ticket.status == anvil_types::TicketStatus::Open {
            anyhow::bail!("{id} is already open");
        }
        let in_progress = state
            .in_progress
            .as_ref()
            .is_some_and(|current| current.id == id.to_string());
        if in_progress {
            if service_active {
                anyhow::bail!(
                    "{id} is the ticket the service is working; stop it first (systemctl --user stop factory)"
                );
            }
            // A stopped service left its claim behind; the ticket is no longer in progress.
            state.in_progress = None;
            state.save(&home.state_file())?;
        }
        reopened.push(queue.transition(id, Transition::Reopen).await?);
    }
    Ok(reopened)
}
