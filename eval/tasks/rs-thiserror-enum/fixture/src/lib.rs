//! Reads `key = value` lines from a file.

use std::path::Path;

/// Splits one `key = value` line.
pub fn parse_line(line: &str) -> Result<(String, String), String> {
    let (key, value) = line
        .split_once('=')
        .ok_or_else(|| format!("line `{line}` has no `=`"))?;
    Ok((key.trim().to_owned(), value.trim().to_owned()))
}

/// Reads the value of `key` from the file at `path`.
pub fn get(path: &Path, key: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (k, v) = parse_line(line)?;
        if k == key {
            return Ok(v);
        }
    }
    Err(format!("missing key: {key}"))
}
