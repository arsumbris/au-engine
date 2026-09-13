---
type: decision.decided
description: "Migrate dev DB to SQLite"
status: made
---

# Why

Shared-volume contention had become the dominant source of dev-env flakiness.

# How

Switched the dev pipeline to a SQLite-backed driver behind the existing port.

# Outcome

Migration completed cleanly; no rollback needed.
