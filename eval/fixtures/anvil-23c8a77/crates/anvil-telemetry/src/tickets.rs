//! The draft and open ticket folders as files.

use std::path::{Path, PathBuf};

use anvil_types::{Ticket, TicketKind, TicketStatus};

use crate::error::TelemetryError;
use crate::paths::{DRAFT_DIR, OPEN_DIR};
use crate::signal::normalized_title;

/// A ticket and the file it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTicket {
    /// The file.
    pub path: PathBuf,
    /// The ticket.
    pub ticket: Ticket,
}

/// Every ticket under `tickets/draft/` and `tickets/open/`.
#[derive(Debug, Clone)]
pub struct TicketStore {
    /// The repository root.
    root: PathBuf,
    /// Drafts, by file name.
    pub drafts: Vec<StoredTicket>,
    /// Open tickets, by file name.
    pub open: Vec<StoredTicket>,
}

impl TicketStore {
    /// Reads both folders.
    ///
    /// # Arguments
    ///
    /// * `root` - The repository root.
    ///
    /// # Returns
    ///
    /// The store. Missing folders count as empty.
    ///
    /// # Errors
    ///
    /// When a folder cannot be listed or a ticket file does not parse.
    pub fn load(root: &Path) -> Result<Self, TelemetryError> {
        Ok(Self {
            root: root.to_path_buf(),
            drafts: read_folder(&root.join(DRAFT_DIR))?,
            open: read_folder(&root.join(OPEN_DIR))?,
        })
    }

    /// The drafts folder.
    ///
    /// # Returns
    ///
    /// `<root>/tickets/draft`.
    pub fn draft_dir(&self) -> PathBuf {
        self.root.join(DRAFT_DIR)
    }

    /// The open folder.
    ///
    /// # Returns
    ///
    /// `<root>/tickets/open`.
    pub fn open_dir(&self) -> PathBuf {
        self.root.join(OPEN_DIR)
    }

    /// Writes a new draft.
    ///
    /// # Arguments
    ///
    /// * `ticket` - The draft; its id names the file.
    ///
    /// # Returns
    ///
    /// The file written.
    ///
    /// # Errors
    ///
    /// When the file cannot be written.
    pub fn add_draft(&mut self, ticket: Ticket) -> Result<PathBuf, TelemetryError> {
        let path = self.draft_dir().join(format!("{}.toml", ticket.id));
        write_ticket(&path, &ticket)?;
        self.drafts.push(StoredTicket {
            path: path.clone(),
            ticket,
        });
        Ok(path)
    }

    /// Rewrites a stored ticket in place.
    ///
    /// # Arguments
    ///
    /// * `stored` - The ticket and its file.
    ///
    /// # Errors
    ///
    /// When the file cannot be written.
    pub fn save(stored: &StoredTicket) -> Result<(), TelemetryError> {
        write_ticket(&stored.path, &stored.ticket)
    }

    /// The draft or open ticket with this kind and title, for dedup.
    ///
    /// # Arguments
    ///
    /// * `kind` - The ticket kind.
    /// * `normalized` - The title as [`normalized_title`] renders it.
    ///
    /// # Returns
    ///
    /// The first match, drafts before open tickets.
    pub fn find_duplicate(
        &mut self,
        kind: TicketKind,
        normalized: &str,
    ) -> Option<&mut StoredTicket> {
        self.drafts
            .iter_mut()
            .chain(self.open.iter_mut())
            .find(|stored| {
                stored.ticket.kind == kind && normalized_title(&stored.ticket.title) == normalized
            })
    }

    /// The draft whose id starts with `id`.
    ///
    /// # Arguments
    ///
    /// * `id` - A ticket id or a unique prefix of one.
    ///
    /// # Returns
    ///
    /// The draft.
    ///
    /// # Errors
    ///
    /// [`TelemetryError::UnknownTicket`] when nothing matches,
    /// [`TelemetryError::AmbiguousTicket`] when several drafts do.
    pub fn find_draft(&self, id: &str) -> Result<&StoredTicket, TelemetryError> {
        let upper = id.trim().to_ascii_uppercase();
        let mut matches = self
            .drafts
            .iter()
            .filter(|stored| stored.ticket.id.to_string().starts_with(&upper));
        let first = matches
            .next()
            .ok_or_else(|| TelemetryError::UnknownTicket { id: id.to_owned() })?;
        if matches.next().is_some() {
            return Err(TelemetryError::AmbiguousTicket { id: id.to_owned() });
        }
        Ok(first)
    }

    /// Deletes a draft.
    ///
    /// # Arguments
    ///
    /// * `id` - A ticket id or a unique prefix of one.
    ///
    /// # Returns
    ///
    /// The dropped ticket.
    ///
    /// # Errors
    ///
    /// When the draft is not found or the file cannot be removed.
    pub fn drop_draft(&mut self, id: &str) -> Result<Ticket, TelemetryError> {
        let path = self.find_draft(id)?.path.clone();
        std::fs::remove_file(&path).map_err(|e| TelemetryError::io(&path, &e))?;
        let index = self
            .drafts
            .iter()
            .position(|stored| stored.path == path)
            .ok_or_else(|| TelemetryError::UnknownTicket { id: id.to_owned() })?;
        Ok(self.drafts.remove(index).ticket)
    }

    /// Validates a draft and moves it to the open folder with `status = open`.
    ///
    /// # Arguments
    ///
    /// * `id` - A ticket id or a unique prefix of one.
    ///
    /// # Returns
    ///
    /// The ticket at its new path.
    ///
    /// # Errors
    ///
    /// When the draft is not found, fails the validator, or cannot be moved.
    pub fn submit(&mut self, id: &str) -> Result<StoredTicket, TelemetryError> {
        let draft = self.find_draft(id)?.clone();
        draft.ticket.validate()?;
        let mut ticket = draft.ticket;
        ticket.status = TicketStatus::Open;
        let path = self.open_dir().join(format!("{}.toml", ticket.id));
        write_ticket(&path, &ticket)?;
        std::fs::remove_file(&draft.path).map_err(|e| TelemetryError::io(&draft.path, &e))?;
        self.drafts.retain(|stored| stored.path != draft.path);
        let stored = StoredTicket { path, ticket };
        self.open.push(stored.clone());
        Ok(stored)
    }
}

/// Reads every `*.toml` in a folder, sorted by file name.
fn read_folder(dir: &Path) -> Result<Vec<StoredTicket>, TelemetryError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(TelemetryError::io(dir, &error)),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let ticket = load_ticket(&path)?;
            Ok(StoredTicket { path, ticket })
        })
        .collect()
}

/// Parses one ticket file.
///
/// # Arguments
///
/// * `path` - The file.
///
/// # Returns
///
/// The ticket.
///
/// # Errors
///
/// When the file cannot be read or is not a ticket.
pub fn load_ticket(path: &Path) -> Result<Ticket, TelemetryError> {
    let text = std::fs::read_to_string(path).map_err(|e| TelemetryError::io(path, &e))?;
    toml::from_str(&text).map_err(|error| TelemetryError::Parse {
        path: path.to_path_buf(),
        line: 0,
        detail: error.to_string(),
    })
}

/// Writes one ticket file, creating the folder as needed.
///
/// # Arguments
///
/// * `path` - The file.
/// * `ticket` - The ticket.
///
/// # Errors
///
/// When the folder or file cannot be written.
pub fn write_ticket(path: &Path, ticket: &Ticket) -> Result<(), TelemetryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| TelemetryError::io(parent, &e))?;
    }
    let text = toml::to_string_pretty(ticket).map_err(|error| TelemetryError::Parse {
        path: path.to_path_buf(),
        line: 0,
        detail: error.to_string(),
    })?;
    std::fs::write(path, text).map_err(|e| TelemetryError::io(path, &e))
}

#[cfg(test)]
mod tests {
    use anvil_types::{Priority, TicketOrigin};
    use chrono::Utc;
    use ulid::Ulid;

    use super::*;

    /// A valid draft with the given title.
    pub(crate) fn draft(title: &str) -> Ticket {
        Ticket {
            id: Ulid::generate(),
            created_at: Utc::now(),
            kind: TicketKind::Tool,
            title: title.to_owned(),
            symptom: "the model asked for a tool that does not exist".to_owned(),
            repro: None,
            spec: "add the tool".to_owned(),
            acceptance: vec!["the tool is registered".to_owned()],
            evidence: vec![],
            priority: Priority::P2,
            status: TicketStatus::Draft,
            origin: TicketOrigin::Cli,
            claimed_at: None,
            claimed_by: None,
            pr: None,
            note: None,
        }
    }

    #[test]
    fn add_find_submit_and_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = TicketStore::load(dir.path()).expect("empty store");
        assert!(store.drafts.is_empty() && store.open.is_empty());
        let first = draft("Missing tool `a`");
        let second = draft("Missing tool `b`");
        let first_id = first.id.to_string();
        let second_id = second.id.to_string();
        store.add_draft(first).expect("add");
        store.add_draft(second).expect("add");

        let reloaded = TicketStore::load(dir.path()).expect("store");
        assert_eq!(reloaded.drafts.len(), 2);
        assert_eq!(
            reloaded.find_draft(&first_id).expect("found").ticket.title,
            "Missing tool `a`"
        );
        assert!(matches!(
            reloaded.find_draft("nope"),
            Err(TelemetryError::UnknownTicket { .. })
        ));
        assert!(matches!(
            reloaded.find_draft(""),
            Err(TelemetryError::AmbiguousTicket { .. })
        ));

        let mut store = reloaded;
        let submitted = store.submit(&first_id).expect("submit");
        assert_eq!(submitted.ticket.status, TicketStatus::Open);
        assert_eq!(
            submitted.path,
            dir.path().join(OPEN_DIR).join(format!("{first_id}.toml"))
        );
        assert!(
            !dir.path()
                .join(DRAFT_DIR)
                .join(format!("{first_id}.toml"))
                .exists()
        );
        assert_eq!(store.open.len(), 1);
        assert_eq!(
            load_ticket(&submitted.path).expect("parses").status,
            TicketStatus::Open
        );

        let dropped = store.drop_draft(&second_id.to_lowercase()).expect("drop");
        assert_eq!(dropped.title, "Missing tool `b`");
        assert!(store.drafts.is_empty());
        assert!(
            TicketStore::load(dir.path())
                .expect("store")
                .drafts
                .is_empty()
        );
    }

    #[test]
    fn submit_runs_the_validator() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = TicketStore::load(dir.path()).expect("store");
        let mut bad = draft("Smuggled");
        bad.spec = "see /home/me/proj/src/main.rs".to_owned();
        let id = bad.id.to_string();
        store.add_draft(bad).expect("add");
        assert!(matches!(
            store.submit(&id),
            Err(TelemetryError::Validation(_))
        ));
        assert!(store.open.is_empty());
        assert!(!dir.path().join(OPEN_DIR).exists());
    }

    #[test]
    fn duplicates_match_on_kind_and_normalized_title() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = TicketStore::load(dir.path()).expect("store");
        let mut open = draft("Missing tool `browser`");
        open.status = TicketStatus::Open;
        write_ticket(
            &dir.path().join(OPEN_DIR).join(format!("{}.toml", open.id)),
            &open,
        )
        .expect("write");
        let mut store2 = TicketStore::load(dir.path()).expect("store");
        assert!(
            store2
                .find_duplicate(TicketKind::Tool, "missing tool browser")
                .is_some()
        );
        assert!(
            store2
                .find_duplicate(TicketKind::Skill, "missing tool browser")
                .is_none()
        );
        assert!(store.find_duplicate(TicketKind::Tool, "x").is_none());
    }
}
