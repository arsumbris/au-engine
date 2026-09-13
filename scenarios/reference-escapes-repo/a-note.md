---
type: note
# Climbs out of the repo: repo-scoped by construction, so it can never resolve.
# The fix is the `::repo` qualifier, not a relative path.
sibling: "[[../peer-repo/notes/x.md]]"
# Merely absent, and open-world: it may be authored next.
later: "[[not-written-yet]]"
# The case that motivated the code, in the slot a consumer actually shipped it
# in. A `*@` slot softens a live-counterpart failure into `pinned-reference-
# drifted`, because such an edge is EXPECTED to move. An impossible address is
# not that: no commit makes it resolve, so it must survive un-softened.
touched: "[[../peer-repo/notes/x.md::@0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c]]"
---

A prose link out of the repo is the same impossible address: [[../peer-repo/notes/x.md]].
