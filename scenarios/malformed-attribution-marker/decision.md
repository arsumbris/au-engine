---
type: decision
description: "Migrate dev DB to SQLite"
---

A bracket-and-colon start with whitespace name: `[: ]`.

An empty marker: `[:]`.

Stray leading whitespace inside brackets: `[: decided_at] 2026-04-15`.

Marker without a value following: `[:decided_at]`.

Each of the four forms above is malformed.
