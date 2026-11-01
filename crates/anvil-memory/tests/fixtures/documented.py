"""Fixture: every public item has a docstring."""


def add(a, b):
    """Adds two numbers.

    Args:
        a: First.
        b: Second.

    Returns:
        The sum.
    """
    return a + b


class Counter:
    """Counts things."""

    def bump(self):
        """Increments the count."""
        self.n = getattr(self, "n", 0) + 1

    def _hidden(self):
        return None
