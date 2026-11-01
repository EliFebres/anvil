"""Count words in a text file."""

import argparse
import sys


def count_words(text: str) -> int:
    """Return the number of whitespace-separated words in text."""
    return len(text.split())


def main(argv: list[str] | None = None) -> int:
    """Run the command line: print the word count of the given file."""
    parser = argparse.ArgumentParser(description="Count words in a file.")
    parser.add_argument("path", help="the text file to count")
    args = parser.parse_args(argv)
    with open(args.path, encoding="utf-8") as handle:
        text = handle.read()
    print(count_words(text))
    return 0


if __name__ == "__main__":
    sys.exit(main())
