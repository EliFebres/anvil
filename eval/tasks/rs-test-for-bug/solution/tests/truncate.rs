use truncate::truncate_words;

#[test]
fn shorter_text_is_unchanged() {
    assert_eq!(truncate_words("a b", 3), "a b");
}

#[test]
fn exact_word_count_is_unchanged() {
    assert_eq!(truncate_words("a b c", 3), "a b c");
}

#[test]
fn longer_text_is_cut_with_an_ellipsis() {
    assert_eq!(truncate_words("a b c d e", 3), "a b c...");
}
