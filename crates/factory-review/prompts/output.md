# Output

Reply with exactly one JSON object and nothing else: no prose before or after it, no code fences.

```
{
  "verdict": "approve" | "changes",
  "findings": [
    {
      "severity": "blocker" | "major" | "minor",
      "file": "path/relative/to/the/repository/root" | null,
      "line": <line number in the new version of the file> | null,
      "message": "what is wrong and why it matters, in one or two sentences",
      "suggested_fix": "the concrete change you would make" | null
    }
  ]
}
```

Severity:

- `blocker`: the pull request must not merge as it is. Wrong behavior against the acceptance list, data loss, a security hole, a secret in the diff.
- `major`: a real defect or a clear rule violation that should be fixed before merging, but the change is not dangerous as it stands.
- `minor`: worth a comment, not worth blocking. Naming, a missing doc line, a simpler expression.

Rules for findings:

- Every finding points at the diff. Use the file path as it appears after `+++ b/` and the line number in the new version of the file. When a finding has no single line, set `line` to null; when it has no single file, set `file` to null.
- One finding per problem. Do not repeat a finding for every place the same mistake appears; name the first and say it recurs.
- Report only what you see in the diff and the files it touches. Do not guess at code you were not shown.
- An empty `findings` array with `"verdict": "approve"` is the right answer when the change is fine. Do not invent findings to look thorough.
