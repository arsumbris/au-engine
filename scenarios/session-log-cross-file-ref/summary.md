---
type: note
description: "A content note referencing a captured session event"
evidence:
  - "[[s-001^^e-prompt]]"
---

# Capture flow

The session log lives nested under `operations/`, far from this note.
Its events are inline records carrying `^:` ids.
The `evidence` slot reaches one cross-file by bare name: the extensionless
target matches the yaml file's stem, and the record's claim
(`sessionEvent.userPrompt`) satisfies `sessionEvent&[]` through the
sealed family. Prose like [[s-001^e-tool]] stays navigational.
