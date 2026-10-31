"""Metric series the dashboard shows. Pure Python so it can be tested without Streamlit."""

METRICS = {
    "revenue": [120.0, 135.5, 150.25],
    "active_users": [40, 42, 47],
    "churn_percent": [2.1, 1.9, 1.7],
}


def latest(name: str) -> float:
    """Return the most recent value of the named metric."""
    return METRICS[name][-1]


def names() -> list[str]:
    """Return the metric names in display order."""
    return list(METRICS)
