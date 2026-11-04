//! Validates every ticket committed under `tickets/`.
//!
//! This is the data boundary enforced where the data lives: a ticket that
//! leaks a project path or too much code cannot be merged into the queue.
//! Each state folder holds tickets of its status; on `main` only `open/`
//! exists, the others live on the `factory/state` branch.

use std::path::{Path, PathBuf};

use anvil_types::ticket::dirs;
use anvil_types::{Ticket, TicketStatus};

/// The repository root relative to this crate.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The statuses a ticket in `dir` may have.
fn allowed_statuses(dir: &str) -> &'static [TicketStatus] {
    match dir {
        dirs::OPEN_DIR => &[TicketStatus::Open],
        dirs::CLAIMED_DIR => &[TicketStatus::Claimed, TicketStatus::InReview],
        dirs::WAITING_DIR => &[TicketStatus::Waiting],
        dirs::MERGED_DIR => &[TicketStatus::Merged],
        dirs::REJECTED_DIR => &[TicketStatus::Rejected],
        other => panic!("unknown ticket folder {other}"),
    }
}

#[test]
fn every_committed_ticket_parses_validates_and_matches_its_folder() {
    for dir_name in dirs::ALL {
        let dir = repo_root().join(dir_name);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.expect("readable directory entry").path();
            if path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("readable ticket file");
            let ticket: Ticket =
                toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            ticket
                .validate()
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));

            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .expect("utf-8 file stem");
            assert_eq!(
                stem,
                ticket.id.to_string(),
                "{}: file name must equal the ticket id",
                path.display()
            );
            assert!(
                allowed_statuses(dir_name).contains(&ticket.status),
                "{}: status {:?} does not belong in {dir_name}",
                path.display(),
                ticket.status
            );
        }
    }
}

#[test]
fn every_state_folder_maps_back_to_a_status() {
    for dir in dirs::ALL {
        assert!(!allowed_statuses(dir).is_empty());
        for status in allowed_statuses(dir) {
            assert_eq!(dirs::dir_for(*status), Some(dir));
        }
    }
}
