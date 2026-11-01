//! Sums of slices.

/// The sum of the values `keep` accepts.
fn sum_where(values: &[i64], keep: impl Fn(i64) -> bool) -> i64 {
    let mut total = 0;
    for &value in values {
        if keep(value) {
            total += value;
        }
    }
    total
}

/// The sum of the even values.
pub fn sum_even(values: &[i64]) -> i64 {
    sum_where(values, |v| v % 2 == 0)
}

/// The sum of the odd values.
pub fn sum_odd(values: &[i64]) -> i64 {
    sum_where(values, |v| v % 2 != 0)
}

/// The sum of every value.
pub fn sum_all(values: &[i64]) -> i64 {
    sum_where(values, |_| true)
}
