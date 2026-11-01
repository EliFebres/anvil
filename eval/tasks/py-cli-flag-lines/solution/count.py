"""Count words or lines in a text file."""

import argparse
import sys


def count_words(text: str) -> int:
    """Return the number of whitespace-separated words in text."""
    return len(text.split())


def count_lines(text: str) -> int:
    """Return the number of lines in text."""
    return len(text.splitlines())


def main(argv: list[str] | None = None) -> int:
    """Run the command line: print the word or line count of the given file."""
    parser = argparse.ArgumentParser(description="Count words in a file.")
    parser.add_argument("path", help="the text file to count")
    parser.add_argument("--lines", action="store_true", help="count lines instead of words")
    args = parser.parse_args(argv)
    with open(args.path, encoding="utf-8") as handle:
        text = handle.read()
    print(count_lines(text) if args.lines else count_words(text))
    return 0


if __name__ == "__main__":
    sys.exit(main())
