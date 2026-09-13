---
type: note
description: "Author left a fenced block open by mistake"
---

# Notes

Some intro prose.

```yaml
key: value
nested:
  - item

Even though the fence above never closes, the `# Notes` heading at the
top should still be detected by the parser, and `body-unterminated-fence`
should fire as a warning rather than the whole body silently dropping.
