---
# `decision` is in the type graph but NOT in this instance's closure
# (instance claims journal-entry, whose closure is {journal-entry}).
# → qualifier-not-in-closure.
# Bare `title` satisfies journal-entry's required slot; the qualified
# field name is `description`, distinct from `title`, so no
# mixed-bare-and-qualified surfaces here.
type: journal-entry
title: "x"
description{decision}: "y"
---
