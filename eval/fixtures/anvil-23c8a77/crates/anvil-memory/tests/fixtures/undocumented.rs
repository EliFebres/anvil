//! Fixture: public items without doc comments.

pub fn add(a: u32, b: u32) -> u32 {
    a + b
}

#[derive(Debug, Default)]
pub struct Counter {
    pub n: u32,
}

impl Counter {
    pub fn bump(&mut self) {
        self.n += 1;
    }

    fn hidden(&self) {}
}
