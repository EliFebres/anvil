from slugify import slugify


def test_lowercases_and_joins_with_hyphens():
    assert slugify("Hello World") == "hello-world"


def test_collapses_repeated_separators():
    assert slugify("hello   world") == "hello-world"


def test_drops_leading_and_trailing_punctuation():
    assert slugify("Hello, World!") == "hello-world"


def test_leaves_a_slug_alone():
    assert slugify("already-a-slug") == "already-a-slug"
