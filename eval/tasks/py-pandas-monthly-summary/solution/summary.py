"""Sum orders per month."""

import pandas as pd


def main() -> None:
    """Read orders.csv and write monthly.csv with one total per month."""
    orders = pd.read_csv("orders.csv")
    orders["month"] = pd.to_datetime(orders["date"]).dt.strftime("%Y-%m")
    monthly = orders.groupby("month", as_index=False)["amount"].sum()
    monthly = monthly.rename(columns={"amount": "total"}).sort_values("month")
    monthly.to_csv("monthly.csv", index=False)


if __name__ == "__main__":
    main()
