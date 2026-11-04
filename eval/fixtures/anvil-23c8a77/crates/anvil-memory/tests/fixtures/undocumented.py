"""Fixture: public items without docstrings."""


def add(a, b):
    return a + b


class Counter:
    def bump(self):
        self.n = getattr(self, "n", 0) + 1

    def _hidden(self):
        return None
