"""Render table rows."""


def _clean_cells(cells):
    """Return each cell as stripped, single-line text."""
    return [str(cell).strip().replace("\n", " ") for cell in cells]


def _row(parts):
    return "| " + " | ".join(parts) + " |"


def format_header_row(cells):
    """Render a header row: cells uppercased."""
    return _row(text.upper() for text in _clean_cells(cells))


def format_body_row(cells):
    """Render a body row: cells as they are."""
    return _row(_clean_cells(cells))


def format_footer_row(cells):
    """Render a footer row: cells wrapped in asterisks."""
    return _row(f"*{text}*" for text in _clean_cells(cells))
