---
type: event
payload:
  type: session-log
  count: 42
---

The `payload` slot is `any`, the interpreted top. The engine reads the value:
the nested `type: session-log` IS an inline claim, so a claim on a type the
graph does not define fires `unknown-type-claim`. Contrast the `opaque-payload`
scenario, where the same nested `type:` is inert.
