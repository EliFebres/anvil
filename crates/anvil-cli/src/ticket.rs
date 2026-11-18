//! `anvil ticket`: drafting, reviewing and submitting improvement tickets.
//!
//! Drafts are written by the summarizer from recorded signals and live in
//! `~/.anvil/tickets/draft/` on this machine only. Submitting validates a
//! draft, moves it to `~/.anvil/tickets/open/` and pushes it on its own
//! branch to the ticket queue named by `[tickets] queue`; that push is the
//! only thing that leaves the machine. With no queue configured nothing is
//! sent.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anvil_core::context::collect_text;
use anvil_core::provider::{CompletionRequest, Message, Provider};
use anvil_telemetry::paths::{DRAFT_DIR, project_telemetry_dir, signals_file};
use anvil_telemetry::{
    DraftFuture, Drafter, QUEUE_REMOTE, StoredTicket, TicketStore, draft_all, load_records,
    load_signals, load_ticket, publish_ticket, save_signals, sync_queue,
};
use anvil_types::TicketStatus;
use anvil_types::config::CONFIG_DIR;
use anyhow::{Context, bail};
use chrono::Utc;

use crate::cli::TicketCommand;
use crate::terminal::read_line;
use crate::wiring::{ProjectAccess, Wiring};

/// The bare cache of the ticket queue, inside the anvil home.
const QUEUE_CACHE: &str = "queue.git";

/// What `submit` says when `[tickets] queue` is not set.
const NO_QUEUE: &str =
    "no ticket queue is configured, so nothing was sent; set it in ~/.anvil/config.toml:

[tickets]
queue = \"https://github.com/<owner>/<queue-repository>.git\"

then submit again. The draft stays in ~/.anvil/tickets/draft/.";

/// Runs one `anvil ticket` subcommand.
///
/// # Arguments
///
/// * `command` - The subcommand.
/// * `home` - The home directory, if known; tickets live under `<home>/.anvil`.
/// * `cwd` - The project directory, for its telemetry and configuration.
///
/// # Returns
///
/// Zero on success, one when a submit or draft partly failed.
///
/// # Errors
///
/// When the home directory is unknown, the ticket folders cannot be read, a
/// ticket is not found, submitting has no queue to send to, or drafting
/// needs a configuration that is missing.
pub async fn run(command: TicketCommand, home: Option<&Path>, cwd: &Path) -> anyhow::Result<i32> {
    let root = home
        .context("cannot determine the home directory; set ANVIL_HOME")?
        .join(CONFIG_DIR);
    migrate_project_drafts(cwd, &root)?;
    let mut store = TicketStore::load(&root).context("reading the ticket folders")?;
    match command {
        TicketCommand::List => {
            list(&store);
            Ok(0)
        }
        TicketCommand::Show { id } => {
            print!("{}", render(find_any(&store, &id)?));
            Ok(0)
        }
        TicketCommand::Drop { id } => {
            let ticket = store.drop_draft(&id)?;
            println!("dropped {}: {}", ticket.id, ticket.title);
            Ok(0)
        }
        TicketCommand::Submit { id, all } => {
            let queue = queue_url(home).context(NO_QUEUE)?;
            let ids: Vec<String> = if all {
                store
                    .drafts
                    .iter()
                    .map(|stored| stored.ticket.id.to_string())
                    .collect()
            } else {
                vec![id.context("give a ticket id or --all")?]
            };
            if ids.is_empty() {
                println!("no drafts to submit");
                return Ok(0);
            }
            let cache = root.join(QUEUE_CACHE);
            sync_queue(&cache, &queue)
                .with_context(|| format!("fetching the ticket queue {queue}; nothing was sent"))?;
            let mut failures = 0;
            for id in ids {
                if let Err(error) = submit(&mut store, &cache, &queue, &id) {
                    eprintln!("error: {error:#}");
                    failures += 1;
                }
            }
            Ok(i32::from(failures > 0))
        }
        TicketCommand::Draft => {
            let config = anvil_types::config::load(home)?.config;
            let wiring = Wiring::new(config, ProjectAccess::default())?;
            draft(&wiring, &mut store).await
        }
        TicketCommand::Review => {
            let queue = queue_url(home);
            review(&mut store, &root.join(QUEUE_CACHE), queue.as_deref())
        }
    }
}

/// `[tickets] queue` from the configuration; `None` when it is unset or
/// there is no configuration.
fn queue_url(home: Option<&Path>) -> Option<String> {
    anvil_types::config::load(home)
        .ok()
        .and_then(|loaded| loaded.config.tickets.queue)
}

/// Moves drafts an earlier anvil wrote to `<project>/tickets/draft/` into
/// the store under the anvil home, once. A draft whose file already exists
/// there is left where it is.
///
/// # Arguments
///
/// * `cwd` - The project directory.
/// * `root` - The store's root, `~/.anvil`.
///
/// # Errors
///
/// When a draft cannot be moved.
fn migrate_project_drafts(cwd: &Path, root: &Path) -> anyhow::Result<()> {
    let old = cwd.join(DRAFT_DIR);
    let Ok(entries) = std::fs::read_dir(&old) else {
        return Ok(());
    };
    let new = root.join(DRAFT_DIR);
    let mut moved = 0;
    for entry in entries {
        let path = entry
            .with_context(|| format!("listing {}", old.display()))?
            .path();
        let Some(name) = path
            .file_name()
            .filter(|_| path.extension().is_some_and(|ext| ext == "toml"))
        else {
            continue;
        };
        let dest: PathBuf = new.join(name);
        if dest.exists() {
            continue;
        }
        std::fs::create_dir_all(&new).with_context(|| format!("creating {}", new.display()))?;
        // A rename cannot cross drives, so copy and remove instead.
        std::fs::copy(&path, &dest)
            .with_context(|| format!("copying {} to {}", path.display(), dest.display()))?;
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        moved += 1;
    }
    if moved > 0 {
        println!(
            "moved {moved} draft(s) from {} to {}",
            old.display(),
            new.display()
        );
    }
    // Only an empty folder is removed; one that still holds files stays.
    let _ = std::fs::remove_dir(&old);
    Ok(())
}

/// Prints one line per ticket, drafts first.
fn list(store: &TicketStore) {
    if store.drafts.is_empty() && store.open.is_empty() {
        println!("no tickets; `anvil ticket draft` turns recorded signals into drafts");
        return;
    }
    for stored in store.drafts.iter().chain(store.open.iter()) {
        let ticket = &stored.ticket;
        println!(
            "{:<6} {} {:<6} {:<3} {} ({} evidence)",
            status_label(ticket.status),
            ticket.id,
            kind_label(ticket),
            priority_label(ticket),
            ticket.title,
            ticket.evidence.len()
        );
    }
}

/// A draft or open ticket by id or prefix.
fn find_any<'a>(store: &'a TicketStore, id: &str) -> anyhow::Result<&'a StoredTicket> {
    if let Ok(draft) = store.find_draft(id) {
        return Ok(draft);
    }
    let upper = id.trim().to_ascii_uppercase();
    store
        .open
        .iter()
        .find(|stored| stored.ticket.id.to_string().starts_with(&upper))
        .with_context(|| format!("no ticket matches `{id}`; `anvil ticket list` shows them"))
}

/// A ticket as its file reads.
fn render(stored: &StoredTicket) -> String {
    format!(
        "# {}\n{}",
        stored.path.display(),
        toml::to_string_pretty(&stored.ticket).unwrap_or_default()
    )
}

/// Validates, moves and publishes one draft.
///
/// # Arguments
///
/// * `store` - The ticket folders.
/// * `cache` - The queue's bare cache, already fetched.
/// * `queue` - The queue's URL, for the message.
/// * `id` - The draft's id or a prefix of it.
fn submit(store: &mut TicketStore, cache: &Path, queue: &str, id: &str) -> anyhow::Result<()> {
    let stored = store
        .submit(id)
        .with_context(|| format!("submitting `{id}`"))?;
    let id = stored.ticket.id.to_string();
    let branch = publish_ticket(cache, QUEUE_REMOTE, &stored.path, &id, &stored.ticket.title)
        .with_context(|| {
            format!(
                "the ticket is in {} but the push failed; fix the cause and run `git --git-dir {} push {QUEUE_REMOTE} tickets/{id}` by hand",
                store.open_dir().display(),
                cache.display()
            )
        })?;
    println!(
        "submitted {id}: {} (pushed {branch} to {queue})",
        stored.ticket.title
    );
    Ok(())
}

/// Turns every unhandled signal into a draft or merges it into a ticket.
async fn draft(wiring: &Wiring, store: &mut TicketStore) -> anyhow::Result<i32> {
    let telemetry_dir = project_telemetry_dir(&wiring.cwd);
    let signals_path = signals_file(&telemetry_dir);
    let mut signals = load_signals(&signals_path).context("reading the signals")?;
    let pending = signals
        .iter()
        .filter(|signal| signal.ticket.is_none())
        .count();
    if pending == 0 {
        println!("no new signals; run a session first");
        return Ok(0);
    }
    let records = load_records(&telemetry_dir, None).context("reading the telemetry")?;
    let name = &wiring.config.roles.summarizer;
    let drafter = SummarizerDrafter {
        provider: wiring.provider(name)?,
    };
    println!("drafting {pending} signal(s) with `{name}`");
    let report = draft_all(&mut signals, &records, store, &drafter, Utc::now())
        .await
        .context("writing drafts")?;
    save_signals(&signals_path, &signals).context("saving the signals")?;
    for (id, title) in &report.created {
        println!("drafted {id}: {title}");
    }
    for (id, title) in &report.merged {
        println!("added evidence to {id}: {title}");
    }
    for (id, error) in &report.failed {
        eprintln!("signal {id} not drafted: {error}");
    }
    if !report.created.is_empty() {
        println!("review them with `anvil ticket review`, or `anvil ticket submit --all`");
    }
    Ok(i32::from(!report.failed.is_empty()))
}

/// Walks the drafts and asks what to do with each.
///
/// # Arguments
///
/// * `store` - The ticket folders.
/// * `cache` - Where the queue's bare cache lives.
/// * `queue` - `[tickets] queue`; submitting is refused while it is `None`.
fn review(store: &mut TicketStore, cache: &Path, queue: Option<&str>) -> anyhow::Result<i32> {
    if store.drafts.is_empty() {
        println!("no drafts to review");
        return Ok(0);
    }
    let ids: Vec<String> = store
        .drafts
        .iter()
        .map(|stored| stored.ticket.id.to_string())
        .collect();
    for id in ids {
        loop {
            let stored = store.find_draft(&id)?;
            println!("\n{}", render(stored));
            let answer = read_line("[s]ubmit / [d]rop / [e]dit / [n]ext / [q]uit: ");
            match answer.trim().to_ascii_lowercase().as_str() {
                "s" | "submit" => {
                    let Some(queue) = queue else {
                        eprintln!("error: {NO_QUEUE}");
                        continue;
                    };
                    let sent = sync_queue(cache, queue)
                        .with_context(|| format!("fetching the ticket queue {queue}"))
                        .and_then(|()| submit(store, cache, queue, &id));
                    if let Err(error) = sent {
                        eprintln!("error: {error:#}");
                    }
                    break;
                }
                "d" | "drop" => {
                    let ticket = store.drop_draft(&id)?;
                    println!("dropped {}", ticket.id);
                    break;
                }
                "e" | "edit" => {
                    let path = stored.path.clone();
                    if let Err(error) = edit(&path) {
                        eprintln!("error: {error:#}");
                        continue;
                    }
                    match load_ticket(&path) {
                        Ok(ticket) => {
                            if let Err(error) = ticket.validate() {
                                eprintln!("the edited draft fails the validator: {error}");
                            }
                            if let Some(slot) = store.drafts.iter_mut().find(|s| s.path == path) {
                                slot.ticket = ticket;
                            }
                        }
                        Err(error) => eprintln!("the edited draft does not parse: {error}"),
                    }
                }
                "n" | "next" | "" => break,
                "q" | "quit" => return Ok(0),
                _ => eprintln!("  please answer s, d, e, n or q"),
            }
        }
    }
    Ok(0)
}

/// Opens a file in `$VISUAL`, `$EDITOR`, or the platform's plain editor.
fn edit(path: &Path) -> anyhow::Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                "notepad".to_owned()
            } else {
                "vi".to_owned()
            }
        });
    let mut words = editor.split_whitespace();
    let program = words.next().context("the editor variable is empty")?;
    let status = std::process::Command::new(program)
        .args(words)
        .arg(path)
        .status()
        .with_context(|| format!("running the editor `{editor}`"))?;
    if !status.success() {
        bail!("the editor exited with {status}");
    }
    Ok(())
}

/// Drafts with the summarizer role.
struct SummarizerDrafter {
    provider: Arc<dyn Provider>,
}

impl Drafter for SummarizerDrafter {
    fn complete<'a>(&'a self, system: &'a str, user: &'a str) -> DraftFuture<'a> {
        Box::pin(async move {
            let request = CompletionRequest {
                system: system.to_owned(),
                messages: vec![Message::user(user)],
                tools: Vec::new(),
                max_output_tokens: None,
            };
            collect_text(&*self.provider, request)
                .await
                .map(|(text, _)| text)
                .map_err(|error| error.to_string())
        })
    }
}

/// The status column of `anvil ticket list`.
fn status_label(status: TicketStatus) -> &'static str {
    match status {
        TicketStatus::Draft => "draft",
        TicketStatus::Open => "open",
        TicketStatus::Claimed => "claimed",
        TicketStatus::InReview => "review",
        TicketStatus::Waiting => "waiting",
        TicketStatus::Merged => "merged",
        TicketStatus::Rejected => "closed",
    }
}

/// The kind column of `anvil ticket list`.
fn kind_label(ticket: &anvil_types::Ticket) -> String {
    serde_json::to_value(ticket.kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The priority column of `anvil ticket list`.
fn priority_label(ticket: &anvil_types::Ticket) -> String {
    serde_json::to_value(ticket.priority)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}
