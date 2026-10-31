from duration import parse_duration


def test_seconds():
    assert parse_duration("30s") == 30


def test_minutes():
    assert parse_duration("90m") == 5400


def test_hours():
    assert parse_duration("2h") == 7200


def test_bare_number_is_seconds():
    assert parse_duration("15") == 15
