use kvstore::{get, parse_line};

#[test]
fn parses_key_and_value() {
    assert_eq!(
        parse_line("name = anvil").unwrap(),
        ("name".to_owned(), "anvil".to_owned())
    );
}

#[test]
fn rejects_a_line_without_equals() {
    let error = parse_line("nonsense").unwrap_err();
    assert!(error.to_string().contains("nonsense"), "{error}");
}

#[test]
fn reads_a_key_from_a_file() {
    let dir = std::env::temp_dir().join(format!("kvstore-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("store.txt");
    std::fs::write(&path, "a = 1\nb = 2\n").unwrap();
    assert_eq!(get(&path, "b").unwrap(), "2");
    let missing = get(&path, "zzz").unwrap_err();
    assert!(missing.to_string().contains("zzz"), "{missing}");
    let unreadable = get(&dir.join("nope.txt"), "a").unwrap_err();
    assert!(!unreadable.to_string().is_empty());
}
