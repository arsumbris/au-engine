//! The held intermediate representation of a knowledge base.
//!
//! A [`KnowledgeBase`] is what the engine keeps in memory after analysing the file
//! tree once: the file catalog (each entry carrying its parse), the type
//! graph, the resolved analysis per instance, and the merged diagnostic
//! stream.
//!
//! Two layers sit per file, following [[design - engine shape]]:
//! - the parse layer, [`FileParse`], depends only on the bytes, branch- and
//!   graph-agnostic, held in the catalog keyed by path and tagged with the
//!   content hash so a rebuild can tell what changed.
//! - the resolved layer, [`ResolvedInstance`], depends on the type graph and
//!   the file set, recomputed per build over the parse layer.
//!
//! The held analysis is shared structurally: the patched maps (catalog,
//! instances, backlinks, the reverse-dependency indices, the served diagnostic
//! stream, ...) are persistent ordered maps ([`OrdMap`]) and the invariant
//! structs (graphs, repos, cross-repo sites, workspaces) are `Arc`-shared, so an
//! incremental rebuild clones the knowledge base in O(1) and patches it by delta.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use au_core::{EffectiveShape, ResolutionGraph, TypeGraph, TypeName};
use au_diagnostics::{Diagnostic, LineIndex};
use au_parser::FileKind;
use au_references::{RepoIndex, ResolutionError};

use crate::backlinks::Backlink;
use crate::parse::FileParse;
use crate::repo::{RepoMap, RepoName, Workspace};

/// The persistent ordered map backing the held [`KnowledgeBase`]'s patched fields.
///
/// Clone is O(1) and a patch is O(log n) with structural sharing, so the
/// incremental apply carrier shares the unchanged majority of the knowledge base by
/// pointer instead of deep-copying it on every edit. Iteration is ordered by
/// key (a red-black tree), matching `BTreeMap`, which the parity oracle and the
/// served-diagnostic stream require. The `Sync` variant carries `Arc` pointers,
/// so the held knowledge base stays `Send + Sync` for the served-thread handle.
pub type OrdMap<K, V> = rpds::RedBlackTreeMapSync<K, V>;

/// The served diagnostic stream, keyed by the total sort key
/// `(file, byte-start, code)`, the value the content-sorted diagnostics sharing
/// that key. See [`KnowledgeBase::served`].
pub type DiagStream = OrdMap<(PathBuf, usize, String), Vec<Diagnostic>>;

/// A content hash of a file's bytes, FNV-1a 64-bit over the raw bytes.
///
/// It tags the catalog entry with the bytes it was built from, so a rebuild can
/// tell whether a file's content changed. It also arms read-before-write CAS, so
/// a consumer holding the bytes can compute the identical hash locally without an
/// engine round-trip.
///
/// The algorithm is a DECLARED au-engine-sdk contract, not an internal detail. It
/// may change (e.g. to a git blob oid once the engine faults blobs by oid), but
/// only as a COORDINATED change: `content-hash-vectors.json` and the SDK mirror
/// update together, never silently, so a mirror can never drift unnoticed. See
/// `crates/au-engine/WIRE.md` and the `content_hash_vector_tests` guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash(pub u64);

impl ContentHash {
    /// FNV-1a 64-bit over the bytes. Deterministic, so a clean rebuild over
    /// unchanged content produces the same catalog. The blessed contract
    /// algorithm, mirrored in au-engine-sdk and pinned by `content-hash-vectors.json`.
    pub fn of(bytes: &[u8]) -> Self {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        ContentHash(hash)
    }
}

#[cfg(test)]
mod content_hash_vector_tests {
    use super::ContentHash;

    /// `content-hash-vectors.json` is the published lockstep guard for the blessed
    /// FNV-1a-64 content-hash contract: a change to `ContentHash::of` breaks this,
    /// forcing a coordinated au-engine-sdk mirror + vector update rather than a
    /// silent CAS drift. See `crates/au-engine/WIRE.md`.
    #[test]
    fn content_hash_matches_the_published_vectors() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/content-hash-vectors.json");
        let bytes = std::fs::read(path).expect("read content-hash-vectors.json");
        let doc: serde_json::Value = serde_json::from_slice(&bytes).expect("parse vector json");
        let vectors = doc["vectors"].as_array().expect("vectors is an array");
        assert!(!vectors.is_empty(), "the vector file must carry cases");
        for v in vectors {
            let input = v["input"].as_str().expect("input is a string");
            let expected = u64::from_str_radix(v["hash"].as_str().expect("hash is a string"), 16)
                .expect("hash is 16 hex digits");
            assert_eq!(
                ContentHash::of(input.as_bytes()).0,
                expected,
                "content hash drifted from the published vector for {input:?}; if the algorithm \
                 change is intentional, update content-hash-vectors.json AND coordinate the \
                 au-engine-sdk mirror"
            );
        }
    }
}

/// One file the engine knows: its classification, the content hash of the
/// bytes it was built from, and its parse.
///
/// `hash` is `Some` only for files the build read, type-defs and instance
/// candidates. Asset binaries are catalogued by path and kind but not read, so
/// they carry no hash and an empty parse until the content-addressed store
/// lands.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub kind: FileKind,
    pub hash: Option<ContentHash>,
    /// The parse layer for this file, reusable while `hash` is unchanged.
    /// `Arc`-shared so a rebuild can hand it to the next build by pointer,
    /// the reuse seam [`ParseLayer`] extracts.
    pub parse: Arc<FileParse>,
    /// Byte-offset → line/column table over the bytes `hash` was computed
    /// from. `Some` exactly when `hash` is: files the build read. `Arc`-shared
    /// alongside the parse, so a read-skipping rebuild reuses it without the
    /// bytes.
    pub line_index: Option<Arc<LineIndex>>,
    /// Byte length of the content `hash` was computed over. `Some` exactly when
    /// `hash` is: files the build read. Recorded here, where the build already
    /// holds the bytes, so a reader costs a file without stat-ing it, and a
    /// read-skipping rebuild carries the length forward like the line index. An
    /// unread asset carries `None`: its length is not costable without reading
    /// it, which is the cost this field exists to avoid.
    pub byte_len: Option<usize>,
}

/// The reusable parse layer carried across rebuilds.
///
/// Maps a path to the content hash its parse was built from, the shared parse,
/// and its line index. A rebuild consults it per file: an unchanged hash reuses
/// the parse, and the read it came from, instead of re-reading and re-parsing.
/// Cheap to clone, every parse and index is `Arc`-shared, so handing the layer
/// to the next build is a pointer copy.
///
/// This is the parse-layer seam [[design - engine shape]] names: branch- and
/// graph-agnostic, keyed by content. The content hash becomes the git blob oid
/// once blobs fault by oid, the same shape with a canonical function behind it.
#[derive(Debug, Default, Clone)]
pub struct ParseLayer {
    by_path: BTreeMap<PathBuf, ParseEntry>,
}

#[derive(Debug, Clone)]
struct ParseEntry {
    hash: ContentHash,
    parse: Arc<FileParse>,
    line_index: Option<Arc<LineIndex>>,
    /// Byte length of the content this parse was built from, carried so a
    /// read-skipping reuse reports the length without the bytes, like
    /// `line_index`. Non-optional here: the layer holds only read files.
    byte_len: usize,
}

impl ParseLayer {
    /// The reusable parse and line index for a file whose content hash matches.
    ///
    /// `None` when the path is absent or its stored hash differs, the bytes
    /// changed, so the caller parses fresh. A match returns the shared parse and
    /// index by pointer; `parse_file` is pure over `(path, bytes)`, so a path
    /// and hash match yields the identical parse without re-running it.
    pub fn reuse(
        &self,
        path: &Path,
        hash: ContentHash,
    ) -> Option<(Arc<FileParse>, Option<Arc<LineIndex>>, usize)> {
        let entry = self.by_path.get(path)?;
        (entry.hash == hash).then(|| {
            (
                entry.parse.clone(),
                entry.line_index.clone(),
                entry.byte_len,
            )
        })
    }

    /// The number of files held, for build instrumentation and tests.
    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    /// True when the layer holds no files, the first-build case.
    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }
}

/// The resolved analysis of one instance file, the ref-scoped layer over its
/// parse.
///
/// `effective_shape` is `None` when a claimed type name is absent from the
/// graph, the claim doesn't resolve, so there's no shape to validate against.
/// The parse, including field values, lives in the catalog entry, see
/// [`KnowledgeBase::file_parse`]; a divergent field stays in the shape's
/// `divergent` set (`effective_shape.divergent()`).
#[derive(Debug, Clone)]
pub struct ResolvedInstance {
    /// The merged shape the instance is validated against, `None` for an
    /// unresolved claim.
    pub effective_shape: Option<EffectiveShape>,
}

/// How far the build got before stopping.
///
/// A broken vocabulary or invalid meta-bodies stop the build before
/// per-instance validation, validating against them would emit noise the user
/// can't act on. Where it stopped governs what the build managed to produce:
/// a graph-level abort never built the repo indices or parsed instances, a
/// meta-level abort did both before stopping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildOutcome {
    /// Per-instance validation ran; the held analysis is complete.
    Complete,
    /// The type vocabulary had errors. The build stopped before building the
    /// repo indices or parsing any instance.
    AbortedAtGraph,
    /// The vocabulary built but its meta-bodies had errors. The repo indices
    /// and instance parses ran; per-instance validation did not.
    AbortedAtMeta,
}

impl BuildOutcome {
    /// True for either abort, false when the build completed.
    pub fn aborted(self) -> bool {
        !matches!(self, BuildOutcome::Complete)
    }
}

/// The per-repo resolved type graphs.
///
/// Each repo resolves type claims against its own vocabulary, so a
/// repo's graph holds only that repo's type-defs. Cross-repo sameness is by
/// canonical hash in the composition layer, never by merging graphs. A path is
/// routed to its graph through the [`RepoMap`], see [`KnowledgeBase::graph_for_path`].
#[derive(Debug, Default, Clone)]
pub struct RepoGraphs {
    by_repo: BTreeMap<RepoName, TypeGraph>,
    /// The graph for a path under no known repo. Unreachable for catalog paths,
    /// the knowledge base root is always a repo, but held so routing is total.
    empty: TypeGraph,
}

impl RepoGraphs {
    pub fn new(by_repo: BTreeMap<RepoName, TypeGraph>) -> Self {
        Self {
            by_repo,
            empty: TypeGraph::default(),
        }
    }

    /// The graph for a repo by name, the empty graph if the repo has none.
    pub fn of(&self, repo: &RepoName) -> &TypeGraph {
        self.by_repo.get(repo).unwrap_or(&self.empty)
    }

    /// The sentinel empty graph, for a path under no known repo.
    pub fn empty(&self) -> &TypeGraph {
        &self.empty
    }

    pub fn iter(&self) -> impl Iterator<Item = (&RepoName, &TypeGraph)> {
        self.by_repo.iter()
    }
}

/// The per-repo cross-repo resolution graphs, the fold of each repo's own
/// vocabulary plus the claim / parent closures of every `foo::repo` it imports,
/// keyed by [`au_core::TypeId`] ([[design - cross-repo type vocabulary - reference import and vendor as one spectrum over the repo qualifier]]).
///
/// Only a repo that actually imports gets an entry; a repo with no `::repo`
/// claim or parent has `None` here and resolves against its own [`TypeGraph`] as
/// before, so a single-repo knowledge base engages none of this. Built by
/// [`crate::resolution_build::build_resolution_graphs`].
#[derive(Debug, Default, Clone)]
pub struct ResolutionGraphs {
    by_repo: BTreeMap<RepoName, ResolutionGraph>,
}

impl ResolutionGraphs {
    pub fn new(by_repo: BTreeMap<RepoName, ResolutionGraph>) -> Self {
        Self { by_repo }
    }

    /// The resolution graph for a repo, `None` when the repo imports nothing
    /// (resolve against its own [`TypeGraph`] then).
    pub fn of(&self, repo: &RepoName) -> Option<&ResolutionGraph> {
        self.by_repo.get(repo)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&RepoName, &ResolutionGraph)> {
        self.by_repo.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.by_repo.is_empty()
    }
}

/// The per-repo reference indices.
///
/// Each repo resolves wikilink targets against its own files, so resolution is
/// repo-local: an unqualified link in one repo never reaches another's files.
/// Crossing a boundary is explicit, `[[name::repo]]`. A scopeless navigation
/// read with no source repo unions every index, see [`resolve_any`].
///
/// Symmetric with [`RepoGraphs`], scoping is which index you pass; au-core's
/// resolution is unchanged.
///
/// [`resolve_any`]: RepoIndexes::resolve_any
#[derive(Debug, Default, Clone)]
pub struct RepoIndexes {
    by_repo: OrdMap<RepoName, RepoIndex>,
    /// The index for a path under no known repo, empty so routing is total.
    empty: RepoIndex,
}

impl RepoIndexes {
    pub fn new(by_repo: BTreeMap<RepoName, RepoIndex>) -> Self {
        Self {
            by_repo: by_repo.into_iter().collect(),
            empty: RepoIndex::default(),
        }
    }

    /// The per-repo indices, for identical-build parity assertions.
    #[cfg(test)]
    pub(crate) fn parity_entries(&self) -> impl Iterator<Item = (&RepoName, &RepoIndex)> {
        self.by_repo.iter()
    }

    /// The index for a repo by name, the empty index if the repo has none.
    pub fn of(&self, repo: &RepoName) -> &RepoIndex {
        self.by_repo.get(repo).unwrap_or(&self.empty)
    }

    /// Swap in a rebuilt index for one repo, the incremental path's per-repo
    /// index update after a path-set change.
    pub(crate) fn replace(&mut self, repo: RepoName, index: RepoIndex) {
        self.by_repo.insert_mut(repo, index);
    }

    /// The sentinel empty index, for a path under no known repo.
    pub fn empty(&self) -> &RepoIndex {
        &self.empty
    }

    /// Resolve a target across every repo's index, the scopeless navigation
    /// form for a read with no source repo. A target unique across all repos
    /// resolves; a target in two repos is `Ambiguous`, as it is whole-knowledge-base.
    pub fn resolve_any(&self, target: &str) -> Result<PathBuf, ResolutionError> {
        let mut matches: Vec<PathBuf> = Vec::new();
        for idx in self.by_repo.values() {
            match idx.resolve(target) {
                Ok(p) => matches.push(p),
                Err(ResolutionError::Ambiguous(ps)) => matches.extend(ps),
                Err(ResolutionError::Missing) => {}
            }
        }
        matches.sort();
        matches.dedup();
        match matches.len() {
            0 => Err(ResolutionError::Missing),
            1 => Ok(matches.into_iter().next().unwrap()),
            _ => Err(ResolutionError::Ambiguous(matches)),
        }
    }
}

/// What produced a diagnostic, the key the held diagnostic stream is
/// partitioned by.
///
/// Incremental recompute replaces a source's slice and re-merges, rather than
/// rebuilding the whole stream. Each pass attributes its diagnostics to a
/// source: a per-instance validation, a per-repo graph or meta pass, a per-file
/// parse, or the cross-cutting knowledge-base-level passes (walk, discovery, workspace,
/// cross-repo resolution, consistency). See [[spec - incremental resolved recompute - a
/// change recomputes its blast radius byte-identical to a full build]].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagSource {
    /// Cross-cutting: the filesystem walk, repo discovery, workspace assembly,
    /// cross-repo resolution, and consistency. Recomputed as a unit; a
    /// finer cut is deferred to the granularity decision. Invariant under an
    /// instance content edit, so the common edit never recomputes it.
    KnowledgeBase,
    /// A repo's vocabulary: graph load checks, the reference index, meta bodies.
    Repo(RepoName),
    /// One file's parse: structural parse diagnostics and a read failure.
    File(PathBuf),
    /// One source file's resolved diagnostics: `validate`, `validate_body`, and
    /// its cross-boundary `::repo` references. Cross-repo references key here,
    /// not to `KnowledgeBase`, so an instance edit recomputes only its own. A source
    /// note with no type claim still produces cross-repo references and keys
    /// here.
    Instance(PathBuf),
}

/// The held representation of a knowledge base, the result of one full build.
///
/// Regenerable from the file tree; the engine holds it so consumers read the
/// analysis many times without re-running the pipeline.
#[derive(Debug, Clone)]
pub struct KnowledgeBase {
    /// Every file the walker found, by path, each carrying its parse. Holds
    /// non-instance files too (type-defs, assets) so the catalog matches the
    /// resolution file set.
    pub catalog: OrdMap<PathBuf, FileEntry>,
    /// The type vocabulary, one graph per repo. Each repo resolves against its
    /// own; route a path with [`KnowledgeBase::graph_for_path`].
    pub graphs: Arc<RepoGraphs>,
    /// The cross-repo resolution graphs, one per importing repo, the fold of a
    /// repo's own vocabulary plus the claim / parent closures it imports
    /// ([[design - cross-repo type vocabulary - reference import and vendor as one spectrum over the repo qualifier]]).
    /// A repo that imports nothing has no entry and resolves against `graphs`.
    /// Derived from `graphs` plus the catalog's `::repo` claims; not yet served
    /// on the wire, consumed by validation once the fold is wired in.
    pub resolution_graphs: Arc<ResolutionGraphs>,
    /// The repos discovered in the workspace and the file-to-repo membership.
    pub repos: Arc<RepoMap>,
    /// The workspace manifests found in the tree, each with its members and
    /// their resolved paths. Not yet load-bearing; held for scope checks and
    /// assembly.
    pub workspaces: Arc<Vec<Workspace>>,
    /// The forward reference indices, one per repo. Each resolves wikilink
    /// targets against its own repo's files, so resolution is repo-local; route
    /// a path with [`KnowledgeBase::index_for_path`], a scopeless target with
    /// [`KnowledgeBase::resolve_any`].
    pub indexes: RepoIndexes,
    /// The resolved analysis per instance file, keyed by path. Empty when the
    /// build aborted before instance validation, see `outcome`.
    pub instances: OrdMap<PathBuf, ResolvedInstance>,
    /// The reverse reference index: inbound edges keyed by target file. Empty
    /// when the build aborted before the repo indices were built.
    pub backlinks: OrdMap<PathBuf, Vec<Backlink>>,
    /// Closure membership: a `(repo, type)` to the instances in that repo whose
    /// effective closure includes the type. The reverse-dependency index a
    /// type-def edit walks to find the instances it must re-validate.
    ///
    /// Keyed by repo, not bare type name: type graphs are per-repo, so a name is
    /// not globally unique, two repos with a same-named type stay distinct. The
    /// value is path-sorted. See [[spec - incremental resolved recompute - a
    /// change recomputes its blast radius byte-identical to a full build]].
    pub closure_members: OrdMap<(RepoName, TypeName), Vec<PathBuf>>,
    /// The referenced-name index: a `(repo, name-key)` to the sources with an
    /// edge naming it, resolved or dangling. The reverse-dependency index a
    /// path-set change (an add, delete, or rename) walks to its edge-flip set.
    ///
    /// The key's repo is the target side, a `::repo` qualifier or the source's
    /// own repo, so it spans repos. The name-key mirrors au-references
    /// resolution (basename, stem, repo-relative path). See [[spec - incremental
    /// resolved recompute - a change recomputes its blast radius byte-identical
    /// to a full build]].
    pub referenced_names: OrdMap<(RepoName, crate::refnames::NameKey), BTreeSet<PathBuf>>,
    /// The served diagnostic stream, load-time plus per-instance. The view
    /// derived from `diagnostics_by_source` by attaching line/columns and
    /// sorting.
    ///
    /// Held as a persistent ordered map keyed by the total sort key
    /// `(file, byte-start, code)`, the value the content-sorted diagnostics
    /// sharing that key (duplicates preserved). Iterating keys in order, then
    /// each bucket, reproduces the canonical [`crate::diagnostics::sort_diagnostics`]
    /// order. Held this way so an incremental apply splices only the changed
    /// sources' diagnostics rather than rebuilding and re-sorting the whole
    /// stream. Read it through [`KnowledgeBase::diagnostics`].
    pub served: DiagStream,
    /// The diagnostic stream partitioned by producing source, the held state a
    /// recompute replaces slices of. Holds raw diagnostics, line/columns not yet
    /// attached, applied when `diagnostics` is derived. See [`DiagSource`].
    pub diagnostics_by_source: OrdMap<DiagSource, Vec<Diagnostic>>,
    /// How far the build got, per repo. A repo with a broken vocabulary aborts
    /// only its own instance validation; clean repos still validate, so the
    /// outcome is per-repo, not one global verdict.
    pub outcomes: OrdMap<RepoName, BuildOutcome>,
}

impl KnowledgeBase {
    /// The parse layer for a file, by path.
    pub fn file_parse(&self, path: &Path) -> Option<&FileParse> {
        self.catalog.get(path).map(|e| e.parse.as_ref())
    }

    /// The line/column table for a file, by path. `None` for files the
    /// build never read (assets, unreadable files) and unknown paths.
    pub fn line_index(&self, path: &Path) -> Option<&LineIndex> {
        self.catalog.get(path).and_then(|e| e.line_index.as_deref())
    }

    /// The served diagnostic stream in canonical sorted order. Iterates the held
    /// [`KnowledgeBase::served`] map, keys in order then each bucket, which reproduces
    /// [`crate::diagnostics::sort_diagnostics`].
    pub fn diagnostics(&self) -> impl Iterator<Item = &Diagnostic> + '_ {
        self.served.values().flatten()
    }

    /// The number of served diagnostics.
    pub fn diagnostics_len(&self) -> usize {
        self.served.values().map(|v| v.len()).sum()
    }

    /// The served diagnostics for one file, in canonical order.
    ///
    /// The served stream is keyed `(file, byte, code)`, sorted file-first, so a
    /// file's diagnostics are a contiguous key range. This seeks to that range
    /// and stops at the first other file, O(log n + k) over the file's k, not a
    /// whole-stream scan. Same order as [`KnowledgeBase::diagnostics`] restricted to the
    /// file.
    pub fn diagnostics_for_file<'a>(&'a self, file: &Path) -> impl Iterator<Item = &'a Diagnostic> {
        let file = file.to_path_buf();
        let lower = (file.clone(), 0usize, String::new());
        self.served
            .range(lower..)
            .take_while(move |((f, _, _), _)| *f == file)
            .flat_map(|(_, ds)| ds.iter())
    }

    /// The served diagnostics under a directory prefix, in canonical order.
    ///
    /// Path ordering groups a directory's descendants contiguously, so this
    /// seeks to the prefix and stops at the first path outside it, O(log n + k)
    /// over the subtree's k. Same order as [`KnowledgeBase::diagnostics`] restricted to
    /// the prefix.
    pub fn diagnostics_under<'a>(&'a self, prefix: &Path) -> impl Iterator<Item = &'a Diagnostic> {
        let prefix = prefix.to_path_buf();
        let lower = (prefix.clone(), 0usize, String::new());
        self.served
            .range(lower..)
            .take_while(move |((f, _, _), _)| f.starts_with(&prefix))
            .flat_map(|(_, ds)| ds.iter())
    }

    /// Extract the reusable parse layer from this knowledge base, for the next build.
    ///
    /// Only files the build read carry a hash and a reusable parse; an unread
    /// entry (asset, unreadable file) is omitted. The parses and indices are
    /// `Arc`-shared, so this is a map of pointer copies, not a deep copy of the
    /// parse layer.
    pub fn parse_layer(&self) -> ParseLayer {
        let by_path = self
            .catalog
            .iter()
            .filter_map(|(path, entry)| {
                // `hash` and `byte_len` are `Some` together, both set from the
                // bytes the build read. Requiring both keeps a malformed entry
                // (one set, the other not) out of the reuse layer, so it simply
                // re-reads next build rather than reusing an inconsistent pair.
                let hash = entry.hash?;
                let byte_len = entry.byte_len?;
                Some((
                    path.clone(),
                    ParseEntry {
                        hash,
                        parse: entry.parse.clone(),
                        line_index: entry.line_index.clone(),
                        byte_len,
                    },
                ))
            })
            .collect();
        ParseLayer { by_path }
    }

    /// The inbound references to a file, the empty slice when none point at it.
    pub fn backlinks(&self, path: &Path) -> &[Backlink] {
        self.backlinks.get(path).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The instances whose validation depends on `target`'s type identity, the
    /// inbound identity-dependents.
    ///
    /// A view over the backlink index, not separate held state, so it stays
    /// coherent whenever the backlinks do. The backlink index already forms an
    /// inbound edge for a `::repo` link, so dependents in other repos are
    /// included, the index spans repos by construction.
    ///
    /// Identity-bearing edges are those filling a slot or pulling a block
    /// (`slot.is_some()`); purely navigational prose links are dropped. This
    /// over-approximates, a `file*` slot resolves by existence not identity yet
    /// fills a slot, so it rides along. Over-approximation only over-recomputes,
    /// it never misses a true dependent, the safe direction for invalidation.
    ///
    /// Path-sorted and deduplicated, a source with several identity edges to one
    /// target appears once.
    pub fn identity_dependents(&self, target: &Path) -> Vec<&Path> {
        let mut sources: Vec<&Path> = self
            .backlinks(target)
            .iter()
            .filter(|b| b.slot.is_some())
            .map(|b| b.source.as_path())
            .collect();
        sources.sort();
        sources.dedup();
        sources
    }

    /// The source files in `repo` that reference the type-def `name` by name —
    /// in a `type:` claim, a `sealed:` branch, a slot shape, a qualified
    /// key (`field{name}`), a body `use:`, a meta `type:`, or a nested inline-record claim.
    ///
    /// The "who references type X" predicate, the inverse of the type-name
    /// reference sites. A reusable engine-internal predicate, not a served query
    /// — its only caller today is the `rename_type` cascade; a socket/wire
    /// surface is deferred for sequencing, no consumer pulls it yet. Computed on
    /// demand over the current catalog, so it stays coherent without a standing
    /// cache to keep in incremental parity. Repo-AWARE over the mounted set: a
    /// bare `foo` matches only in `repo` itself (own-repo reference), a mounted
    /// `foo::repo` matches from any repo (cross-repo reference), and a same-named
    /// `foo` or `foo::other` in another repo names that repo's copy, never this
    /// one. The wikilink surface (`[[name]]` / `[[name::repo]]` to the def file)
    /// is the backlink index's, not this; see [`crate::typerefs`].
    ///
    /// Path-sorted.
    pub fn type_referrers(
        &self,
        repo: &crate::repo::RepoName,
        name: &au_core::TypeName,
    ) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self
            .catalog
            .iter()
            .filter(|(path, entry)| {
                let file_repo = crate::refnames::source_repo(&self.repos, path);
                crate::typerefs::references_type(
                    &entry.parse,
                    file_repo.as_str(),
                    name,
                    repo.as_str(),
                )
            })
            .map(|(path, _)| path.clone())
            .collect();
        out.sort();
        out
    }

    /// The type graph a path resolves against: its repo's graph. Scoping is
    /// which graph you pass; au-core's resolution is unchanged.
    pub fn graph_for_path(&self, path: &Path) -> &TypeGraph {
        match self.repos.repo_of(path) {
            Some(r) => self.graphs.of(&r.name),
            None => self.graphs.empty(),
        }
    }

    /// The root repo's graph, the repo-root repo. The transitional scope for
    /// type-name reads that do not yet carry a repo, and a single-repo knowledge base's
    /// only graph.
    pub fn root_graph(&self) -> &TypeGraph {
        match self.repos.root() {
            Some(r) => self.graphs.of(&r.name),
            None => self.graphs.empty(),
        }
    }

    /// The reference index a path resolves against: its repo's index. Scoping
    /// is which index you pass; au-core's resolution is unchanged.
    pub fn index_for_path(&self, path: &Path) -> &RepoIndex {
        match self.repos.repo_of(path) {
            Some(r) => self.indexes.of(&r.name),
            None => self.indexes.empty(),
        }
    }

    /// The reference index of a repo named on the wire, `None` when no such
    /// repo exists. The repo-scoped analog for a read that names a repo.
    pub fn index_for_repo(&self, repo: &str) -> Option<&RepoIndex> {
        self.repos.by_name(repo).map(|r| self.indexes.of(&r.name))
    }

    /// The root repo's index, the transitional scope for a scopeless read that
    /// does not yet carry a repo.
    pub fn root_index(&self) -> &RepoIndex {
        match self.repos.root() {
            Some(r) => self.indexes.of(&r.name),
            None => self.indexes.empty(),
        }
    }

    /// Resolve a target across every repo's index, for a scopeless navigation
    /// read with no source repo.
    pub fn resolve_any(&self, target: &str) -> Result<PathBuf, ResolutionError> {
        self.indexes.resolve_any(target)
    }

    /// The graph of a repo named on the wire, `None` when no such repo exists.
    /// The explicit scope a repo-scoped type read resolves against; distinct
    /// from [`root_graph`], which is the no-scope default.
    ///
    /// [`root_graph`]: KnowledgeBase::root_graph
    pub fn graph_for_repo(&self, repo: &str) -> Option<&TypeGraph> {
        self.repos.by_name(repo).map(|r| self.graphs.of(&r.name))
    }

    /// How far the build got for a named repo, `None` when no such repo exists.
    pub fn outcome_for_repo(&self, repo: &str) -> Option<BuildOutcome> {
        self.repos
            .by_name(repo)
            .and_then(|r| self.outcomes.get(&r.name).copied())
    }

    /// How far the build got for the repo a path belongs to.
    pub fn outcome_for_path(&self, path: &Path) -> BuildOutcome {
        self.repos
            .repo_of(path)
            .and_then(|r| self.outcomes.get(&r.name).copied())
            .unwrap_or(BuildOutcome::Complete)
    }

    /// How far the build got for the root repo, the scope of type-name reads.
    pub fn root_outcome(&self) -> BuildOutcome {
        self.repos
            .root()
            .and_then(|r| self.outcomes.get(&r.name).copied())
            .unwrap_or(BuildOutcome::Complete)
    }

    /// True if any repo aborted its load. The aggregate the knowledge-base-wide instance
    /// and candidate reads report.
    pub fn any_aborted(&self) -> bool {
        self.outcomes.values().any(|o| o.aborted())
    }
}

#[cfg(test)]
impl KnowledgeBase {
    /// A deterministic, field-by-field rendering of the whole knowledge base, for
    /// identical-rebuild parity assertions.
    ///
    /// Every held field renders by ordered iteration (`BTreeMap` / `BTreeSet` /
    /// already-sorted `Vec` / [`OrdMap`]), so the rendering is stable across
    /// builds: equal `parity_facts` means byte-identical knowledge bases. This is the
    /// oracle incremental recompute is checked against, the incremental output
    /// must equal a from-scratch build, see [[spec - incremental resolved
    /// recompute - a change recomputes its blast radius byte-identical to a full
    /// build]].
    ///
    /// An [`OrdMap`] field is iterated entry by entry, never whole-map
    /// `Debug`-rendered: a red-black tree's internal shape depends on the
    /// insertion order, so two equal maps built by different op sequences (an
    /// incremental patch versus a from-scratch build) render different whole-map
    /// `Debug` strings. Entry iteration is sorted and canonical, so it compares
    /// the logical content, which is what byte-identity means.
    ///
    /// One fact per line, grouped by field, so a divergence localizes to a
    /// single entity rather than a whole-struct blob.
    pub(crate) fn parity_facts(&self) -> Vec<String> {
        let mut facts = Vec::new();
        for (p, e) in &self.catalog {
            facts.push(format!(
                "catalog {}|{:?}|{:?}|{:?}|{:?}",
                p.display(),
                e.kind,
                e.hash,
                e.parse,
                e.line_index
            ));
        }
        facts.push(format!("graphs {:?}", self.graphs));
        // The cross-repo fold, one entry per importing repo. Emits nothing for a
        // single-repo or non-importing knowledge base (empty `by_repo`). `ResolutionGraphs`
        // holds only `BTreeMap`s, so its `Debug` is canonical (no `OrdMap`
        // insertion-order hazard). Guards a future fold delta-patch against a
        // build-vs-incremental divergence, which today `apply_recompute` masks by
        // rebuilding the fold wholesale.
        for (repo, rg) in self.resolution_graphs.iter() {
            facts.push(format!("resolution_graph {repo:?}|{rg:?}"));
        }
        for (repo, idx) in self.indexes.parity_entries() {
            facts.push(format!("index {repo:?}|{}", idx.parity_repr()));
        }
        for (p, ri) in &self.instances {
            facts.push(format!("instance {}|{:?}", p.display(), ri));
        }
        for (p, bl) in &self.backlinks {
            facts.push(format!("backlinks {}|{:?}", p.display(), bl));
        }
        for ((repo, ty), members) in &self.closure_members {
            let members: Vec<String> = members.iter().map(|p| p.display().to_string()).collect();
            facts.push(format!("closure_member {repo:?}|{ty:?}|{members:?}"));
        }
        for ((repo, key), sources) in &self.referenced_names {
            let sources: Vec<String> = sources.iter().map(|p| p.display().to_string()).collect();
            facts.push(format!("referenced_name {repo:?}|{key:?}|{sources:?}"));
        }
        for d in self.diagnostics() {
            facts.push(format!("diag {d:?}"));
        }
        for (src, ds) in &self.diagnostics_by_source {
            for d in ds {
                facts.push(format!("diag_src {src:?}|{d:?}"));
            }
        }
        facts.push(format!("repos {:?}", self.repos));
        facts.push(format!("workspaces {:?}", self.workspaces));
        for (r, o) in &self.outcomes {
            facts.push(format!("outcome {r:?}|{o:?}"));
        }
        facts
    }
}

/// Assert two knowledge bases are byte-identical, the identical-rebuild oracle.
///
/// Reports the first diverging fact, a whole-`Vec` `assert_eq!` would bury it
/// under the full rendering of a large knowledge base. A trailing count check catches a
/// divergence in length (an added or dropped entity) the zip would otherwise
/// miss past the shorter knowledge base's end.
#[cfg(test)]
pub(crate) fn assert_kb_parity(incremental: &KnowledgeBase, scratch: &KnowledgeBase) {
    let a = incremental.parity_facts();
    let b = scratch.parity_facts();
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        assert_eq!(x, y, "knowledge base parity diverged at fact {i}");
    }
    assert_eq!(
        a.len(),
        b.len(),
        "knowledge base parity: incremental produced {} facts, scratch {}",
        a.len(),
        b.len()
    );
}
