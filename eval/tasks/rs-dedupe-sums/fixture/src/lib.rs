//! Sums of slices.

/// The sum of the even values.
pub fn sum_even(values: &[i64]) -> i64 {
    let mut total = 0;
    for value in values {
        if value % 2 == 0 {
            total += value;
        }
    }
    total
}

/// The sum of the odd values.
pub fn sum_odd(values: &[i64]) -> i64 {
    let mut total = 0;
    for value in values {
        if value % 2 != 0 {
            total += value;
        }
    }
    total
}

/// The sum of every value.
pub fn sum_all(values: &[i64]) -> i64 {
    let mut total = 0;
    for value in values {
        total += value;
    }
    total
}
