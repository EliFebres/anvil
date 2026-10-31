//! Fixture: every public item has a doc comment.

/// Adds two numbers.
///
/// # Arguments
///
/// * `a` - First.
/// * `b` - Second.
///
/// # Returns
///
/// The sum.
pub fn add(a: u32, b: u32) -> u32 {
    a + b
}

/// Counts things.
#[derive(Debug, Default)]
pub struct Counter {
    /// The count.
    pub n: u32,
}

impl Counter {
    /// Increments the count.
    pub fn bump(&mut self) {
        self.n += 1;
    }

    fn hidden(&self) {}
}
