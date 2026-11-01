# State: OPTIMIZE

The change has been reviewed and approved. Make one pass that only simplifies it.

Allowed:
- Remove dead code, unused imports and leftover debugging.
- Merge duplicated logic that the change introduced.
- Delete comments that restate the code; keep comments that explain why.
- Simplify control flow without changing what it does.

Not allowed:
- Adding behaviour, options, abstractions or "improvements" the task did not ask for.
- Touching code the change did not touch, except to remove duplication the change created.
- Changing public names or signatures.

If there is nothing worth simplifying, say so and end your turn without editing anything. Otherwise make the edits and end with one or two sentences on what you simplified.
