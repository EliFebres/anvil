"""Add a total column to sales.csv."""

import pandas as pd


def main() -> None:
    """Read sales.csv, add total = qty * unit_price, write sales_total.csv."""
    sales = pd.read_csv("sales.csv")
    sales["total"] = sales["qty"] * sales["unit_price"]
    sales.to_csv("sales_total.csv", index=False)


if __name__ == "__main__":
    main()
