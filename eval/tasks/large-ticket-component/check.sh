#!/usr/bin/env bash
# A hidden test checks the field's rules through toml and Ticket::validate;
# the schema names it and every crate that builds a Ticket still compiles.
set -eu

cat > crates/anvil-types/tests/eval_component.rs <<'RS'
use anvil_types::ticket::Ticket;

const BASE: &str = r#"
id = "01J9Z6Q4K8XW2R3T5V7Y9B1D3F"
created_at = "2026-10-30T10:00:00Z"
kind = "tool"
title = "Add a flag"
symptom = "Something is missing."
spec = "A flag."
acceptance = ["cargo test passes"]
priority = "p2"
status = "open"
origin = "cli"
"#;

fn ticket(extra: &str) -> Ticket {
    toml::from_str(&format!("{BASE}{extra}")).expect("parses")
}

#[test]
fn component_is_optional_and_checked() {
    ticket("").validate().expect("no component is fine");
    let named = ticket("component = \"anvil-core\"\n");
    assert_eq!(named.component.as_deref(), Some("anvil-core"));
    named.validate().expect("a crate name is fine");
    for bad in ["Bad Name", "", "under_score", &"a".repeat(65)] {
        let result = ticket(&format!("component = \"{bad}\"\n")).validate();
        assert!(result.is_err(), "{bad:?} must be rejected");
        assert!(!result.unwrap_err().to_string().is_empty());
    }
    ticket(&format!("component = \"{}\"\n", "a".repeat(64))).validate().expect("64 is fine");
}
RS
cargo test -q -p anvil-types
cargo run -q -p anvil-types -- --schema | grep -q '"component"'
cargo test -q -p anvil-telemetry
cargo build -q -p anvil-cli
