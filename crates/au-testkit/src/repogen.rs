//! Seeded deterministic cross-repo knowledge base generator for scale benchmarks and
//! correctness fuzzing.
//!
//! Emits a FOLDER-REPO workspace: a content-free entry repo whose
//! `.arsumbris/workspace.yaml` lists itself plus each member in `edit`, and
//! `repo_count` co-present member subdirs resolved by their `.arsumbris/repo.yaml`
//! markers. A fraction of the members (`xref_density`) import `repo0`'s primary
//! type (`deps:` + `type: t0::repo0`) and reference its instances, the realistic
//! cross-repo compose site. Each repo carries a parent CHAIN of `closure_depth`
//! base types, so a claimed leaf folds the whole chain into its closure id (the
//! `DeepChain` profile stresses the closure-id recursion depth).
//!
//! Deterministic: all content is a pure function of `(knobs, seed)` via a tiny
//! explicit PRNG, so the same inputs write a byte-identical tree. No `rand`, no
//! clock (the engine forbids both, and byte-identity is the property under test).
//!
//! Format knowledge lives here as string templates, so au-testkit needs no
//! au-engine dependency (the crate graph stays acyclic).

use std::path::{Path, PathBuf};

/// A splitmix64 PRNG: seeded, deterministic, no external crate. Used only to make
/// seed-varying SELECTIONS (which members import), never for content bytes, which
/// derive from indices, so output stays byte-identical per `(knobs, seed)`.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A deterministic fraction in `[0, 1)`.
    fn ratio(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A named point in the knob space. Each maps to a [`Knobs`] preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Tiny, for a fast correctness gate.
    Small,
    /// Mid-size, the default fuzzer target.
    Medium,
    /// Wide and deep, an opt-in stressor.
    Large,
    /// Deep and narrow: a long parent chain at low width, to exercise the
    /// closure-id recursion-depth boundary.
    DeepChain,
    /// A realistic median host+harness workspace: dozens of repos (base types,
    /// projections, plugins, a few edit repos) with the instance bulk spread
    /// across them. Models the repo count real usage sits at, well past the old
    /// profiles, so repo-count-scaling costs (`repo_of`, composition assembly)
    /// are exercised at a representative size.
    Workspace,
    /// The degenerate extreme: a very high repo count, for the repo-count²
    /// composition-assembly stressor and the "sky's the limit" upper bound. Large
    /// enough that the disk guard matters; keep it opt-in.
    WorkspaceExtreme,
}

/// The generator's tunable surface. MVP knobs; cycle-ratio and vendored-closure
/// ratio are deferred (a follow-up).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Knobs {
    /// Number of member repos (`repo0..repo{n-1}`), beside the content-free entry.
    pub repo_count: usize,
    /// Leaf types per repo (`t0..t{m-1}`); `t0` is the claimed / imported primary.
    pub types_per_repo: usize,
    /// Depth of the parent chain (`base0 <- base1 <- ...`) each leaf extends.
    pub closure_depth: usize,
    /// Fraction of members (beyond `repo0`) that import `repo0`'s primary type.
    pub xref_density: f64,
    /// Instances per repo.
    pub instance_count: usize,
    /// Hub in-degree: this many `repo0` instances (`n1..n{hub_fanin}`) each hold a
    /// typed reference to the single hub node `repo0/n0`. `0` = no hub. Dirtying
    /// all of them at once is the O(N^2)-per-hub co-dirtied stressor.
    pub hub_fanin: usize,
    /// Co-typed cohort size: this many EXTRA `repo0` instances (`c0..c{n-1}`) each
    /// claiming the single primary type `t0`, with NO links. `0` = no cohort. They
    /// share the one closure key `(repo0, t0)`, so bulk-retyping them all at once
    /// (adding a `t1` mixin) churns that membership vec — the closure-membership
    /// sibling of `hub_fanin`, isolated from the backlink index because the cohort
    /// is link-free. Assumes `types_per_repo >= 2` so the `t1` retype target exists.
    pub cotype_cohort: usize,
}

impl Profile {
    /// The knob preset for this profile. Initial values; the timing harness tunes.
    pub fn knobs(self) -> Knobs {
        match self {
            Profile::Small => Knobs {
                repo_count: 2,
                types_per_repo: 2,
                closure_depth: 1,
                xref_density: 0.5,
                instance_count: 10,
                hub_fanin: 0,
                cotype_cohort: 0,
            },
            Profile::Medium => Knobs {
                repo_count: 5,
                types_per_repo: 4,
                closure_depth: 3,
                xref_density: 0.4,
                instance_count: 50,
                hub_fanin: 0,
                cotype_cohort: 0,
            },
            Profile::Large => Knobs {
                repo_count: 20,
                types_per_repo: 8,
                closure_depth: 4,
                xref_density: 0.3,
                instance_count: 200,
                hub_fanin: 0,
                cotype_cohort: 0,
            },
            Profile::DeepChain => Knobs {
                repo_count: 2,
                types_per_repo: 1,
                closure_depth: 32,
                xref_density: 0.5,
                instance_count: 20,
                hub_fanin: 0,
                cotype_cohort: 0,
            },
            // ~30 repos × 350 instances ≈ 10.5k instances, moderate cross-repo
            // import density, the representative loaded-workspace size.
            Profile::Workspace => Knobs {
                repo_count: 30,
                types_per_repo: 8,
                closure_depth: 4,
                xref_density: 0.4,
                instance_count: 350,
                hub_fanin: 0,
                cotype_cohort: 0,
            },
            // ~150 repos × 200 instances ≈ 30k instances. The high repo count is
            // the point: composition assembly is O(repos²), so this is its
            // stressor. ~33k files, ~130 MiB with block rounding, hence the guard.
            Profile::WorkspaceExtreme => Knobs {
                repo_count: 150,
                types_per_repo: 12,
                closure_depth: 6,
                xref_density: 0.5,
                instance_count: 200,
                hub_fanin: 0,
                cotype_cohort: 0,
            },
        }
    }
}

/// The generated tree's handles, the seams the harness drives.
#[derive(Debug, Clone, Default)]
pub struct GenOutput {
    /// The entry folder-repo directory, passed straight to `build()`.
    pub entry: PathBuf,
    /// Every generated instance file (`*.md`), across all repos.
    pub instances: Vec<PathBuf>,
    /// Every generated type-def (`*.type.yaml`), across all repos.
    pub type_defs: Vec<PathBuf>,
    /// The importing instances that hold a cross-repo reference, the edit targets
    /// for the incremental-vs-full cross-repo pressure.
    pub xref_referrers: Vec<PathBuf>,
    /// The `repo0` instances that reference the single hub node `repo0/n0` (see
    /// `Knobs::hub_fanin`), the co-dirtied set for the O(N^2)-per-hub measurement.
    pub hub_referrers: Vec<PathBuf>,
    /// The co-typed `repo0` cohort (`c0..c{n-1}`, see `Knobs::cotype_cohort`): the
    /// link-free instances sharing the closure key `(repo0, t0)`, the co-dirtied
    /// set for the closure-membership O(N^2) measurement (bulk-retype them).
    pub cotype_cohort: Vec<PathBuf>,
}

/// Generate a folder-repo knowledge base under `dir` for `profile`. See [`generate`].
/// A rough pre-generation footprint estimate, used by the disk guard and worth
/// printing before a large run.
#[derive(Debug, Clone, Copy)]
pub struct Footprint {
    /// Approximate file count the generation writes.
    pub files: usize,
    /// Approximate disk bytes. The files are tiny, so disk usage is dominated by
    /// per-file block rounding, not content, estimated at a conservative 4 KiB
    /// block.
    pub disk_bytes: u64,
}

/// Estimate the on-disk footprint of generating `knobs`.
///
/// Each repo writes a `repo.yaml`, a `closure_depth` base-type chain, its
/// `types_per_repo` leaf types, and `instance_count` instances, plus the entry's
/// two manifests and repo0's cohort.
pub fn estimate_footprint(knobs: &Knobs) -> Footprint {
    let per_repo = 1 + knobs.closure_depth + knobs.types_per_repo.max(1) + knobs.instance_count;
    let files = knobs.repo_count * per_repo + knobs.cotype_cohort + 2;
    const BLOCK: u64 = 4096;
    Footprint {
        files,
        disk_bytes: files as u64 * BLOCK,
    }
}

/// The nearest existing ancestor of `path`, the filesystem the generation will
/// write into. `None` only if nothing on the path exists.
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut cur = Some(path);
    while let Some(p) = cur {
        if p.exists() {
            return Some(p.to_path_buf());
        }
        cur = p.parent();
    }
    None
}

/// Refuse a generation that would not fit, so a large profile never fills a
/// storage-limited device.
///
/// Fails OPEN: if free space cannot be read, generation proceeds. The guard can
/// only PREVENT an overflow, never spuriously block. Keeps a 2× margin over the
/// block-rounded estimate for the OS, transient files, and estimate slack.
pub fn check_disk_space(dir: &Path, knobs: &Knobs) -> Result<(), String> {
    let fp = estimate_footprint(knobs);
    let needed = fp.disk_bytes.saturating_mul(2);
    let Some(anchor) = nearest_existing(dir) else {
        return Ok(());
    };
    let Ok(available) = fs4::available_space(&anchor) else {
        return Ok(());
    };
    if available < needed {
        let mib = |b: u64| b / (1024 * 1024);
        return Err(format!(
            "repogen would write ~{} files (~{} MiB with block rounding, ~{} MiB needed with \
             margin), but only ~{} MiB is free at {}; refusing to fill the device",
            fp.files,
            mib(fp.disk_bytes),
            mib(needed),
            mib(available),
            anchor.display(),
        ));
    }
    Ok(())
}

pub fn generate_profile(dir: &Path, profile: Profile, seed: u64) -> GenOutput {
    generate(dir, &profile.knobs(), seed)
}

/// Generate a folder-repo knowledge base under `dir` from explicit `knobs` and `seed`.
///
/// `dir` becomes the entry folder-repo. The same `(knobs, seed)` writes a
/// byte-identical tree. Returns the [`GenOutput`] handles.
pub fn generate(dir: &Path, knobs: &Knobs, seed: u64) -> GenOutput {
    // Guard the device before writing anything, so a large profile fails fast and
    // clean instead of half-filling the disk. Fails open when free space is
    // unreadable.
    if let Err(e) = check_disk_space(dir, knobs) {
        panic!("{e}");
    }

    let mut out = GenOutput {
        entry: dir.to_path_buf(),
        ..Default::default()
    };

    // The entry repo: content-free, a pure composition root. Its `workspace.yaml`
    // must list itself (else `workspace-omits-containing-repo`) plus every member.
    write_file(&dir.join(".arsumbris/repo.yaml"), "name: bench\n");
    let mut edit = String::from("edit:\n  - bench\n");
    for k in 0..knobs.repo_count {
        edit.push_str(&format!("  - repo{k}\n"));
    }
    write_file(&dir.join(".arsumbris/workspace.yaml"), &edit);

    // Which members import `repo0`. `repo0` never imports (it is the source);
    // each later repo imports with probability `xref_density`, drawn in a fixed
    // order so the selection is deterministic per `(knobs, seed)`.
    let mut rng = Rng::new(seed);
    let importers: Vec<bool> = (0..knobs.repo_count)
        .map(|k| k != 0 && knobs.repo_count > 1 && rng.ratio() < knobs.xref_density)
        .collect();

    for k in 0..knobs.repo_count {
        gen_repo(dir, k, knobs, importers[k], &mut out);
    }
    out
}

/// Write one member repo `repo{k}` under the entry `dir`.
fn gen_repo(dir: &Path, k: usize, knobs: &Knobs, imports: bool, out: &mut GenOutput) {
    let rdir = dir.join(format!("repo{k}"));

    // repo.yaml: an importer declares `repo0` a dep (a co-present sibling resolved
    // by its marker), so the `t0::repo0` type crossing is a declared dependency.
    let repo_yaml = if imports {
        format!("name: repo{k}\ndeps:\n  - name: repo0\n")
    } else {
        format!("name: repo{k}\n")
    };
    write_file(&rdir.join(".arsumbris/repo.yaml"), &repo_yaml);

    // The parent chain `base0 <- base1 <- ... <- base{closure_depth-1}`. base0 is a
    // root; each deeper base extends the previous, so a leaf that extends the tip
    // folds the whole chain into its closure id. Each carries one OPTIONAL field:
    // it makes the level a distinct type-def yet, being optional, never forces a
    // value on an instance that inherits the chain, so the knowledge base stays clean.
    for d in 0..knobs.closure_depth {
        let td = if d == 0 {
            format!("fields:\n  d{d}?: Number\n")
        } else {
            format!("extends: base{}\nfields:\n  d{d}?: Number\n", d - 1)
        };
        let path = rdir.join(format!("type/base{d}.type.yaml"));
        write_file(&path, &td);
        out.type_defs.push(path);
    }
    let tip: Option<usize> = knobs.closure_depth.checked_sub(1);

    // A bare-name record type, the inline-record slot target for `t0`'s optional
    // `detail?`. Inline-only (bare name), so an instance fills it with a nested
    // map, exercising the record-target index and nested value resolution.
    let rec_path = rdir.join("type/rec.type.yaml");
    write_file(&rec_path, "fields:\n  note: String\n");
    out.type_defs.push(rec_path);

    // Leaf types `t0..t{types_per_repo-1}`, each extending the chain tip. `t0` is
    // the primary: it carries a `link?: t0*` reference slot (self-typed, so an
    // importer's `[[..::repo0]]` reference is a conforming cross-repo edge) and an
    // optional `detail?: rec` inline-record slot (filled by the inline-record
    // instance shape below, unset elsewhere, so instances without it stay clean).
    let types_per_repo = knobs.types_per_repo.max(1);
    for m in 0..types_per_repo {
        let parent = tip
            .map(|t| format!("extends: base{t}\n"))
            .unwrap_or_default();
        let td = if m == 0 {
            format!("{parent}fields:\n  title: String\n  link?: t0*\n  detail?: rec\n")
        } else {
            format!("{parent}fields:\n  title: String\n")
        };
        let path = rdir.join(format!("type/t{m}.type.yaml"));
        write_file(&path, &td);
        out.type_defs.push(path);
    }

    // Instances. An importer's first `xref` instances claim `t0::repo0` and fill
    // the typed `link?: t0*` slot with a cross-repo reference to `repo0`'s `n0`, a
    // guaranteed `t0` instance (index 0, `0 % types_per_repo == 0`), so the
    // reference resolves AND conforms, a real cross-repo edge. A local instance
    // claims a leaf type and holds only an UNTYPED navigational wikilink in its
    // body, so it carries no conformance constraint and the knowledge base stays clean.
    let xref = if imports {
        knobs.instance_count.min(2)
    } else {
        0
    };
    // A hub referrer is a `repo0` instance (`n1..n{hub_fanin}`) that claims t0 and
    // references the single hub node `repo0/n0` through the typed `t0*` slot, so
    // n0 accrues `hub_fanin` backlinks. Dirtying them all at once is the
    // co-dirtied-hub O(N^2) stressor. Only `repo0` owns the hub.
    for i in 0..knobs.instance_count {
        let path = rdir.join(format!("n{i}.md"));
        let is_hub = k == 0 && i >= 1 && i <= knobs.hub_fanin;
        let body = if i < xref {
            format!(
                "---\ntype: t0::repo0\ntitle: Ref {i}\nlink: \"[[n0::repo0]]\"\n---\n# Ref {i}\n"
            )
        } else if is_hub {
            format!("---\ntype: t0\ntitle: Hub ref {i}\nlink: \"[[n0]]\"\n---\n# Hub ref {i}\n")
        } else {
            // Local instances rotate through clean but AWKWARD shapes, so the
            // build-twice and incremental-vs-full checks exercise the reference,
            // body, and inline-record surfaces where real bugs have lived, not just
            // uniform typed notes. Deterministic by index.
            let ty = i % types_per_repo;
            let link = (i + 1) % knobs.instance_count.max(1);
            let link2 = (i + 2) % knobs.instance_count.max(1);
            match i % 4 {
                // Untyped note: no frontmatter, a bare node in the link graph.
                1 => format!("# Note {i}\n\nbody {i}, see [[n{link}]].\n"),
                // Inline record: a `t0` instance filling `detail?: rec` with an
                // addressable (`^:`) inline record.
                2 => format!(
                    "---\ntype: t0\ntitle: Note {i}\ndetail:\n  ^: d{i}\n  note: detail {i}\n---\n# Note {i}\n\nbody {i}, see [[n{link}]].\n"
                ),
                // Two embedded wikilinks in the body.
                3 => format!(
                    "---\ntype: t{ty}\ntitle: Note {i}\n---\n# Note {i}\n\nbody {i}, see [[n{link}]] and [[n{link2}]].\n"
                ),
                // Baseline: one typed note, one body wikilink.
                _ => format!(
                    "---\ntype: t{ty}\ntitle: Note {i}\n---\n# Note {i}\n\nbody {i}, see [[n{link}]].\n"
                ),
            }
        };
        write_file(&path, &body);
        if i < xref {
            out.xref_referrers.push(path.clone());
        }
        if is_hub {
            out.hub_referrers.push(path.clone());
        }
        out.instances.push(path);
    }

    // The co-typed cohort (`repo0` only): `cotype_cohort` extra instances all
    // claiming the primary `t0`, link-free, so they share the one closure key
    // `(repo0, t0)` and stress ONLY the closure-membership index when bulk-retyped.
    // `t0` requires `title`; the base chain is optional, so a bare title is clean.
    if k == 0 {
        for i in 0..knobs.cotype_cohort {
            let path = rdir.join(format!("c{i}.md"));
            write_file(
                &path,
                &format!("---\ntype: t0\ntitle: Cohort {i}\n---\n# Cohort {i}\n"),
            );
            out.cotype_cohort.push(path.clone());
            out.instances.push(path);
        }
    }
}

/// Create parent dirs and write `contents`. Panics on I/O error, this is a test
/// toolkit driving a fresh tempdir.
fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Collect a directory tree as `relative-path -> bytes`, so two trees compare
    /// independent of their (tempdir) roots.
    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let rel = path.strip_prefix(root).unwrap().to_path_buf();
                    out.insert(rel, std::fs::read(&path).unwrap());
                }
            }
        }
        out
    }

    #[test]
    fn same_profile_and_seed_is_byte_identical() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        generate_profile(a.path(), Profile::Medium, 42);
        generate_profile(b.path(), Profile::Medium, 42);
        assert_eq!(
            snapshot(a.path()),
            snapshot(b.path()),
            "same (profile, seed) must write a byte-identical tree"
        );
    }

    #[test]
    fn a_different_seed_varies_the_tree() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        // A profile with cross-repo edges, so the seed's importer selection shows.
        generate_profile(a.path(), Profile::Large, 1);
        generate_profile(b.path(), Profile::Large, 2);
        assert_ne!(
            snapshot(a.path()),
            snapshot(b.path()),
            "a different seed must vary the generated tree"
        );
    }

    #[test]
    fn output_handles_point_at_written_files() {
        let d = tempfile::tempdir().unwrap();
        let out = generate_profile(d.path(), Profile::Small, 7);
        assert_eq!(out.entry, d.path());
        assert!(!out.instances.is_empty() && out.instances.iter().all(|p| p.is_file()));
        assert!(!out.type_defs.is_empty() && out.type_defs.iter().all(|p| p.is_file()));
        assert!(out.xref_referrers.iter().all(|p| p.is_file()));
    }

    #[test]
    fn cotype_cohort_yields_link_free_repo0_instances_claiming_t0() {
        let d = tempfile::tempdir().unwrap();
        let knobs = Knobs {
            repo_count: 1,
            types_per_repo: 2,
            closure_depth: 1,
            xref_density: 0.0,
            instance_count: 3,
            hub_fanin: 0,
            cotype_cohort: 5,
        };
        let out = generate(d.path(), &knobs, 7);
        assert_eq!(
            out.cotype_cohort.len(),
            5,
            "handle exposes the whole cohort"
        );
        for p in &out.cotype_cohort {
            let body = std::fs::read_to_string(p).unwrap();
            assert!(body.contains("type: t0"), "cohort member claims t0: {body}");
            assert!(
                !body.contains("[["),
                "cohort member is link-free (isolates the closure index): {body}"
            );
            assert!(
                out.instances.contains(p),
                "cohort member is a tracked instance"
            );
        }
    }
}
