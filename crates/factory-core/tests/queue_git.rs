//! The queue against real git: ticket branches, claims and moves on the
//! `factory/state` branch of a bare repository.

mod support;

use anvil_types::ticket::dirs::{self, STATE_BRANCH};
use anvil_types::{Priority, TicketKind, TicketStatus};
use factory_core::{FactoryError, Transition};
use support::{World, git, have_git, ticket, write};

#[tokio::test]
async fn ticket_branches_are_copied_and_anything_else_is_refused() {
    if !have_git() {
        eprintln!("git not available; skipping");
        return;
    }
    let world = World::new();
    let queue = world.queue().await;

    // Put the state branch ahead of main first, so a branch based on it
    // carries that history and a branch based on main does not.
    let early = ticket(Priority::P1, "2026-10-01T00:00:00Z", TicketKind::Bug);
    world.submit(&early, &format!("origin/{STATE_BRANCH}"));
    queue.refresh().await.expect("refresh");
    let ingested = queue.ingest_branches().await.expect("ingest");
    assert_eq!(ingested.accepted.len(), 1);
    queue.claim(&early, "nuc").await.expect("claim");
    assert!(
        world.state_log()[0].starts_with("Claim "),
        "{:?}",
        world.state_log()
    );

    let on_state = ticket(Priority::P1, "2026-10-02T00:00:00Z", TicketKind::Tool);
    let on_main = ticket(Priority::P2, "2026-10-03T00:00:00Z", TicketKind::Skill);
    let tampered = ticket(Priority::P0, "2026-10-04T00:00:00Z", TicketKind::Bug);
    world.submit(&on_state, &format!("origin/{STATE_BRANCH}"));
    world.submit(&on_main, "origin/main");
    let tampered_branch = world.submit_with_extra_file(&tampered);

    queue.refresh().await.expect("refresh");
    let ingested = queue.ingest_branches().await.expect("ingest");
    let accepted: Vec<_> = ingested.accepted.iter().map(|t| t.id).collect();
    assert!(accepted.contains(&on_state.id), "{accepted:?}");
    assert!(accepted.contains(&on_main.id), "{accepted:?}");
    assert_eq!(accepted.len(), 2);
    assert_eq!(ingested.refused.len(), 1);
    assert_eq!(ingested.refused[0].branch, tampered_branch);
    assert!(
        ingested.refused[0].reason.contains("README.md"),
        "{}",
        ingested.refused[0].reason
    );

    let files = world.state_files();
    assert!(files.contains(&format!("{}/{}.toml", dirs::OPEN_DIR, on_state.id)));
    assert!(files.contains(&format!("{}/{}.toml", dirs::OPEN_DIR, on_main.id)));
    assert!(files.contains(&format!("{}/{}.toml", dirs::CLAIMED_DIR, early.id)));
    assert!(!files.contains(&format!("{}/{}.toml", dirs::OPEN_DIR, tampered.id)));
    assert_eq!(
        world.remote_ticket_branches(),
        vec![tampered_branch.clone()],
        "accepted branches are deleted, the refused one stays"
    );
    let readme = git(
        &world.remote,
        &["show", &format!("{STATE_BRANCH}:README.md")],
    );
    assert_eq!(
        readme, "# repo",
        "the state branch never takes code changes"
    );

    // The refused branch is reported again, unchanged, on the next pass.
    let again = queue.ingest_branches().await.expect("ingest");
    assert_eq!(again.accepted.len(), 0);
    assert_eq!(again.refused.len(), 1);
    assert_eq!(again.refused[0].branch, tampered_branch);
}

#[tokio::test]
async fn pick_takes_p0_before_p1_and_the_oldest_within_a_priority() {
    if !have_git() {
        eprintln!("git not available; skipping");
        return;
    }
    let world = World::new();
    let queue = world.queue().await;
    let p1_old = ticket(Priority::P1, "2026-09-01T00:00:00Z", TicketKind::Bug);
    let p0_new = ticket(Priority::P0, "2026-10-20T00:00:00Z", TicketKind::Bug);
    let p0_old = ticket(Priority::P0, "2026-10-10T00:00:00Z", TicketKind::Bug);
    for t in [&p1_old, &p0_new, &p0_old] {
        world.submit(t, "origin/main");
    }
    queue.refresh().await.expect("refresh");
    queue.ingest_branches().await.expect("ingest");

    let open = queue.open_tickets().expect("lists");
    assert!(open.unusable.is_empty());
    let ids: Vec<_> = open.tickets.iter().map(|t| t.id).collect();
    assert_eq!(ids, vec![p0_old.id, p0_new.id, p1_old.id]);
    let picked = queue.pick().await.expect("pick").expect("a ticket");
    assert_eq!(picked.id, p0_old.id);
}

#[tokio::test]
async fn claim_and_transitions_move_the_file_and_merged_is_final() {
    if !have_git() {
        eprintln!("git not available; skipping");
        return;
    }
    let world = World::new();
    let queue = world.queue().await;
    let t = ticket(Priority::P1, "2026-10-01T00:00:00Z", TicketKind::Bug);
    world.submit(&t, "origin/main");
    queue.refresh().await.expect("refresh");
    queue.ingest_branches().await.expect("ingest");

    let claimed = queue.claim(&t, "nuc").await.expect("claim");
    assert_eq!(claimed.status, TicketStatus::Claimed);
    assert_eq!(claimed.claimed_by.as_deref(), Some("nuc"));
    assert!(claimed.claimed_at.is_some());
    let stored = world.state_ticket(dirs::CLAIMED_DIR, t.id);
    assert_eq!(stored, claimed);
    assert!(
        !queue
            .dir()
            .join(dirs::OPEN_DIR)
            .join(format!("{}.toml", t.id))
            .exists()
    );

    let reviewed = queue
        .transition(t.id, Transition::InReview { pr: 5 })
        .await
        .expect("in review");
    assert_eq!(reviewed.status, TicketStatus::InReview);
    assert_eq!(reviewed.pr, Some(5));
    assert!(
        world
            .state_files()
            .contains(&format!("{}/{}.toml", dirs::CLAIMED_DIR, t.id))
    );

    let merged = queue
        .transition(t.id, Transition::Merged { pr: 5 })
        .await
        .expect("merged");
    assert_eq!(merged.status, TicketStatus::Merged);
    let files = world.state_files();
    assert!(files.contains(&format!("{}/{}.toml", dirs::MERGED_DIR, t.id)));
    assert!(!files.contains(&format!("{}/{}.toml", dirs::CLAIMED_DIR, t.id)));
    assert!(
        world.state_log()[0].ends_with("(#5)"),
        "{:?}",
        world.state_log()
    );

    let error = queue
        .transition(t.id, Transition::Reopen)
        .await
        .expect_err("merged tickets are never edited");
    assert!(matches!(error, FactoryError::Immutable { .. }), "{error}");

    // A second ticket: rejected with notes, then a third reopened.
    let r = ticket(Priority::P1, "2026-10-02T00:00:00Z", TicketKind::Bug);
    world.submit(&r, "origin/main");
    queue.refresh().await.expect("refresh");
    queue.ingest_branches().await.expect("ingest");
    queue.claim(&r, "nuc").await.expect("claim");
    let rejected = queue
        .transition(
            r.id,
            Transition::Rejected {
                note: "gate A failed twice".to_owned(),
                notes_md: "# notes\n\nfull output\n".to_owned(),
            },
        )
        .await
        .expect("rejected");
    assert_eq!(rejected.status, TicketStatus::Rejected);
    assert_eq!(rejected.note.as_deref(), Some("gate A failed twice"));
    let files = world.state_files();
    assert!(files.contains(&format!("{}/{}.toml", dirs::REJECTED_DIR, r.id)));
    assert!(files.contains(&format!("{}/{}.notes.md", dirs::REJECTED_DIR, r.id)));

    let o = ticket(Priority::P1, "2026-10-03T00:00:00Z", TicketKind::Bug);
    world.submit(&o, "origin/main");
    queue.refresh().await.expect("refresh");
    queue.ingest_branches().await.expect("ingest");
    queue.claim(&o, "nuc").await.expect("claim");
    let reopened = queue
        .transition(o.id, Transition::Reopen)
        .await
        .expect("reopened");
    assert_eq!(reopened.status, TicketStatus::Open);
    assert_eq!(reopened.claimed_at, None);
    assert_eq!(reopened.claimed_by, None);
    assert!(
        world
            .state_files()
            .contains(&format!("{}/{}.toml", dirs::OPEN_DIR, o.id))
    );
}

#[tokio::test]
async fn an_unusable_file_in_open_is_moved_to_rejected_with_notes() {
    if !have_git() {
        eprintln!("git not available; skipping");
        return;
    }
    let world = World::new();
    let queue = world.queue().await;
    let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    write(
        &queue.dir().join(dirs::OPEN_DIR).join(format!("{id}.toml")),
        "this is = not [a ticket\n",
    );
    git(queue.dir(), &["add", "-A"]);
    git(queue.dir(), &["commit", "-q", "-m", "garbage"]);
    git(queue.dir(), &["push", "-q", "origin", STATE_BRANCH]);

    assert!(queue.pick().await.expect("pick").is_none());
    let files = world.state_files();
    assert!(
        files.contains(&format!("{}/{id}.toml", dirs::REJECTED_DIR)),
        "{files:?}"
    );
    assert!(files.contains(&format!("{}/{id}.notes.md", dirs::REJECTED_DIR)));
    assert!(!files.contains(&format!("{}/{id}.toml", dirs::OPEN_DIR)));

    // A duplicate id on a new branch is refused.
    let dup = ticket(Priority::P1, "2026-10-01T00:00:00Z", TicketKind::Bug);
    world.submit(&dup, "origin/main");
    queue.refresh().await.expect("refresh");
    queue.ingest_branches().await.expect("ingest");
    let mut same = dup.clone();
    same.title = "same id again".to_owned();
    world.submit(&same, "origin/main");
    queue.refresh().await.expect("refresh");
    let ingested = queue.ingest_branches().await.expect("ingest");
    assert_eq!(ingested.accepted.len(), 0);
    assert!(
        ingested.refused[0].reason.contains("already in the queue"),
        "{}",
        ingested.refused[0].reason
    );
}
