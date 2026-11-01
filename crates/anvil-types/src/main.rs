//! `anvil-types --schema`: prints the Ticket JSON Schema to stdout.

use std::process::ExitCode;

/// Usage text printed when the arguments are not understood.
const USAGE: &str = "usage: anvil-types --schema\n\nPrints the Ticket JSON Schema to stdout.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args != ["--schema"] {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    match anvil_types::schema::ticket_schema_json() {
        Ok(schema) => {
            println!("{schema}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: cannot render the ticket schema: {err}");
            ExitCode::FAILURE
        }
    }
}
