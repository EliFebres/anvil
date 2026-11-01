//! Parses `a-b` ranges and expands them.

/// Parses `a-b` into `(a, b)` when `a <= b`.
pub fn parse_range(text: &str) -> Option<(u32, u32)> {
    let (start, end) = text.split_once('-')?;
    let start: u32 = start.trim().parse().ok()?;
    let end: u32 = end.trim().parse().ok()?;
    (start <= end).then_some((start, end))
}

/// Every number from `a` to `b` inclusive, or nothing for a bad range.
pub fn expand(text: &str) -> Vec<u32> {
    match parse_range(text) {
        Some((start, end)) => (start..=end).collect(),
        None => Vec::new(),
    }
}
