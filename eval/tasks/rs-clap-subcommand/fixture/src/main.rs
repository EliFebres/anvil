//! `notes`: a one-file note list kept in `notes.txt` in the current folder.

use std::fs;
use std::path::Path;

use clap::{Parser, Subcommand};

const STORE: &str = "notes.txt";

/// Keep short notes.
#[derive(Parser)]
#[command(name = "notes")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Append a note.
    Add {
        /// The note text.
        text: String,
    },
    /// Print every note, numbered from 1.
    List,
}

fn load() -> Vec<String> {
    match fs::read_to_string(STORE) {
        Ok(text) => text.lines().map(str::to_owned).collect(),
        Err(_) => Vec::new(),
    }
}

fn save(notes: &[String]) {
    let mut text = notes.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    fs::write(Path::new(STORE), text).expect("writing notes.txt");
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Add { text } => {
            let mut notes = load();
            notes.push(text);
            save(&notes);
        }
        Command::List => {
            for (index, note) in load().iter().enumerate() {
                println!("{}. {note}", index + 1);
            }
        }
    }
}
