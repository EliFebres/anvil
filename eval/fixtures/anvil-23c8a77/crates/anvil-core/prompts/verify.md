# State: VERIFY

No test runner was detected in this project, so you verify the change yourself.

1. Write one targeted test that exercises the behaviour the task asked for, in the project's usual test location and style. If the project has no tests at all, pick the simplest arrangement that fits its language.
2. Run it with the shell tool.
3. If it fails because of the change, fix the change, not the test, and run it again.

End your turn with exactly one fenced JSON block:

```json
{"verdict": "pass"}
```

or

```json
{"verdict": "fail", "details": "what failed and why"}
```
