import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "app"))

from metrics import latest, names  # noqa: E402


def test_latest_and_names():
    assert latest("revenue") == 150.25
    assert names() == ["revenue", "active_users", "churn_percent"]
