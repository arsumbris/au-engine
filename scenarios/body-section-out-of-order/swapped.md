---
type: decision
description: "Migrate dev DB to SQLite"
---

# Outcome

Migration completed cleanly; no rollback needed.

# Why

The dev DB creaks under shared-volume contention.
