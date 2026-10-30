//! `factory`: the unattended improvement service. Argument parsing and wiring only.

use clap::Parser;

/// The Dark Factory: works tickets from `tickets/open/` into reviewed pull requests.
#[derive(Debug, Parser)]
#[command(name = "factory", version, about, arg_required_else_help = true)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
