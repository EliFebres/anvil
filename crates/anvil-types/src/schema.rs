//! JSON Schema export for the ticket, so tools outside Rust can validate ticket files.

use schemars::{Schema, schema_for};

use crate::ticket::Ticket;

/// Builds the JSON Schema for [`Ticket`].
///
/// # Returns
///
/// The schema as a `schemars::Schema`, which serializes to JSON.
pub fn ticket_schema() -> Schema {
    schema_for!(Ticket)
}

/// Renders the [`Ticket`] JSON Schema as pretty-printed JSON.
///
/// # Returns
///
/// The schema text, ending without a trailing newline.
///
/// # Errors
///
/// Only if `serde_json` fails to serialize the schema, which indicates a bug.
pub fn ticket_schema_json() -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&ticket_schema())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_schema_is_stable() {
        insta::assert_json_snapshot!(ticket_schema());
    }

    #[test]
    fn ticket_schema_names_the_type_and_closes_it() {
        let json: serde_json::Value =
            serde_json::from_str(&ticket_schema_json().expect("renders")).expect("valid json");
        assert_eq!(json["title"], "Ticket");
        assert_eq!(json["additionalProperties"], false);
    }
}
