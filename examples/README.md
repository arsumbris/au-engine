# Examples

Standalone demos, not part of the `au` binary.

## watch.py

A live view of a repo's subscription stream.

It is a reference external consumer of the daemon wire.
It speaks the framing by hand, the way an editor extension or other outside client would.
It does not use the engine's own Rust client, so it proves the wire is consumable from outside.

Run:
```sh
au daemon start <repo>                       # in another terminal
python3 examples/watch.py <repo> [channel]   # channel defaults to "changes"
```

Channels: `changes`, `lifecycle`, `types`, `type_graph`, `link_graph`, `files`, `diagnostics`.
For `diagnostics`, pass the file:
```sh
python3 examples/watch.py <repo> diagnostics notes/a.md
```

Edit a file in the repo.
The change events print live.

Pure stdlib, no dependencies.
The wire it implements is `crates/au-engine/WIRE.md`.
