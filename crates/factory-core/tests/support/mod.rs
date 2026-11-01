//! A small world for queue and service tests: a bare repository standing in
//! for GitHub with `main` and `factory/state`, a laptop clone that submits
//! tickets on `tickets/<id>` branches, and the factory's own clone.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use anvil_types::ticket::dirs::{OPEN_DIR, STATE_BRANCH};
use anvil_types::{Priority, Ticket, TicketKind, TicketOrigin, TicketStatus};
use factory_core::{FactoryHome, Git, TicketQueue};
use ulid::Ulid;

/// The three repositories.
pub struct World {
    /// Keeps the temp folder alive.
    pub dir: tempfile::TempDir,
    /// The bare "GitHub" repository.
    pub remote: PathBuf,
    /// A developer's clone.
    pub laptop: PathBuf,
    /// The factory's clone, `main` checked out.
    pub factory: PathBuf,
    /// `~/.factory` of the factory machine.
    pub home: FactoryHome,
}

/// Runs git for test setup and panics with its stderr when it fails.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {} in {}: {}",
        args.join(" "),
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Writes a file, creating its folders.
pub fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

impl World {
    /// Builds the world: `main` holds a README and `tickets/open/.gitkeep`,
    /// `factory/state` starts equal to `main`.
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let remote = dir.path().join("github.git");
        let laptop = dir.path().join("laptop");
        let factory = dir.path().join("factory-repo");
        std::fs::create_dir_all(&remote).expect("mkdir");
        git(&remote, &["init", "-q", "--bare", "-b", "main"]);

        let seed = dir.path().join("seed");
        std::fs::create_dir_all(&seed).expect("mkdir");
        git(&seed, &["init", "-q", "-b", "main"]);
        write(&seed.join("README.md"), "# repo\n");
        write(&seed.join(OPEN_DIR).join(".gitkeep"), "");
        write(&seed.join("skills").join("README.md"), "skills live here\n");
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "-q", "-m", "init"]);
        git(
            &seed,
            &["remote", "add", "origin", &remote.to_string_lossy()],
        );
        git(&seed, &["push", "-q", "origin", "main"]);
        git(
            &seed,
            &["push", "-q", "origin", &format!("main:{STATE_BRANCH}")],
        );

        git(
            dir.path(),
            &["clone", "-q", &remote.to_string_lossy(), "laptop"],
        );
        git(
            dir.path(),
            &["clone", "-q", &remote.to_string_lossy(), "factory-repo"],
        );
        let home = FactoryHome::under(&dir.path().join("home"));
        Self {
            dir,
            remote,
            laptop,
            factory,
            home,
        }
    }

    /// Opens the queue on the factory clone.
    pub async fn queue(&self) -> TicketQueue {
        TicketQueue::open(&Git::new(&self.factory), &self.home.state_dir(), "origin")
            .await
            .expect("queue opens")
    }

    /// Pushes a ticket on `tickets/<id>` from the laptop, based on `base`
    /// (`origin/factory/state` or `origin/main`).
    pub fn submit(&self, ticket: &Ticket, base: &str) -> String {
        let branch = format!("tickets/{}", ticket.id);
        git(&self.laptop, &["fetch", "-q", "origin"]);
        git(&self.laptop, &["checkout", "-q", "-B", &branch, base]);
        write(
            &self
                .laptop
                .join(OPEN_DIR)
                .join(format!("{}.toml", ticket.id)),
            &toml::to_string_pretty(ticket).expect("serializes"),
        );
        git(&self.laptop, &["add", "-A"]);
        git(
            &self.laptop,
            &["commit", "-q", "-m", &format!("Add ticket {}", ticket.id)],
        );
        git(&self.laptop, &["push", "-q", "origin", &branch]);
        git(&self.laptop, &["checkout", "-q", "main"]);
        branch
    }

    /// Pushes a branch from the laptop that also changes a non-ticket file.
    pub fn submit_with_extra_file(&self, ticket: &Ticket) -> String {
        let branch = format!("tickets/{}", ticket.id);
        git(&self.laptop, &["fetch", "-q", "origin"]);
        git(
            &self.laptop,
            &[
                "checkout",
                "-q",
                "-B",
                &branch,
                &format!("origin/{STATE_BRANCH}"),
            ],
        );
        write(
            &self
                .laptop
                .join(OPEN_DIR)
                .join(format!("{}.toml", ticket.id)),
            &toml::to_string_pretty(ticket).expect("serializes"),
        );
        write(&self.laptop.join("README.md"), "# tampered\n");
        git(&self.laptop, &["add", "-A"]);
        git(
            &self.laptop,
            &["commit", "-q", "-m", "ticket plus a change"],
        );
        git(&self.laptop, &["push", "-q", "origin", &branch]);
        git(&self.laptop, &["checkout", "-q", "main"]);
        branch
    }

    /// The files under `tickets/` on the remote's state branch.
    pub fn state_files(&self) -> Vec<String> {
        let out = git(
            &self.remote,
            &[
                "ls-tree",
                "-r",
                "--name-only",
                STATE_BRANCH,
                "--",
                "tickets",
            ],
        );
        out.lines().map(str::to_owned).collect()
    }

    /// The subject lines on the remote's state branch, newest first.
    pub fn state_log(&self) -> Vec<String> {
        git(&self.remote, &["log", "--format=%s", STATE_BRANCH])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The `tickets/*` branches on the remote.
    pub fn remote_ticket_branches(&self) -> Vec<String> {
        git(
            &self.remote,
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads/tickets/",
            ],
        )
        .lines()
        .map(str::to_owned)
        .collect()
    }

    /// A ticket file's content on the remote's state branch.
    pub fn state_ticket(&self, dir: &str, id: Ulid) -> Ticket {
        let text = git(
            &self.remote,
            &["show", &format!("{STATE_BRANCH}:{dir}/{id}.toml")],
        );
        toml::from_str(&text).expect("ticket parses")
    }
}

/// Distinguishes ids made in the same millisecond.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A fresh, time-ordered ticket id.
pub fn fresh_id() -> Ulid {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Ulid::from_parts(millis, u128::from(NEXT_ID.fetch_add(1, Ordering::SeqCst)))
}

/// A valid open ticket.
pub fn ticket(priority: Priority, created: &str, kind: TicketKind) -> Ticket {
    Ticket {
        id: fresh_id(),
        created_at: created.parse().expect("rfc3339"),
        kind,
        title: format!("A {kind:?} ticket at {priority:?}"),
        symptom: "Something is off.".to_owned(),
        repro: None,
        spec: "Make it right.".to_owned(),
        acceptance: vec!["cargo test passes".to_owned()],
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

/// Whether git is available; tests skip without it.
pub fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}
