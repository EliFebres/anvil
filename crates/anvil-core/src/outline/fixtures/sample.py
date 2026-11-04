"""An inventory, used as an outline fixture."""

import json
from pathlib import Path


class Inventory:
    """Items and how many of each are in stock."""

    def __init__(self):
        self.items = {}

    @property
    def total(self):
        """Every item counted."""
        return sum(self.items.values())

    def add(self, name, count=1):
        """Adds `count` of `name`."""
        self.items[name] = self.items.get(name, 0) + count


def load(path: Path) -> Inventory:
    """Reads an inventory from a JSON file.

    The file maps names to counts.
    """
    inventory = Inventory()
    for name, count in json.loads(path.read_text()).items():
        inventory.add(name, count)
    return inventory
