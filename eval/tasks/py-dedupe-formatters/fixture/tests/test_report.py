from report import format_body_row, format_footer_row, format_header_row


def test_header_is_uppercased():
    assert format_header_row(["name", " qty\n"]) == "| NAME | QTY |"


def test_body_keeps_case_and_strips():
    assert format_body_row([" bolt ", 3, "two\nlines"]) == "| bolt | 3 | two lines |"


def test_footer_is_starred():
    assert format_footer_row(["total", 3]) == "| *total* | *3* |"
