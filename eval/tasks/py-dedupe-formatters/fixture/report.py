"""Render table rows."""


def format_header_row(cells):
    """Render a header row: cells uppercased."""
    parts = []
    for cell in cells:
        text = str(cell).strip().replace("\n", " ")
        parts.append(text.upper())
    return "| " + " | ".join(parts) + " |"


def format_body_row(cells):
    """Render a body row: cells as they are."""
    parts = []
    for cell in cells:
        text = str(cell).strip().replace("\n", " ")
        parts.append(text)
    return "| " + " | ".join(parts) + " |"


def format_footer_row(cells):
    """Render a footer row: cells wrapped in asterisks."""
    parts = []
    for cell in cells:
        text = str(cell).strip().replace("\n", " ")
        parts.append(f"*{text}*")
    return "| " + " | ".join(parts) + " |"
