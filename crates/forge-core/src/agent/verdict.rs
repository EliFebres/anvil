//! Structured verdicts the reviewer and the verify turn must end with.

use serde::{Deserialize, Serialize};

/// What the reviewer decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "lowercase")]
pub enum ReviewVerdict {
    /// Ship it.
    Approve,
    /// Send it back with these requests.
    Changes {
        /// Specific, actionable requests.
        #[serde(default)]
        items: Vec<String>,
    },
}

/// What a model-run verification decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "lowercase")]
pub enum VerifyVerdict {
    /// The targeted test passed.
    Pass,
    /// Something failed.
    Fail {
        /// What and why.
        #[serde(default)]
        details: String,
    },
}

/// Finds the last JSON object in a reply and parses it as a verdict.
///
/// A fenced ```json block is preferred; otherwise the last `{...}` span is
/// tried. Returns `None` when nothing parses.
///
/// # Arguments
///
/// * `text` - The model's reply.
///
/// # Returns
///
/// The verdict, if one was found.
pub fn parse_verdict<T: serde::de::DeserializeOwned>(text: &str) -> Option<T> {
    for candidate in candidates(text) {
        if let Ok(verdict) = serde_json::from_str::<T>(candidate) {
            return Some(verdict);
        }
    }
    None
}

/// JSON spans to try, most likely first: fenced blocks from the end, then
/// the last brace-delimited span.
fn candidates(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.rfind("```") {
        let before = &rest[..start];
        let Some(open) = before.rfind("```") else {
            break;
        };
        let body = &before[open + 3..];
        let body = body
            .strip_prefix("json")
            .or_else(|| body.strip_prefix("JSON"))
            .unwrap_or(body)
            .trim();
        found.push(body);
        rest = &rest[..open];
    }
    if let (Some(open), Some(close)) = (text.find('{'), text.rfind('}'))
        && open < close
    {
        found.push(&text[open..=close]);
        if let Some(last_open) = text[..close].rfind('{')
            && last_open > open
        {
            found.push(&text[last_open..=close]);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_review_verdicts() {
        let text = "Looks fine overall.\n\n```json\n{\"verdict\": \"approve\"}\n```\n";
        assert_eq!(
            parse_verdict::<ReviewVerdict>(text),
            Some(ReviewVerdict::Approve)
        );
        let text = "Two issues.\n```json\n{\"verdict\":\"changes\",\"items\":[\"a\",\"b\"]}\n```";
        assert_eq!(
            parse_verdict::<ReviewVerdict>(text),
            Some(ReviewVerdict::Changes {
                items: vec!["a".to_owned(), "b".to_owned()]
            })
        );
    }

    #[test]
    fn prefers_the_last_block_and_falls_back_to_bare_json() {
        let text = "```json\n{\"verdict\": \"changes\"}\n```\nOn reflection:\n```json\n{\"verdict\": \"approve\"}\n```";
        assert_eq!(
            parse_verdict::<ReviewVerdict>(text),
            Some(ReviewVerdict::Approve)
        );
        let text = "verdict: {\"verdict\": \"pass\"} done";
        assert_eq!(
            parse_verdict::<VerifyVerdict>(text),
            Some(VerifyVerdict::Pass)
        );
        let text = "{\"other\": 1} then {\"verdict\": \"fail\", \"details\": \"x\"}";
        assert_eq!(
            parse_verdict::<VerifyVerdict>(text),
            Some(VerifyVerdict::Fail {
                details: "x".to_owned()
            })
        );
    }

    #[test]
    fn missing_or_wrong_json_is_none() {
        assert_eq!(parse_verdict::<ReviewVerdict>("no verdict here"), None);
        assert_eq!(
            parse_verdict::<ReviewVerdict>("```json\n{\"verdict\": \"maybe\"}\n```"),
            None
        );
        assert_eq!(parse_verdict::<ReviewVerdict>("```\nnot json\n```"), None);
    }
}
