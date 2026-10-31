"""A small stock model."""

from dataclasses import dataclass
from typing import Optional


@dataclass
class Item:
    name: str
    count: int
    price: float


def restock(item: Item, count: int) -> None:
    """Add count units to item."""
    item.count += count


def total_value(items: list[Item]) -> float:
    """Return the value of all stock."""
    return sum(item.count * item.price for item in items)


def find(items: list[Item], name: str) -> Optional[Item]:
    """Return the item called name, or None."""
    for item in items:
        if item.name == name:
            return item
    return None


def main() -> None:
    """Print the value of the sample stock after a restock."""
    items = [Item("bolt", 10, 0.25), Item("nut", 5, 0.10)]
    bolt = find(items, "bolt")
    if bolt is None:
        raise KeyError("bolt")
    restock(bolt, 3)
    print(f"{total_value(items):.2f}")
