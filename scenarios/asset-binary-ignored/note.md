---
type: note
description: A note that sits alongside a PDF binary in the same repo.
---

The repo contains `media/book.pdf`. The walker lists it (so `RepoIndex`
can resolve `file*` references), but the parser never reads its bytes.
