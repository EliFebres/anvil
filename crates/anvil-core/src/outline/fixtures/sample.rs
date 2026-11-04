//! A counter, used as an outline fixture.

use std::collections::HashMap;
use std::fmt;

/// Counts things by name.
#[derive(Debug, Default)]
pub struct Counter {
    counts: HashMap<String, u32>,
}

impl Counter {
    /// Adds one to `name`.
    pub fn increment(&mut self, name: &str) {
        *self.counts.entry(name.to_owned()).or_default() += 1;
    }

    /// How many times `name` was counted.
    pub fn get(&self, name: &str) -> u32 {
        self.counts.get(name).copied().unwrap_or(0)
    }
}

impl fmt::Display for Counter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} names", self.counts.len())
    }
}

/// What a counter can do.
pub trait Count {
    /// Counts one thing.
    fn count(&mut self, name: &str);
}

/// The kinds of thing counted.
pub enum Kind {
    /// A fruit.
    Fruit,
    /// A vegetable.
    Vegetable,
}

mod helpers {
    /// Doubles a number.
    pub fn double(n: u32) -> u32 {
        n * 2
    }
}
