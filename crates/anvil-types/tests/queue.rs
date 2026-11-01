//! Validates every ticket committed to `tickets/open/`.
//!
//! This is the data boundary enforced where the data lives: a ticket that
//! leaks a project path or too much code cannot be merged into the queue.

use std::path::{Path, PathBuf};

use anvil_types::{Ticket, TicketStatus};

/// Location of the open-ticket queue relative to this crate.
fn queue_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tickets/open")
}

#[test]
fn every_open_ticket_parses_validates_and_matches_its_file_name() {
    let dir = queue_dir();
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
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
        assert_eq!(
            ticket.status,
            TicketStatus::Open,
            "{}: tickets in open/ must have status open",
            path.display()
        );
    }
}
