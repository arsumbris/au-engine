---
type: decision
description: "Migrate dev DB to SQLite"
---

# Why

The dev DB creaks under shared-volume contention.

# Assumptions

The shared-volume bottleneck is the dominant cause, not application-layer locking.
