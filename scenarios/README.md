# scenarios/

Curated narrative fixtures — each tells one spec story in 1-5 files.

Most slugs are narrow (one diagnostic code or one feature). A few
`corpus-*` slugs are intentionally broad — they preserve coverage
that's better seen together than split across many narrow fixtures
(load-time grammar, references, mixin/prefix machinery, inline
values, candidate enumeration, meta-body validation).

## Layout

```
scenarios/
  <slug>/
    scenario.yaml         # required — title, command, expected
    type/<name>.type.yaml # zero or more type-defs
    <whatever>.md         # zero or more instance files
```

Slugs are descriptive (e.g. `url-clean`, `sealed-family`) — no numeric prefix.

## scenario.yaml shape

```yaml
title: "Human-readable one-liner"
spec_ref: "[[type-def sealed::au-type-system]]"   # atom(s) the scenario illustrates
command: validate           # validate | graph | candidates | candidate_counts | subtypes | types
base: note                  # for `command: subtypes` only — the base type
repo: app                   # optional, for `command: types` — scope to one member
expected:
  clean: true               # OR
  codes:                    # for validate / graph
    - { code: required-field-absent, severity: error }
    - { code: ...,                   severity: warning }
  candidates:               # for `command: candidates` only
    - { type_name: deliverable }
```

`clean: true`, `codes: [...]`, and `candidates: [...]` are mutually exclusive.
For `command: candidates`, `clean: true` means the scan produced no candidates.

## Adding a scenario

1. New directory `scenarios/<slug>/`.
2. Drop a `scenario.yaml`, type-defs under `type/`, and any instance files.
3. The scenario is the source-of-truth — keep it small enough to read in
   one screen.
