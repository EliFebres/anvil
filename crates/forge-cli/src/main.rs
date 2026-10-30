//! `forge`: the CLI coding agent. Argument parsing and wiring only.

mod cli;
mod config_init;

use clap::Parser;

use crate::cli::{Cli, Command, ConfigCommand};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Config {
            command: ConfigCommand::Init { global },
        } => {
            let path = config_init::run(global)?;
            println!("wrote {}", path.display());
        }
    }
    Ok(())
}
