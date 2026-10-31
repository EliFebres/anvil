# Bug

`parse_duration("90m")` returns `90`. Ninety minutes is `5400` seconds.

First add a test to `tests/test_duration.py` that fails because of this bug, then fix `duration.py` so the whole suite passes.
