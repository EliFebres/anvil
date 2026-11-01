# mypy output

```
inventory.py:17: error: Incompatible return value type (got "float", expected "int")  [return-value]
inventory.py:28: error: Argument 2 to "Item" has incompatible type "str"; expected "int"  [arg-type]
inventory.py:29: error: Argument 1 to "restock" has incompatible type "Item | None"; expected "Item"  [arg-type]
inventory.py:29: error: Argument 2 to "restock" has incompatible type "str"; expected "int"  [arg-type]
```
