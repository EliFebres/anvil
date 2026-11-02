//! The ticket queue: the `tickets/` folders on a worktree of the
//! `factory/state` branch.
//!
//! Tickets arrive on `tickets/<id>` branches and are copied into
//! `tickets/open/`. Claiming moves a ticket to `claimed/`; the worker's
//! outcome moves it on to `merged/`, `waiting/` or `rejected/`. Every move is
//! one commit pushed to the state branch, which the factory alone writes.

use std::path::{Path, PathBuf};

use anvil_types::ticket::dirs::{self, STATE_BRANCH};
use anvil_types::{Ticket, TicketStatus};
use chrono::Utc;
use ulid::Ulid;

use crate::error::{FactoryError, Result};
use crate::git::Git;

/// The branch prefix `anvil ticket submit` pushes to.
pub const TICKET_BRANCH_PREFIX: &str = "tickets/";

/// The note a `kind = model` ticket waits with.
pub const MODEL_NOTE: &str =
    "model tickets are handled by `anvil serve`, which does not exist yet; waiting for it";

/// A ticket branch the factory did not take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The branch name, without the remote.
    pub branch: String,
    /// Its head commit.
    pub sha: String,
    /// Why.
    pub reason: String,
}

/// What one pass over the ticket branches did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Ingested {
    /// Tickets copied into `open/`, in branch order.
    pub accepted: Vec<Ticket>,
    /// Branches left alone, with the reason.
    pub refused: Vec<Refused>,
}

/// What `open/` holds.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OpenTickets {
    /// Valid tickets, highest priority and oldest first.
    pub tickets: Vec<Ticket>,
    /// Files that are not valid tickets, with the problem.
    pub unusable: Vec<(PathBuf, String)>,
}

/// Where a claimed ticket goes next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// Parked for a human.
    Waiting {
        /// Why.
        note: String,
        /// The pull request, when one exists.
        pr: Option<u64>,
    },
    /// A pull request is open.
    InReview {
        /// The pull request number.
        pr: u64,
    },
    /// The pull request merged.
    Merged {
        /// The pull request number.
        pr: u64,
    },
    /// The factory gave up.
    Rejected {
        /// Why, in one line.
        note: String,
        /// The full gate output, written to `<id>.notes.md`.
        notes_md: String,
    },
    /// Back to `open/`, as if never claimed.
    Reopen,
}

/// The queue on the state worktree.
#[derive(Debug)]
pub struct TicketQueue {
    /// The state worktree.
    git: Git,
    /// The remote the state branch is pushed to.
    remote: String,
}

impl TicketQueue {
    /// Opens the queue, creating the state worktree when it does not exist.
    ///
    /// # Arguments
    ///
    /// * `repo` - The factory's clone of the monorepo.
    /// * `state_dir` - Where the state worktree lives, `~/.factory/state`.
    /// * `remote` - The remote, normally `origin`.
    ///
    /// # Returns
    ///
    /// The queue.
    ///
    /// # Errors
    ///
    /// [`FactoryError::Config`] when the remote has no `factory/state`
    /// branch, and git errors.
    pub async fn open(repo: &Git, state_dir: &Path, remote: &str) -> Result<Self> {
        if !state_dir.join(".git").exists() {
            repo.fetch_prune(remote).await?;
            let upstream = format!("{remote}/{STATE_BRANCH}");
            if repo.try_rev_parse(&upstream).await.is_none() {
                return Err(FactoryError::Config(format!(
                    "{remote} has no `{STATE_BRANCH}` branch; run `anvil doctor --fix-github` to create it"
                )));
            }
            repo.worktree_add_tracking(state_dir, STATE_BRANCH, remote)
                .await?;
        }
        Ok(Self {
            git: repo.at(state_dir),
            remote: remote.to_owned(),
        })
    }

    /// The state worktree folder.
    pub fn dir(&self) -> &Path {
        self.git.dir()
    }

    /// The git handle on the state worktree.
    pub fn git(&self) -> &Git {
        &self.git
    }

    /// Fetches and fast-forwards the state branch.
    ///
    /// The factory is the only writer, so anything but a fast-forward is an
    /// error; nothing is ever reset.
    pub async fn refresh(&self) -> Result<()> {
        self.git.fetch_prune(&self.remote).await?;
        self.git.pull_ff_only(&self.remote, STATE_BRANCH).await
    }

    /// Copies the tickets on every `tickets/*` branch into `open/`.
    ///
    /// A branch is taken only when every file it adds, compared with its
    /// merge base against the state branch, is a valid open ticket at
    /// `tickets/open/<id>.toml` with an id that is new to the queue. Taken
    /// branches are committed, pushed and deleted on the remote; refused
    /// ones are left alone and reported.
    ///
    /// # Returns
    ///
    /// What was taken and what was refused.
    pub async fn ingest_branches(&self) -> Result<Ingested> {
        let mut ingested = Ingested::default();
        for branch in self
            .git
            .remote_branches(&self.remote, TICKET_BRANCH_PREFIX)
            .await?
        {
            let remote_ref = format!("{}/{branch}", self.remote);
            let sha = self.git.rev_parse(&remote_ref).await?;
            match self.inspect_branch(&sha).await? {
                Ok(tickets) => {
                    let paths: Vec<String> = tickets
                        .iter()
                        .map(|ticket| format!("{}/{}.toml", dirs::OPEN_DIR, ticket.id))
                        .collect();
                    self.git.checkout_paths(&sha, &paths).await?;
                    let message = match tickets.as_slice() {
                        [one] => format!("Add ticket {}: {}", one.id, one.title),
                        many => format!("Add {} tickets from {branch}", many.len()),
                    };
                    self.git.commit_paths(&paths, &message).await?;
                    self.git.push(&self.remote, STATE_BRANCH).await?;
                    self.git.delete_remote_branch(&self.remote, &branch).await?;
                    tracing::info!(branch, count = tickets.len(), "ingested ticket branch");
                    ingested.accepted.extend(tickets);
                }
                Err(reason) => {
                    tracing::warn!(branch, %sha, reason, "refused ticket branch");
                    ingested.refused.push(Refused {
                        branch,
                        sha,
                        reason,
                    });
                }
            }
        }
        Ok(ingested)
    }

    /// The valid tickets a branch adds, or why the branch is refused.
    async fn inspect_branch(&self, sha: &str) -> Result<std::result::Result<Vec<Ticket>, String>> {
        let base = self.git.merge_base(sha, "HEAD").await?;
        let changed = self.git.diff_names(&base, sha).await?;
        if changed.is_empty() {
            return Ok(Err("the branch changes nothing".to_owned()));
        }
        let mut tickets = Vec::new();
        for path in &changed {
            let Some(id) = open_ticket_id(path) else {
                return Ok(Err(format!(
                    "touches `{path}`, which is not a new open ticket"
                )));
            };
            let text = self.git.show(sha, path).await?;
            let ticket = match parse_ticket(Path::new(path), &text) {
                Ok(ticket) => ticket,
                Err(error) => return Ok(Err(error.to_string())),
            };
            if ticket.id != id {
                return Ok(Err(format!("`{path}` holds ticket {}", ticket.id)));
            }
            if ticket.status != TicketStatus::Open {
                return Ok(Err(format!(
                    "ticket {id} has status {:?}, not open",
                    ticket.status
                )));
            }
            if self.find(id).is_ok() {
                return Ok(Err(format!("ticket {id} is already in the queue")));
            }
            tickets.push(ticket);
        }
        Ok(Ok(tickets))
    }

    /// Every ticket in `open/`, highest priority and oldest first, plus the
    /// files that are not valid tickets.
    ///
    /// # Returns
    ///
    /// The ordered tickets and `(path, problem)` for each unusable file.
    pub fn open_tickets(&self) -> Result<OpenTickets> {
        let dir = self.git.dir().join(dirs::OPEN_DIR);
        let mut tickets = Vec::new();
        let mut invalid = Vec::new();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(OpenTickets::default());
        };
        for entry in entries {
            let path = entry
                .map_err(|source| FactoryError::io("reading", &dir, source))?
                .path();
            if path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .map_err(|source| FactoryError::io("reading", &path, source))?;
            match parse_ticket(&path, &text) {
                Ok(ticket)
                    if path.file_stem().and_then(|s| s.to_str())
                        == Some(&ticket.id.to_string()) =>
                {
                    tickets.push(ticket);
                }
                Ok(ticket) => invalid.push((
                    path,
                    format!("file name does not match ticket id {}", ticket.id),
                )),
                Err(error) => invalid.push((path, error.to_string())),
            }
        }
        order(&mut tickets);
        Ok(OpenTickets {
            tickets,
            unusable: invalid,
        })
    }

    /// The next ticket to work, after moving unusable files to `rejected/`.
    ///
    /// # Returns
    ///
    /// The highest-priority oldest open ticket, if any.
    pub async fn pick(&self) -> Result<Option<Ticket>> {
        let open = self.open_tickets()?;
        for (path, problem) in open.unusable {
            self.reject_file(&path, &problem).await?;
        }
        Ok(open.tickets.into_iter().next())
    }

    /// Moves a file that is not a usable ticket to `rejected/` with a note.
    async fn reject_file(&self, path: &Path, problem: &str) -> Result<()> {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            return Ok(());
        };
        tracing::warn!(path = %path.display(), problem, "rejecting an unusable ticket file");
        let rejected_dir = self.git.dir().join(dirs::REJECTED_DIR);
        std::fs::create_dir_all(&rejected_dir)
            .map_err(|source| FactoryError::io("creating", &rejected_dir, source))?;
        let target = rejected_dir.join(format!("{stem}.toml"));
        let notes = rejected_dir.join(format!("{stem}.notes.md"));
        std::fs::rename(path, &target)
            .map_err(|source| FactoryError::io("moving", path, source))?;
        std::fs::write(
            &notes,
            format!("# {stem}\n\nNot a usable ticket: {problem}\n"),
        )
        .map_err(|source| FactoryError::io("writing", &notes, source))?;
        let paths = vec![
            self.relative(path),
            self.relative(&target),
            self.relative(&notes),
        ];
        self.git
            .commit_paths(&paths, &format!("Reject {stem}: not a usable ticket"))
            .await?;
        self.git.push(&self.remote, STATE_BRANCH).await
    }

    /// Finds a ticket in any state folder.
    ///
    /// # Arguments
    ///
    /// * `id` - The ticket id.
    ///
    /// # Returns
    ///
    /// The file and the ticket.
    ///
    /// # Errors
    ///
    /// [`FactoryError::UnknownTicket`] when no folder holds it.
    pub fn find(&self, id: Ulid) -> Result<(PathBuf, Ticket)> {
        for dir in dirs::ALL {
            let path = self.git.dir().join(dir).join(format!("{id}.toml"));
            if path.is_file() {
                let text = std::fs::read_to_string(&path)
                    .map_err(|source| FactoryError::io("reading", &path, source))?;
                let ticket = parse_ticket(&path, &text)?;
                return Ok((path, ticket));
            }
        }
        Err(FactoryError::UnknownTicket { id: id.to_string() })
    }

    /// Claims an open ticket: moves it to `claimed/` with the time and the
    /// worker's hostname, commits and pushes.
    ///
    /// # Arguments
    ///
    /// * `ticket` - An open ticket.
    /// * `hostname` - This worker.
    ///
    /// # Returns
    ///
    /// The ticket as written.
    pub async fn claim(&self, ticket: &Ticket, hostname: &str) -> Result<Ticket> {
        let (from, mut claimed) = self.find(ticket.id)?;
        if claimed.status != TicketStatus::Open {
            return Err(FactoryError::Ticket {
                path: from,
                detail: format!("cannot claim a ticket with status {:?}", claimed.status),
            });
        }
        claimed.status = TicketStatus::Claimed;
        claimed.claimed_at = Some(Utc::now());
        claimed.claimed_by = Some(hostname.to_owned());
        let message = format!("Claim {}: {}", claimed.id, claimed.title);
        self.move_ticket(&from, &claimed, None, &message).await?;
        Ok(claimed)
    }

    /// Moves a claimed ticket to its next state, commits and pushes.
    ///
    /// # Arguments
    ///
    /// * `id` - The ticket.
    /// * `to` - Where it goes.
    ///
    /// # Returns
    ///
    /// The ticket as written.
    ///
    /// # Errors
    ///
    /// [`FactoryError::Immutable`] for a ticket in `merged/`.
    pub async fn transition(&self, id: Ulid, to: Transition) -> Result<Ticket> {
        let (from, mut ticket) = self.find(id)?;
        if ticket.status == TicketStatus::Merged {
            return Err(FactoryError::Immutable { id: id.to_string() });
        }
        let mut notes = None;
        let verb = match to {
            Transition::Waiting { note, pr } => {
                ticket.status = TicketStatus::Waiting;
                ticket.note = Some(note);
                ticket.pr = pr.or(ticket.pr);
                "Wait"
            }
            Transition::InReview { pr } => {
                ticket.status = TicketStatus::InReview;
                ticket.pr = Some(pr);
                "Review"
            }
            Transition::Merged { pr } => {
                ticket.status = TicketStatus::Merged;
                ticket.pr = Some(pr);
                "Merge"
            }
            Transition::Rejected { note, notes_md } => {
                ticket.status = TicketStatus::Rejected;
                ticket.note = Some(note);
                notes = Some(notes_md);
                "Reject"
            }
            Transition::Reopen => {
                ticket.status = TicketStatus::Open;
                ticket.claimed_at = None;
                ticket.claimed_by = None;
                ticket.pr = None;
                ticket.note = None;
                "Reopen"
            }
        };
        let pr = ticket.pr.map(|pr| format!(" (#{pr})")).unwrap_or_default();
        let message = format!("{verb} {}: {}{pr}", ticket.id, ticket.title);
        self.move_ticket(&from, &ticket, notes.as_deref(), &message)
            .await?;
        Ok(ticket)
    }

    /// Writes the ticket into the folder of its status, removes the old
    /// file, commits both and pushes.
    async fn move_ticket(
        &self,
        from: &Path,
        ticket: &Ticket,
        notes: Option<&str>,
        message: &str,
    ) -> Result<()> {
        ticket.validate().map_err(|error| FactoryError::Ticket {
            path: from.to_path_buf(),
            detail: error.to_string(),
        })?;
        let dir_name = dirs::dir_for(ticket.status).ok_or_else(|| FactoryError::Ticket {
            path: from.to_path_buf(),
            detail: "drafts are never committed".to_owned(),
        })?;
        let dir = self.git.dir().join(dir_name);
        std::fs::create_dir_all(&dir)
            .map_err(|source| FactoryError::io("creating", &dir, source))?;
        let to = dir.join(format!("{}.toml", ticket.id));
        let text = toml::to_string_pretty(ticket).map_err(|error| FactoryError::Ticket {
            path: to.clone(),
            detail: error.to_string(),
        })?;
        let mut paths = vec![self.relative(from), self.relative(&to)];
        if from != to {
            std::fs::remove_file(from)
                .map_err(|source| FactoryError::io("removing", from, source))?;
        }
        std::fs::write(&to, text).map_err(|source| FactoryError::io("writing", &to, source))?;
        if let Some(notes) = notes {
            let notes_path = dir.join(format!("{}.notes.md", ticket.id));
            std::fs::write(&notes_path, notes)
                .map_err(|source| FactoryError::io("writing", &notes_path, source))?;
            paths.push(self.relative(&notes_path));
        }
        paths.dedup();
        self.git.commit_paths(&paths, message).await?;
        self.git.push(&self.remote, STATE_BRANCH).await
    }

    /// A path inside the state worktree as git wants it.
    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(self.git.dir())
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

/// Parses and validates one ticket file.
fn parse_ticket(path: &Path, text: &str) -> Result<Ticket> {
    let ticket: Ticket = toml::from_str(text).map_err(|error| FactoryError::Ticket {
        path: path.to_path_buf(),
        detail: error.message().to_owned(),
    })?;
    ticket.validate().map_err(|error| FactoryError::Ticket {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    Ok(ticket)
}

/// The ticket id a path names when it is `tickets/open/<ULID>.toml`.
///
/// # Arguments
///
/// * `path` - A repository-relative path with `/` separators.
///
/// # Returns
///
/// The id, or `None` for any other path.
pub fn open_ticket_id(path: &str) -> Option<Ulid> {
    let stem = path
        .strip_prefix(dirs::OPEN_DIR)?
        .strip_prefix('/')?
        .strip_suffix(".toml")?;
    if stem.contains('/') {
        return None;
    }
    Ulid::from_string(stem).ok()
}

/// Sorts tickets the way the factory picks them: highest priority first,
/// then oldest first, then by id.
///
/// # Arguments
///
/// * `tickets` - The tickets to order in place.
pub fn order(tickets: &mut [Ticket]) {
    tickets.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then(a.created_at.cmp(&b.created_at))
            .then(a.id.cmp(&b.id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use anvil_types::{Priority, TicketKind, TicketOrigin};

    fn ticket(id: &str, priority: Priority, created: &str) -> Ticket {
        Ticket {
            id: Ulid::from_string(id).expect("ulid"),
            created_at: created.parse().expect("rfc3339"),
            kind: TicketKind::Bug,
            title: format!("ticket {id}"),
            symptom: "broken".to_owned(),
            repro: None,
            spec: "fix it".to_owned(),
            acceptance: vec!["works".to_owned()],
            evidence: Vec::new(),
            priority,
            status: TicketStatus::Open,
            origin: TicketOrigin::Human,
            claimed_at: None,
            claimed_by: None,
            pr: None,
            note: None,
        }
    }

    #[test]
    fn p0_before_p1_and_oldest_first_within_a_priority() {
        let mut tickets = vec![
            ticket(
                "01ARZ3NDEKTSV4RRFFQ69G5FA3",
                Priority::P1,
                "2026-10-01T00:00:00Z",
            ),
            ticket(
                "01ARZ3NDEKTSV4RRFFQ69G5FA2",
                Priority::P0,
                "2026-10-05T00:00:00Z",
            ),
            ticket(
                "01ARZ3NDEKTSV4RRFFQ69G5FA1",
                Priority::P2,
                "2026-09-01T00:00:00Z",
            ),
            ticket(
                "01ARZ3NDEKTSV4RRFFQ69G5FA4",
                Priority::P0,
                "2026-10-02T00:00:00Z",
            ),
            ticket(
                "01ARZ3NDEKTSV4RRFFQ69G5FA5",
                Priority::P1,
                "2026-09-30T00:00:00Z",
            ),
        ];
        order(&mut tickets);
        let ids: Vec<String> = tickets.iter().map(|t| t.id.to_string()).collect();
        assert_eq!(
            ids,
            [
                "01ARZ3NDEKTSV4RRFFQ69G5FA4", // P0, oldest
                "01ARZ3NDEKTSV4RRFFQ69G5FA2", // P0
                "01ARZ3NDEKTSV4RRFFQ69G5FA5", // P1, oldest
                "01ARZ3NDEKTSV4RRFFQ69G5FA3", // P1
                "01ARZ3NDEKTSV4RRFFQ69G5FA1", // P2, oldest of all but lowest priority
            ]
        );
    }

    #[test]
    fn same_priority_and_time_falls_back_to_the_id() {
        let mut tickets = vec![
            ticket(
                "01ARZ3NDEKTSV4RRFFQ69G5FB2",
                Priority::P1,
                "2026-10-01T00:00:00Z",
            ),
            ticket(
                "01ARZ3NDEKTSV4RRFFQ69G5FB1",
                Priority::P1,
                "2026-10-01T00:00:00Z",
            ),
        ];
        order(&mut tickets);
        assert!(tickets[0].id < tickets[1].id);
    }

    #[test]
    fn only_open_ticket_files_are_ticket_paths() {
        let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        assert_eq!(
            open_ticket_id(&format!("tickets/open/{id}.toml")).map(|u| u.to_string()),
            Some(id.to_owned())
        );
        for other in [
            "tickets/claimed/01ARZ3NDEKTSV4RRFFQ69G5FAV.toml",
            "tickets/open/notes.toml",
            "tickets/open/sub/01ARZ3NDEKTSV4RRFFQ69G5FAV.toml",
            "tickets/open/01ARZ3NDEKTSV4RRFFQ69G5FAV.md",
            "README.md",
            "crates/x/Cargo.toml",
        ] {
            assert_eq!(open_ticket_id(other), None, "{other}");
        }
    }
}
