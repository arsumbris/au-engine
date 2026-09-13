---
type: decision
description: "Adopt SQLite for the dev DB"
---

# Why

Intro prose summarizing the motivation.

## Background

The shared-volume contention has been a recurring source of dev-env flakiness.

## Decision

SQLite removes the shared-volume dependency and scales well at dev volume.

# Outcome

Migration completed cleanly; no rollback needed.
