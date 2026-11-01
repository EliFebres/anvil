//! `grepish PATTERN FILE`: print every line of FILE that contains PATTERN.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [pattern, path] = args.as_slice() else {
        eprintln!("usage: grepish PATTERN FILE");
        return ExitCode::from(2);
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("reading {path}: {error}");
            return ExitCode::from(1);
        }
    };
    for line in text.lines().filter(|line| line.contains(pattern.as_str())) {
        println!("{line}");
    }
    ExitCode::SUCCESS
}
