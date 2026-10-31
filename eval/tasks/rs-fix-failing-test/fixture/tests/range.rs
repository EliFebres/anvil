use ranges::{expand, parse_range};

#[test]
fn parses_a_range() {
    assert_eq!(parse_range("3-7"), Some((3, 7)));
    assert_eq!(parse_range("7-3"), None);
    assert_eq!(parse_range("x"), None);
}

#[test]
fn expands_inclusively() {
    assert_eq!(expand("3-7"), vec![3, 4, 5, 6, 7]);
    assert_eq!(expand("5-5"), vec![5]);
    assert_eq!(expand("bad"), Vec::<u32>::new());
}
