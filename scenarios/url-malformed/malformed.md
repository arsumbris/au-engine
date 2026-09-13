---
type: citation
source: "https://exa mple.com"
---

# Malformed URL

`^https?://` matches → the value commits to the `Url` branch.
Url validation then fails on the space in the authority, and the value
does NOT silently fall through to the `String` branch.
