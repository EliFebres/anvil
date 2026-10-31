"""Turn a title into a URL slug."""

import re


def slugify(text: str) -> str:
    """Lowercase text and replace every run of non-alphanumerics with one hyphen."""
    text = text.lower()
    return re.sub(r"[^a-z0-9]+", "-", text).strip("-")
