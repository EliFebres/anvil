//! `grepish [--count] PATTERN FILE`: print (or count) the lines of FILE that contain PATTERN.

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let count_only = args.first().is_some_and(|a| a == "--count");
    if count_only {
        args.remove(0);
    }
    let [pattern, path] = args.as_slice() else {
        eprintln!("usage: grepish [--count] PATTERN FILE");
        return ExitCode::from(2);
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("reading {path}: {error}");
            return ExitCode::from(1);
        }
    };
    let matches = text.lines().filter(|line| line.contains(pattern.as_str()));
    if count_only {
        println!("{}", matches.count());
    } else {
        for line in matches {
            println!("{line}");
        }
    }
    ExitCode::SUCCESS
}
