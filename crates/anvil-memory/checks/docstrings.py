"""Reports public items without a docstring in the given .py and .rs files.

Usage: python docstrings.py FILE...

Prints one line per undocumented public item as ``path:line: message`` and
exits 1 when anything was printed, 0 otherwise. Files with other extensions
and files that cannot be read are skipped. The anvil ``google-docstrings``
rule runs this on the files a session changed.
"""

import ast
import re
import sys

RUST_ITEM = re.compile(
    r"^\s*pub(?:\([^)]*\))?\s+"
    r"(?:(?:unsafe|async|const|extern\s+\"[^\"]*\")\s+)*"
    r"(fn|struct|enum|trait|type|const|static|mod|union)\s+"
    r"([A-Za-z_][A-Za-z0-9_]*)"
)


def check_python(path, source):
    """Finds public functions and classes without a docstring.

    Args:
        path: The file name, used in parse errors.
        source: The file contents.

    Returns:
        A list of ``(line, message)`` pairs for module-level and class-level
        items whose name does not start with an underscore.
    """
    tree = ast.parse(source, filename=path)
    reports = []
    scopes = [tree.body]
    while scopes:
        body = scopes.pop()
        for node in body:
            if not isinstance(
                node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)
            ):
                continue
            if node.name.startswith("_"):
                continue
            kind = "class" if isinstance(node, ast.ClassDef) else "function"
            if ast.get_docstring(node) is None:
                reports.append(
                    (node.lineno, f"public {kind} `{node.name}` has no docstring")
                )
            if isinstance(node, ast.ClassDef):
                scopes.append(node.body)
    return reports


def check_rust(path, source):
    """Finds ``pub`` items without a ``///`` doc comment.

    Args:
        path: The file name, unused but kept for symmetry with check_python.
        source: The file contents.

    Returns:
        A list of ``(line, message)`` pairs.
    """
    del path
    lines = source.splitlines()
    reports = []
    for index, line in enumerate(lines):
        match = RUST_ITEM.match(line)
        if not match or is_documented(lines, index):
            continue
        kind, name = match.groups()
        reports.append((index + 1, f"public {kind} `{name}` has no doc comment"))
    return reports


def is_documented(lines, index):
    """Whether the nearest line above an item, skipping attributes, is a doc comment.

    Args:
        lines: Every line of the file.
        index: The item's zero-based line.

    Returns:
        True when a ``///`` or ``#[doc`` line precedes the item.
    """
    probe = index - 1
    while probe >= 0:
        text = lines[probe].strip()
        if text.startswith("#[") and not text.startswith("#[doc"):
            probe -= 1
            continue
        return text.startswith("///") or text.startswith("#[doc")
    return False


def main(argv):
    """Checks every file given and prints what is missing.

    Args:
        argv: The file paths.

    Returns:
        The process exit code: 1 when anything was reported, 0 otherwise.
    """
    reports = []
    for path in argv:
        try:
            with open(path, encoding="utf-8") as handle:
                source = handle.read()
        except (OSError, UnicodeDecodeError):
            continue
        if path.endswith(".py"):
            try:
                found = check_python(path, source)
            except SyntaxError as error:
                found = [(error.lineno or 1, f"cannot parse: {error.msg}")]
        elif path.endswith(".rs"):
            found = check_rust(path, source)
        else:
            continue
        for line, message in sorted(found):
            reports.append(f"{path}:{line}: {message}")
    for report in reports:
        print(report)
    return 1 if reports else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
