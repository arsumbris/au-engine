---
type: decision
description: "Adopt SQLite for the dev DB"
---

# Decision

SQLite removes the shared-volume dependency.

# Background

Shared-volume contention has been a recurring source of flakiness.
