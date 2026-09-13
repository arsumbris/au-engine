//! The operation catalog: the writes a consumer actually performs, as named,
//! repeatable units over a workspace.
//!
//! Where [`crate::repogen`] answers "how big is the knowledge base", this
//! answers "what did someone just do to it". The two compose: generate a
//! workspace once, then drive each operation against it.
//!
//! The operations take [`Targets`], not a generator handle, so the same list
//! runs against a REAL workspace. That matters because a generated corpus does
//! not reproduce a real one's cost shape.
//!
//! Two consumers, deliberately sharing one list.
//! - a timing harness, which measures each operation end to end.
//! - the gate, which asserts each operation still takes the rebuild path it
//!   takes today.
//!
//! The second is the durable one. A wall-clock assertion is machine-dependent,
//! so it flakes and then gets muted. WHICH PATH a write takes is machine
//! independent, and it is the fact that decides whether a write costs
//! microseconds or a whole rebuild.
//!
//! No `au-engine` dependency, so the crate graph stays acyclic. An operation is
//! filesystem work plus a declared expectation; the engine-side meaning of
//! [`RebuildPath`] is mapped by the caller.

use std::path::{Path, PathBuf};

use crate::repogen::GenOutput;

/// The files an operation acts on.
///
/// Deliberately not [`GenOutput`]: the catalog is operations over A WORKSPACE,
/// not over a generated one. A real on-disk workspace supplies the same handles
/// from its own catalog, which is what lets the harness measure a corpus whose
/// cost shape a generator does not reproduce.
pub struct Targets {
    /// The entry folder-repo.
    pub entry: PathBuf,
    /// Instance files, the edit and delete targets.
    pub instances: Vec<PathBuf>,
    /// Type-def files, the vocabulary-edit targets.
    pub type_defs: Vec<PathBuf>,
}

impl Targets {
    /// Targets from a generated workspace.
    pub fn from_gen(gen: &GenOutput) -> Self {
        Targets {
            entry: gen.entry.clone(),
            instances: gen.instances.clone(),
            type_defs: gen.type_defs.clone(),
        }
    }

    /// Targets supplied directly, for a real workspace whose file set the
    /// caller already knows (the engine's own catalog, say).
    pub fn new(entry: PathBuf, instances: Vec<PathBuf>, type_defs: Vec<PathBuf>) -> Self {
        Targets {
            entry,
            instances,
            type_defs,
        }
    }
}

/// The rebuild path a write drives the engine down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildPath {
    /// The incremental fast path: the write's blast radius is spliced into the
    /// held knowledge base.
    Incremental,
    /// The whole-knowledge-base rebuild. Correct, and orders of magnitude more
    /// expensive than the write that triggered it.
    Full,
}

/// What an applied operation touched, and how to put it back.
pub struct Applied {
    /// The paths the operation created, changed, or removed. What a subscriber
    /// waits to hear about.
    pub touched: Vec<PathBuf>,
    /// Restores the workspace to its pre-operation state, so one generated
    /// workspace serves the whole catalog without cross-contamination.
    pub undo: Undo,
}

/// The inverse of an operation. An enum rather than a boxed closure, so an undo
/// is inspectable and cheap to reason about.
pub enum Undo {
    /// Delete these paths, undoing an add.
    Remove(Vec<PathBuf>),
    /// Rewrite these paths with their original bytes, undoing an edit or a delete.
    Restore(Vec<(PathBuf, Vec<u8>)>),
    /// Move back, undoing a rename.
    Rename { from: PathBuf, to: PathBuf },
    /// The operation already left the workspace as it found it.
    ///
    /// The case is an operation over a file its own `prepare` created: removing
    /// it restores the original state, so there is nothing to undo.
    Nothing,
}

impl Undo {
    /// Whether running this changes nothing on disk.
    ///
    /// A caller that waits for the engine to absorb each of its writes needs to
    /// know which ones produce no write at all, since waiting on those would
    /// block until a timeout.
    pub fn is_noop(&self) -> bool {
        matches!(self, Undo::Nothing)
    }

    /// Apply the inverse. Best-effort per path: a harness runs this between
    /// measurements, where a partial restore must not mask the next result.
    pub fn run(self) -> std::io::Result<()> {
        match self {
            Undo::Remove(paths) => {
                for p in paths {
                    std::fs::remove_file(&p)?;
                }
            }
            Undo::Restore(entries) => {
                for (p, bytes) in entries {
                    if let Some(parent) = p.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&p, bytes)?;
                }
            }
            Undo::Rename { from, to } => std::fs::rename(&from, &to)?,
            Undo::Nothing => {}
        }
        Ok(())
    }
}

/// One named write, plus the path it drives the engine down.
pub struct Operation {
    /// Stable identifier, used as the harness's row label and the gate's case name.
    pub name: &'static str,
    /// One line on what the operation does and why it is in the catalog.
    pub what: &'static str,
    /// The path this operation takes TODAY.
    ///
    /// Pinned by the gate, so a change to it is deliberate rather than
    /// discovered later in a latency report. Some entries pin behaviour that is
    /// known-undesirable, see [`Operation::wanted`].
    pub today: RebuildPath,
    /// The path this operation SHOULD take, when that differs from `today`.
    ///
    /// `Some` marks a known gap, so the catalog carries the discrepancy rather
    /// than the gate silently blessing it. Closing a gap means flipping `today`
    /// and clearing this in the same commit.
    pub wanted: Option<RebuildPath>,
    /// Optional setup, run and SETTLED before the measurement starts.
    ///
    /// An operation over a file that must already exist, a delete, needs that
    /// file created first. Doing it inside `apply` would fold the creation into
    /// the measurement, so the row would time an add and call it a delete.
    ///
    /// Returns the paths it created, so a caller that must wait for the setup to
    /// be absorbed knows what to wait FOR. The prepare is the only thing that
    /// knows, so it says; a caller-side list of seeded filenames is a second
    /// source of truth, and it silently rots the moment a second seeding
    /// operation appears.
    pub prepare: Option<fn(&Targets) -> std::io::Result<Vec<PathBuf>>>,
    /// Perform the write. The measured window.
    pub apply: fn(&Targets) -> std::io::Result<Applied>,
}

impl Operation {
    /// Whether this operation's current path is known to be the wrong one.
    pub fn is_known_gap(&self) -> bool {
        matches!(self.wanted, Some(w) if w != self.today)
    }
}

/// The catalog.
///
/// Ordered cheapest-expected first, so a harness run degrades informatively if
/// it is cut short.
///
/// **There is deliberately no `edit-asset`.** Overwriting an asset is
/// unobservable to the engine: its bytes are never decoded, so the write
/// produces no version advance and no event. Every operation here is measured as
/// write-to-observable latency, and an operation with nothing to observe has no
/// latency to measure, so it would only ever report a timeout. The behaviour is
/// pinned by unit test instead, in au-engine: `an_asset_content_edit_advances_nothing`
/// for the version, `parity_an_asset_edit_changes_nothing` for the state.
pub fn catalog() -> Vec<Operation> {
    vec![
        Operation {
            name: "edit-instance-body",
            what: "append a line to an existing typed instance, the common write loop",
            today: RebuildPath::Incremental,
            wanted: None,
            prepare: None,
            apply: edit_instance_body,
        },
        Operation {
            name: "add-typed-instance",
            what: "write a new instance claiming an existing type",
            today: RebuildPath::Incremental,
            wanted: None,
            prepare: None,
            apply: add_typed_instance,
        },
        Operation {
            name: "delete-instance",
            what: "remove an existing typed instance",
            today: RebuildPath::Incremental,
            wanted: None,
            prepare: None,
            apply: delete_instance,
        },
        Operation {
            name: "rename-instance",
            what: "rename an existing typed instance, a delete plus an add in one dirty set",
            today: RebuildPath::Incremental,
            wanted: None,
            prepare: None,
            apply: rename_instance,
        },
        Operation {
            name: "bulk-edit-instances",
            what: "edit sixteen instances at once, the agent-writing-into-a-pane case",
            today: RebuildPath::Incremental,
            wanted: None,
            prepare: None,
            apply: bulk_edit_instances,
        },
        Operation {
            name: "add-empty-file",
            what: "touch a new empty file, what a file tree does when you create a note",
            // An empty `.md` splits to a note with an empty body: no claim, so
            // no validation and no resolved analysis, but a real path-set change
            // that can resolve a dangling reference. The reported consumer bug,
            // spliced rather than rebuilt.
            today: RebuildPath::Incremental,
            wanted: None,
            prepare: None,
            apply: add_empty_file,
        },
        Operation {
            name: "add-untyped-note",
            what: "write a new prose file with no type claim",
            today: RebuildPath::Incremental,
            wanted: None,
            prepare: None,
            apply: add_untyped_note,
        },
        Operation {
            name: "delete-untyped-note",
            what: "remove an untyped note, the delete-side sibling of the add",
            today: RebuildPath::Incremental,
            wanted: None,
            prepare: Some(seed_untyped_note),
            apply: delete_untyped_note,
        },
        Operation {
            name: "add-asset",
            what: "drop in a file the engine catalogues but never decodes, an image say",
            // Its whole contribution is a path-set membership: it enters the
            // reference index and resolves `file*` references. No parse, no
            // diagnostics, no outgoing edges.
            today: RebuildPath::Incremental,
            wanted: None,
            prepare: None,
            apply: add_asset,
        },
        Operation {
            name: "delete-asset",
            what: "remove an asset, so a `file*` reference to it starts dangling",
            today: RebuildPath::Incremental,
            wanted: None,
            prepare: Some(seed_asset),
            apply: delete_asset,
        },
        Operation {
            name: "edit-typedef",
            what: "add an optional field to a type-def, changing the vocabulary",
            // A graph change: every instance's effective shape can move, so the
            // whole rebuild is correct here, not a gap.
            today: RebuildPath::Full,
            wanted: None,
            prepare: None,
            apply: edit_typedef,
        },
    ]
}

// ----- the operations -----

/// The first instance, the default edit target.
fn first_instance(t: &Targets) -> std::io::Result<&PathBuf> {
    t.instances.first().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "generated workspace has no instances",
        )
    })
}

/// A path beside an existing instance, so a new file lands inside a real member
/// rather than at the entry root.
fn beside(anchor: &Path, name: &str) -> PathBuf {
    anchor
        .parent()
        .map(|d| d.join(name))
        .unwrap_or_else(|| PathBuf::from(name))
}

fn edit_instance_body(t: &Targets) -> std::io::Result<Applied> {
    let path = first_instance(t)?;
    let original = std::fs::read(path)?;
    let mut next = original.clone();
    next.extend_from_slice(b"\nan appended line.\n");
    std::fs::write(path, &next)?;
    Ok(Applied {
        touched: vec![path.clone()],
        undo: Undo::Restore(vec![(path.clone(), original)]),
    })
}

fn add_typed_instance(t: &Targets) -> std::io::Result<Applied> {
    let anchor = first_instance(t)?;
    let path = beside(anchor, "op-added-typed.md");
    // `t0` is repogen's primary type in every repo, and the anchor's own repo
    // owns it, so a bare claim resolves.
    std::fs::write(
        &path,
        "---\ntype: t0\ntitle: Added\n---\n# Added\n\nbody.\n",
    )?;
    Ok(Applied {
        touched: vec![path.clone()],
        undo: Undo::Remove(vec![path]),
    })
}

fn delete_instance(t: &Targets) -> std::io::Result<Applied> {
    let path = first_instance(t)?;
    let original = std::fs::read(path)?;
    std::fs::remove_file(path)?;
    Ok(Applied {
        touched: vec![path.clone()],
        undo: Undo::Restore(vec![(path.clone(), original)]),
    })
}

fn rename_instance(t: &Targets) -> std::io::Result<Applied> {
    let from = first_instance(t)?.clone();
    let to = beside(&from, "op-renamed.md");
    std::fs::rename(&from, &to)?;
    Ok(Applied {
        touched: vec![from.clone(), to.clone()],
        undo: Undo::Rename { from: to, to: from },
    })
}

/// Sixteen at once: enough to show a per-file cost that a single write hides,
/// while staying a plausible agent burst rather than a synthetic flood.
fn bulk_edit_instances(t: &Targets) -> std::io::Result<Applied> {
    let targets: Vec<PathBuf> = t.instances.iter().take(16).cloned().collect();
    if targets.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "generated workspace has no instances",
        ));
    }
    let mut originals = Vec::with_capacity(targets.len());
    for path in &targets {
        let original = std::fs::read(path)?;
        let mut next = original.clone();
        next.extend_from_slice(b"\nbulk appended.\n");
        std::fs::write(path, &next)?;
        originals.push((path.clone(), original));
    }
    Ok(Applied {
        touched: targets,
        undo: Undo::Restore(originals),
    })
}

fn add_empty_file(t: &Targets) -> std::io::Result<Applied> {
    let anchor = first_instance(t)?;
    let path = beside(anchor, "op-added-empty.md");
    std::fs::write(&path, b"")?;
    Ok(Applied {
        touched: vec![path.clone()],
        undo: Undo::Remove(vec![path]),
    })
}

fn add_untyped_note(t: &Targets) -> std::io::Result<Applied> {
    let anchor = first_instance(t)?;
    let path = beside(anchor, "op-added-note.md");
    std::fs::write(&path, "# A note\n\nprose only, no type claim.\n")?;
    Ok(Applied {
        touched: vec![path.clone()],
        undo: Undo::Remove(vec![path]),
    })
}

/// Creates the note the delete operation removes. Run and settled BEFORE the
/// measurement, so the timed window holds the removal alone.
fn seed_untyped_note(t: &Targets) -> std::io::Result<Vec<PathBuf>> {
    let anchor = first_instance(t)?;
    let path = beside(anchor, SEEDED_NOTE);
    std::fs::write(&path, "# Seeded\n\nprose only.\n")?;
    Ok(vec![path])
}

/// The note `seed_untyped_note` creates.
const SEEDED_NOTE: &str = "op-seeded-note.md";

/// Removes the seeded note. Undo is [`Undo::Nothing`]: the file did not exist
/// before `prepare`, so its removal IS the original state.
fn delete_untyped_note(t: &Targets) -> std::io::Result<Applied> {
    let anchor = first_instance(t)?;
    let path = beside(anchor, SEEDED_NOTE);
    std::fs::remove_file(&path)?;
    Ok(Applied {
        touched: vec![path],
        undo: Undo::Nothing,
    })
}

/// A minimal PNG header. The bytes are never decoded, so any non-text content
/// serves; a real magic number keeps the fixture honest about what it stands
/// for.
const ASSET_BYTES: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// The asset `seed_asset` creates and `delete_asset` removes.
const SEEDED_ASSET: &str = "op-seeded-asset.png";

fn add_asset(t: &Targets) -> std::io::Result<Applied> {
    let anchor = first_instance(t)?;
    let path = beside(anchor, "op-added-asset.png");
    std::fs::write(&path, ASSET_BYTES)?;
    Ok(Applied {
        touched: vec![path.clone()],
        undo: Undo::Remove(vec![path]),
    })
}

/// Creates the asset the delete operation removes. Run and settled BEFORE the
/// measurement, so the timed window holds the removal alone.
fn seed_asset(t: &Targets) -> std::io::Result<Vec<PathBuf>> {
    let anchor = first_instance(t)?;
    let path = beside(anchor, SEEDED_ASSET);
    std::fs::write(&path, ASSET_BYTES)?;
    Ok(vec![path])
}

/// Removes the seeded asset. Undo is [`Undo::Nothing`]: the file did not exist
/// before `prepare`, so its removal IS the original state.
fn delete_asset(t: &Targets) -> std::io::Result<Applied> {
    let anchor = first_instance(t)?;
    let path = beside(anchor, SEEDED_ASSET);
    std::fs::remove_file(&path)?;
    Ok(Applied {
        touched: vec![path],
        undo: Undo::Nothing,
    })
}

fn edit_typedef(t: &Targets) -> std::io::Result<Applied> {
    let path = t.type_defs.first().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "generated workspace has no type-defs",
        )
    })?;
    let original = std::fs::read(path)?;
    let mut next = original.clone();
    // Optional, so no existing instance is invalidated: the operation measures
    // the rebuild a vocabulary change forces, not a diagnostic storm.
    next.extend_from_slice(b"  opAdded?: String\n");
    std::fs::write(path, &next)?;
    Ok(Applied {
        touched: vec![path.clone()],
        undo: Undo::Restore(vec![(path.clone(), original)]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_operation_is_named_once() {
        let cat = catalog();
        let mut names: Vec<&str> = cat.iter().map(|o| o.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "operation names must be unique");
    }

    #[test]
    fn no_operation_is_a_known_gap() {
        let cat = catalog();
        let gaps: Vec<&str> = cat
            .iter()
            .filter(|o| o.is_known_gap())
            .map(|o| o.name)
            .collect();
        // Every catalogued write now takes the path it should. The three
        // non-instance writes were gaps until the fast path stopped requiring a
        // typed instance; `edit-typedef` is Full and correct, since a vocabulary
        // change can move every instance's effective shape.
        //
        // A gap reappearing here means an operation was added whose behaviour is
        // known-wrong, or that one regressed. Either is worth seeing, so this
        // asserts the SET rather than merely that the flipped three are clean.
        assert_eq!(gaps, Vec::<&str>::new());
    }

    #[test]
    fn a_wanted_path_never_merely_restates_today() {
        for op in catalog() {
            if let Some(wanted) = op.wanted {
                assert_ne!(
                    wanted, op.today,
                    "{}: `wanted` is for a real discrepancy, drop it when it matches",
                    op.name
                );
            }
        }
    }
}
