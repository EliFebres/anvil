"""Parse durations like 30s, 90m, 2h into seconds."""

UNITS = {"s": 1, "m": 60, "h": 3600}


def parse_duration(text: str) -> int:
    """Return the number of seconds text denotes; a bare number is seconds."""
    text = text.strip()
    if text.isdigit():
        return int(text)
    if text.endswith("m"):
        return int(text[:-1])
    unit = text[-1]
    return int(text[:-1]) * UNITS[unit]
