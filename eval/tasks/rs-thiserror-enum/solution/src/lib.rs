//! Reads `key = value` lines from a file.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Why a read failed.
#[derive(Debug, Error)]
pub enum KvError {
    /// The file holds no such key.
    #[error("missing key: {key}")]
    Missing { key: String },
    /// The file could not be read.
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A line is not `key = value`.
    #[error("line `{line}` has no `=`")]
    Parse { line: String },
}

/// Splits one `key = value` line.
pub fn parse_line(line: &str) -> Result<(String, String), KvError> {
    let (key, value) = line.split_once('=').ok_or_else(|| KvError::Parse {
        line: line.to_owned(),
    })?;
    Ok((key.trim().to_owned(), value.trim().to_owned()))
}

/// Reads the value of `key` from the file at `path`.
pub fn get(path: &Path, key: &str) -> Result<String, KvError> {
    let text = std::fs::read_to_string(path).map_err(|source| KvError::Io {
        path: path.to_owned(),
        source,
    })?;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (k, v) = parse_line(line)?;
        if k == key {
            return Ok(v);
        }
    }
    Err(KvError::Missing { key: key.to_owned() })
}
