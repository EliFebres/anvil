//! Redaction: the one place secret-shaped text is replaced before it enters
//! a conversation, a transcript or a tool-output file.
//!
//! [`KEY_SHAPES`] are the built-in patterns; `anvil scan-keys` reads the
//! same list. A [`Redactor`] adds the exact values of secret-looking
//! environment variables and a policy's extra patterns, and replaces every
//! match with `[REDACTED:<kind>]`. It reports how many of each kind it
//! replaced, never the value.

use std::collections::BTreeMap;

use regex::Regex;
use thiserror::Error;

/// One built-in secret shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyShape {
    /// The kind written into `[REDACTED:<kind>]`.
    pub kind: &'static str,
    /// What `anvil scan-keys` prints for a hit.
    pub label: &'static str,
    /// The regular expression. Written so that this source file never
    /// matches itself.
    pub pattern: &'static str,
    /// Whether `anvil scan-keys` looks for it in tracked files.
    pub scan: bool,
}

/// The built-in secret shapes, most specific first.
///
/// Private key blocks are redacted but not scanned for: a repository may
/// commit test-only keys on purpose (this one does, for its TLS tests), and
/// the file tools already refuse `*.key` and `*.pem` files.
pub const KEY_SHAPES: &[KeyShape] = &[
    KeyShape {
        kind: "private_key",
        label: "private key block",
        pattern: r"(?s)-----BEGIN ([A-Z0-9]+ )*PRIVATE KEY-----.*?(-----END ([A-Z0-9]+ )*PRIVATE KEY-----|\z)",
        scan: false,
    },
    KeyShape {
        kind: "jwt",
        label: "JWT",
        pattern: r"\beyJ[A-Za-z0-9_-]{5,}\.eyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]*",
        scan: true,
    },
    KeyShape {
        kind: "aws_access_key",
        label: "AWS access key",
        pattern: r"\b(AKIA|ASIA)[0-9A-Z]{16}\b",
        scan: true,
    },
    KeyShape {
        kind: "github_token",
        label: "github_pat_ token",
        pattern: "github_pat_[A-Za-z0-9_]{22,}",
        scan: true,
    },
    KeyShape {
        kind: "github_token",
        label: "ghp_ token",
        pattern: "ghp_[A-Za-z0-9]{36}",
        scan: true,
    },
    KeyShape {
        kind: "api_key",
        label: "sk- key",
        pattern: "sk-[A-Za-z0-9_-]{20,}",
        scan: true,
    },
    KeyShape {
        kind: "google_api_key",
        label: "AIza key",
        pattern: "AIza[0-9A-Za-z_-]{35}",
        scan: true,
    },
    KeyShape {
        kind: "huggingface_token",
        label: "hf_ token",
        pattern: "hf_[A-Za-z0-9]{20,}",
        scan: true,
    },
];

/// Shortest environment value that is redacted. Shorter values (`1`,
/// `true`, a port) would replace ordinary text.
pub const MIN_ENV_VALUE_LEN: usize = 8;

/// The longest kind name a policy pattern may use.
const MAX_KIND_LEN: usize = 40;

/// Why a policy-supplied pattern cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RedactError {
    /// The kind is empty, too long, or not lowercase letters, digits and `_`.
    #[error("redaction kind `{kind}` must be 1 to {MAX_KIND_LEN} lowercase letters, digits or `_`")]
    Kind {
        /// The kind as written.
        kind: String,
    },
    /// The regular expression does not compile.
    #[error("redaction pattern `{kind}` is not a valid regular expression: {reason}")]
    Regex {
        /// The pattern's kind.
        kind: String,
        /// What the regex parser said.
        reason: String,
    },
    /// The regular expression matches the empty string, so it would insert
    /// a marker between every character.
    #[error("redaction pattern `{kind}` matches the empty string")]
    MatchesEmpty {
        /// The pattern's kind.
        kind: String,
    },
}

/// A pattern to redact besides the built-in ones: a kind and a regex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraPattern {
    /// The kind written into `[REDACTED:<kind>]`.
    pub kind: String,
    /// The regular expression.
    pub regex: String,
}

/// Checks a pattern a policy supplies.
///
/// # Arguments
///
/// * `kind` - The kind name.
/// * `regex` - The regular expression.
///
/// # Returns
///
/// The compiled expression.
///
/// # Errors
///
/// A [`RedactError`] naming the pattern.
pub fn compile_extra(kind: &str, regex: &str) -> Result<Regex, RedactError> {
    let kind_ok = !kind.is_empty()
        && kind.len() <= MAX_KIND_LEN
        && kind
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !kind_ok {
        return Err(RedactError::Kind {
            kind: kind.to_owned(),
        });
    }
    let compiled = Regex::new(regex).map_err(|error| RedactError::Regex {
        kind: kind.to_owned(),
        reason: error.to_string(),
    })?;
    if compiled.is_match("") {
        return Err(RedactError::MatchesEmpty {
            kind: kind.to_owned(),
        });
    }
    Ok(compiled)
}

/// Text after redaction, with what was replaced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Redacted {
    /// The text with every match replaced by `[REDACTED:<kind>]`.
    pub text: String,
    /// How many matches of each kind were replaced. Never holds a value.
    pub counts: BTreeMap<String, usize>,
}

impl Redacted {
    /// The number of replacements of every kind.
    ///
    /// # Returns
    ///
    /// The sum of [`Redacted::counts`].
    pub fn total(&self) -> usize {
        self.counts.values().sum()
    }
}

/// Replaces secrets in text. Cheap to share behind an `Arc`.
#[derive(Debug, Clone)]
pub struct Redactor {
    /// Whether anything is replaced; `false` only when a policy turns
    /// redaction off.
    enabled: bool,
    /// Kinds and compiled patterns: the built-in shapes, then the policy's.
    patterns: Vec<(String, Regex)>,
    /// Exact environment values, longest first, with `env:<NAME>` kinds.
    values: Vec<(String, String)>,
}

impl Default for Redactor {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Redactor {
    /// A redactor with the built-in shapes only.
    ///
    /// # Returns
    ///
    /// The redactor.
    pub fn builtin() -> Self {
        let patterns = KEY_SHAPES
            .iter()
            .filter_map(|shape| {
                Regex::new(shape.pattern)
                    .ok()
                    .map(|re| (shape.kind.to_owned(), re))
            })
            .collect::<Vec<_>>();
        debug_assert_eq!(
            patterns.len(),
            KEY_SHAPES.len(),
            "every built-in key shape must compile"
        );
        Self {
            enabled: true,
            patterns,
            values: Vec::new(),
        }
    }

    /// A redactor that changes nothing, for a policy with `redaction = false`.
    ///
    /// # Returns
    ///
    /// The redactor.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            patterns: Vec::new(),
            values: Vec::new(),
        }
    }

    /// The built-in shapes, the given extra patterns and environment values.
    ///
    /// # Arguments
    ///
    /// * `extra` - Patterns from the policy.
    /// * `env` - Variable names and values to consider, usually
    ///   `std::env::vars()`.
    /// * `secret_names` - Names whose values count as secrets even when
    ///   they do not look secret, such as each provider's `api_key_env`.
    ///
    /// # Returns
    ///
    /// The redactor.
    ///
    /// # Errors
    ///
    /// A [`RedactError`] when an extra pattern is unusable.
    pub fn new(
        extra: &[ExtraPattern],
        env: impl IntoIterator<Item = (String, String)>,
        secret_names: &[String],
    ) -> Result<Self, RedactError> {
        let mut redactor = Self::builtin();
        for pattern in extra {
            let compiled = compile_extra(&pattern.kind, &pattern.regex)?;
            redactor.patterns.push((pattern.kind.clone(), compiled));
        }
        redactor.values = secret_values(env, secret_names);
        Ok(redactor)
    }

    /// Whether this redactor replaces anything.
    ///
    /// # Returns
    ///
    /// `false` only for [`Redactor::disabled`].
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Replaces every secret in `text`.
    ///
    /// Environment values go first, so a key held in a variable is reported
    /// by its variable's name; the patterns follow in order.
    ///
    /// # Arguments
    ///
    /// * `text` - Any text.
    ///
    /// # Returns
    ///
    /// The redacted text and the count of each kind.
    pub fn redact(&self, text: &str) -> Redacted {
        let mut out = Redacted {
            text: text.to_owned(),
            counts: BTreeMap::new(),
        };
        if !self.enabled {
            return out;
        }
        for (kind, value) in &self.values {
            let found = out.text.matches(value.as_str()).count();
            if found > 0 {
                out.text = out.text.replace(value.as_str(), &marker(kind));
                *out.counts.entry(kind.clone()).or_default() += found;
            }
        }
        for (kind, re) in &self.patterns {
            let found = re.find_iter(&out.text).count();
            if found > 0 {
                out.text = re.replace_all(&out.text, marker(kind)).into_owned();
                *out.counts.entry(kind.clone()).or_default() += found;
            }
        }
        out
    }
}

/// The text that replaces a secret of `kind`.
///
/// # Arguments
///
/// * `kind` - The kind.
///
/// # Returns
///
/// `[REDACTED:<kind>]`.
pub fn marker(kind: &str) -> String {
    format!("[REDACTED:{kind}]")
}

/// The values of variables that hold secrets, longest first, with kinds.
///
/// A variable counts when its name looks secret
/// ([`crate::child_env::looks_secret`]) or is in `secret_names`, and its
/// value is at least [`MIN_ENV_VALUE_LEN`] characters and not a placeholder.
fn secret_values(
    env: impl IntoIterator<Item = (String, String)>,
    secret_names: &[String],
) -> Vec<(String, String)> {
    let mut values: Vec<(String, String)> = env
        .into_iter()
        .filter(|(name, value)| {
            let named = crate::child_env::looks_secret(name)
                || secret_names.iter().any(|n| n.eq_ignore_ascii_case(name));
            let value = value.trim();
            named && value.len() >= MIN_ENV_VALUE_LEN && !crate::dotenv::is_placeholder(value)
        })
        .map(|(name, value)| (format!("env:{name}"), value.trim().to_owned()))
        .collect();
    values.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    values.dedup_by(|a, b| a.1 == b.1);
    values
}

/// Secret-shaped strings built at run time, so this file never holds one.
#[cfg(test)]
pub(crate) mod fakes {
    /// A fake `sk-` key.
    pub fn api_key() -> String {
        format!("sk-{}", "a1".repeat(20))
    }

    /// A fake AWS access key id.
    pub fn aws() -> String {
        format!("{}{}", "AKIA", "ABCDEFGHIJ234567")
    }

    /// A fake JWT.
    pub fn jwt() -> String {
        format!(
            "{}{}.{}{}.{}",
            "eyJ", "hbGciOiJIUzI1NiJ9", "eyJ", "zdWIiOiIxIn0", "c2lnbmF0dXJl"
        )
    }

    /// A fake PEM private key block.
    pub fn pem() -> String {
        let label = ["PRIVATE", "KEY"].join(" ");
        format!("-----BEGIN {label}-----\nMIIBVQIBADANBgkqhkiG\n-----END {label}-----")
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::*;
    use super::*;

    #[test]
    fn every_built_in_shape_is_replaced_with_its_kind() {
        let redactor = Redactor::builtin();
        let cases = [
            (api_key(), "api_key"),
            (aws(), "aws_access_key"),
            (jwt(), "jwt"),
            (pem(), "private_key"),
            (format!("ghp_{}", "b".repeat(36)), "github_token"),
            (format!("github_pat_{}", "c".repeat(30)), "github_token"),
            (format!("AIza{}", "d".repeat(35)), "google_api_key"),
            (format!("hf_{}", "e".repeat(24)), "huggingface_token"),
        ];
        for (secret, kind) in cases {
            let out = redactor.redact(&format!("before {secret} after"));
            assert_eq!(
                out.text,
                format!("before [REDACTED:{kind}] after"),
                "{kind}"
            );
            assert_eq!(out.counts.get(kind), Some(&1), "{kind}");
            assert!(!format!("{out:?}").contains(&secret), "{kind}");
        }
    }

    #[test]
    fn a_private_key_cut_off_by_truncation_is_redacted_to_the_end() {
        let pem = pem();
        let cut = &pem[..pem.len() - 10];
        let out = Redactor::builtin().redact(&format!("x\n{cut}"));
        assert_eq!(out.text, "x\n[REDACTED:private_key]");
    }

    #[test]
    fn environment_values_are_redacted_by_variable_name_only_when_secret() {
        let env = [
            ("GITHUB_TOKEN".to_owned(), "tok-value-12345".to_owned()),
            ("MY_ENDPOINT".to_owned(), "endpoint-value-678".to_owned()),
            ("PATH".to_owned(), "/usr/local/bin".to_owned()),
            ("SHORT_KEY".to_owned(), "abc".to_owned()),
            ("EMPTY_SECRET".to_owned(), "<fill me>".to_owned()),
        ];
        let redactor = Redactor::new(&[], env, &["MY_ENDPOINT".to_owned()]).expect("builds");
        let out = redactor.redact("a tok-value-12345 b endpoint-value-678 c /usr/local/bin abc");
        assert_eq!(
            out.text,
            "a [REDACTED:env:GITHUB_TOKEN] b [REDACTED:env:MY_ENDPOINT] c /usr/local/bin abc"
        );
        assert_eq!(out.total(), 2);
    }

    #[test]
    fn policy_patterns_are_added_and_checked() {
        let extra = [ExtraPattern {
            kind: "employee_id".to_owned(),
            regex: r"\bEMP-[0-9]{6}\b".to_owned(),
        }];
        let redactor = Redactor::new(&extra, [], &[]).expect("builds");
        let out = redactor.redact("EMP-123456 and EMP-654321");
        assert_eq!(
            out.text,
            "[REDACTED:employee_id] and [REDACTED:employee_id]"
        );
        assert_eq!(out.counts.get("employee_id"), Some(&2));

        assert!(matches!(
            compile_extra("Bad Kind", "x"),
            Err(RedactError::Kind { .. })
        ));
        assert!(matches!(
            compile_extra("k", "("),
            Err(RedactError::Regex { .. })
        ));
        assert!(matches!(
            compile_extra("k", "a*"),
            Err(RedactError::MatchesEmpty { .. })
        ));
    }

    #[test]
    fn a_disabled_redactor_changes_nothing() {
        let text = format!("key {}", api_key());
        let out = Redactor::disabled().redact(&text);
        assert_eq!(out.text, text);
        assert_eq!(out.total(), 0);
        assert!(!Redactor::disabled().is_enabled());
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        let text = "fn main() { let sk = \"sk-short\"; } // AKIA in prose; eyJ.eyJ.";
        assert_eq!(Redactor::builtin().redact(text).text, text);
    }
}
