from inventory import Item, find, restock, total_value


def test_restock_and_find():
    items = [Item("bolt", 1, 2.0)]
    restock(items[0], 2)
    assert find(items, "bolt").count == 3
    assert find(items, "nut") is None
    assert total_value(items) == 6.0
