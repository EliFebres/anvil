use sums::{sum_all, sum_even, sum_odd};

#[test]
fn sums() {
    let values = [1, 2, 3, 4, 5, 6];
    assert_eq!(sum_even(&values), 12);
    assert_eq!(sum_odd(&values), 9);
    assert_eq!(sum_all(&values), 21);
    assert_eq!(sum_even(&[]), 0);
}
