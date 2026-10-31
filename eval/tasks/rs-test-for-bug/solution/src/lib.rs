//! Shorten text to a number of words.

/// Keeps the first `max_words` words of `text`, adding `...` when words were dropped.
pub fn truncate_words(text: &str, max_words: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() <= max_words {
        return text.to_owned();
    }
    format!("{}...", words[..max_words].join(" "))
}
