"""A tiny todo list kept in todo.json."""

import argparse
import json
import sys
from pathlib import Path

STORE = Path("todo.json")


def load() -> list[dict]:
    """Return the stored items, or an empty list."""
    if not STORE.exists():
        return []
    return json.loads(STORE.read_text(encoding="utf-8"))


def save(items: list[dict]) -> None:
    """Write the items back to the store."""
    STORE.write_text(json.dumps(items, indent=2), encoding="utf-8")


def cmd_add(args: argparse.Namespace) -> int:
    """Append an item."""
    items = load()
    items.append({"text": args.text, "done": False})
    save(items)
    return 0


def cmd_list(args: argparse.Namespace) -> int:
    """Print every item as `N [ ] text`, N counting from 1."""
    for number, item in enumerate(load(), start=1):
        mark = "x" if item["done"] else " "
        print(f"{number} [{mark}] {item['text']}")
    return 0


def main(argv: list[str] | None = None) -> int:
    """Run the command line."""
    parser = argparse.ArgumentParser(prog="todo")
    sub = parser.add_subparsers(dest="command", required=True)
    add = sub.add_parser("add", help="add an item")
    add.add_argument("text")
    add.set_defaults(func=cmd_add)
    lst = sub.add_parser("list", help="list items")
    lst.set_defaults(func=cmd_list)
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
