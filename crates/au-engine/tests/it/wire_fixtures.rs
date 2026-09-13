//! Shared daemon-wire fixtures for the scoped reads.
//!
//! `scope: own` is a NO-OP on a single-repo knowledge base — the entry is editable, so
//! every repo is already "own". A scoping test written against a single-repo
//! fixture therefore passes whether or not the filter exists. Same hazard one
//! level down for the `hubs` top-N: nothing is crowded out below `HUB_LIMIT`,
//! so a scoping-before-truncation assertion is vacuous unless something
//! genuinely crowds.
//!
//! These fixtures exist to make those assertions capable of failing. They land
//! BEFORE the scoping work so the tests that consume them are written against a
//! knowledge base that can fail them.
//!
//! - [`multi_member_kb`] — a thin entry, one `edit` member, one co-present
//!   dependency, each contributing types, instances, hubs, top-level dirs, and
//!   diagnostics.
//! - [`hub_crowding_kb`] — a dependency whose hubs outrank the entry's own
//!   and exceed `HUB_LIMIT`, so unscoped ranking crowds own content out
//!   entirely.
//!
//! Both are consumed by the `overview` / `diagnostics` / `hubs` /
//! `top_level_dirs` scope tests. The self-check tests below assert each
//! fixture's topology is what it claims, so a fixture cannot silently stop
//! exercising the thing it exists for.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::{json, Value};

/// A running daemon over a fixture root, with a connected client.
pub struct Harness {
    _sock_dir: tempfile::TempDir,
    _server: ServeHandle,
    pub client: Client,
}

impl Harness {
    /// Issue a read and return its `result`, asserting the frame is ready.
    /// Args carry the read's own params; `read` is filled in here.
    pub fn read(&mut self, verb: &str, args: Value) -> Value {
        let mut req = args;
        req["read"] = json!(verb);
        let resp = self.client.query(&req).expect("query");
        assert_eq!(resp["ready"], json!(true), "{verb}: not ready");
        resp["result"].clone()
    }

    /// Issue a read and return its enveloped payload, `result[verb]`.
    pub fn payload(&mut self, verb: &str, args: Value) -> Value {
        let result = self.read(verb, args);
        result
            .get(verb)
            .unwrap_or_else(|| panic!("{verb}: result carries no `{verb}` key: {result}"))
            .clone()
    }
}

/// Boot a daemon over `root` and connect. The build runs before `serve`
/// returns a ready frame, so the first read answers against a complete knowledge base.
pub fn harness(root: &Path) -> Harness {
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(root, ConfigSource::Empty);
    engine.rebuild();
    let server = serve(engine.handle(), &socket).expect("serve");
    let client = Client::connect(&socket).expect("connect");
    Harness {
        _sock_dir: sock_dir,
        _server: server,
        client,
    }
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// The multi-member topology the scoping tests need.
///
/// - `v`, the entry, holds NO content of its own. This is the expected shape: a
///   thin repo carrying `workspace.yaml` with the content in `edit` members. It
///   is also what makes an entry-scoped answer visibly wrong. (Its members sit
///   physically inside it, so `top_level_dirs` does report their FOLDERS for
///   `v`, tagged `member` — see the self-check below.)
/// - `app`, an `edit` member — editable, so `scope: own` KEEPS it.
/// - `base`, a co-present dependency of `app` — a live working tree, but
///   consumed, so `scope: own` DROPS it. That pins the role-over-location rule
///   of decision 2607161333: `own` follows the member's role, never where it
///   resolved on disk.
///
/// Every scoped field has content on BOTH sides of the filter, so each
/// assertion can fail in both directions:
/// - types: `app` owns `task`, `base` owns `thing`.
/// - top-level dirs: each member has `type/` and `content/`; the entry has none.
/// - hubs: each member has an instance others point a typed slot at.
/// - diagnostics: each member has a dangling reference. Most fixtures produce
///   diagnostics only in the entry, which leaves a repo filter nothing to
///   filter.
pub fn multi_member_kb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();

    // The entry edits only `app`; `base` is pulled in solely as `app`'s
    // declared dependency, so assembly gives it the `dep` role.
    crate::seed_workspace(&root, &["app"]);

    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    write(
        &root,
        "app/type/task.type.yaml",
        "fields:\n  title: String\n  rel?: task*\n",
    );
    write(
        &root,
        "app/content/app-hub.md",
        "---\ntype: task\ntitle: app hub\n---\n",
    );
    for i in 0..3 {
        write(
            &root,
            &format!("app/content/app-ref-{i}.md"),
            &format!("---\ntype: task\ntitle: a{i}\nrel: \"[[app-hub]]\"\n---\n"),
        );
    }
    // A dangling reference, so `app` owns a diagnostic.
    write(
        &root,
        "app/content/app-broken.md",
        "---\ntype: task\ntitle: broken\nrel: \"[[app-nowhere]]\"\n---\n",
    );

    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/thing.type.yaml",
        "fields:\n  title: String\n  rel?: thing*\n",
    );
    write(
        &root,
        "base/content/base-hub.md",
        "---\ntype: thing\ntitle: base hub\n---\n",
    );
    for i in 0..3 {
        write(
            &root,
            &format!("base/content/base-ref-{i}.md"),
            &format!("---\ntype: thing\ntitle: b{i}\nrel: \"[[base-hub]]\"\n---\n"),
        );
    }
    // A dangling reference, so `base` owns a diagnostic too.
    write(
        &root,
        "base/content/base-broken.md",
        "---\ntype: thing\ntitle: broken\nrel: \"[[base-nowhere]]\"\n---\n",
    );

    (dir, root)
}

/// How many hubs a dependency contributes in [`hub_crowding_kb`]. Above
/// `HUB_LIMIT` (20) on purpose: the dependency must fill the whole ranking, not
/// merely most of it, so an unscoped `hubs` drops own content ENTIRELY rather
/// than shortening it.
pub const CROWDING_HUBS: usize = 25;

/// A dependency that crowds own content out of the hub top-N.
///
/// Ranking is `refs_structural` desc, then `refs_total`, then path. So `base`'s
/// hubs each carry TWO structural edges and `app`'s own hub carries one: every
/// `base` hub outranks it, and there are more of them than `HUB_LIMIT`, so the
/// unscoped ranking is 100% dependency.
///
/// The edges come from two list-slot referrers rather than 50 referrer files: a
/// `thing*[]` slot yields one backlink edge per element, so two referrers give
/// every target exactly two structural edges.
pub fn hub_crowding_kb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();

    crate::seed_workspace(&root, &["app"]);

    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    write(
        &root,
        "app/type/task.type.yaml",
        "fields:\n  title: String\n  rel?: task*\n",
    );
    write(
        &root,
        "app/content/app-hub.md",
        "---\ntype: task\ntitle: app hub\n---\n",
    );
    // ONE structural edge, so every `base` hub (two each) outranks it.
    write(
        &root,
        "app/content/app-ref.md",
        "---\ntype: task\ntitle: a\nrel: \"[[app-hub]]\"\n---\n",
    );

    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/thing.type.yaml",
        "fields:\n  title: String\n  rels?: \"thing*[]\"\n",
    );
    for i in 0..CROWDING_HUBS {
        write(
            &root,
            &format!("base/content/base-hub-{i}.md"),
            &format!("---\ntype: thing\ntitle: base hub {i}\n---\n"),
        );
    }
    let all: String = (0..CROWDING_HUBS)
        .map(|i| format!("  - \"[[base-hub-{i}]]\"\n"))
        .collect();
    for r in 0..2 {
        write(
            &root,
            &format!("base/content/base-ref-{r}.md"),
            &format!("---\ntype: thing\ntitle: r{r}\nrels:\n{all}---\n"),
        );
    }

    (dir, root)
}

// ---------------------------------------------------------------------------
// Self-checks: each asserts the fixture's topology is what its doc claims.
// A fixture that stopped exercising its scoping axis would make every test
// downstream of it pass vacuously, which is the exact failure these prevent.
// ---------------------------------------------------------------------------

/// The multi-member fixture really has an editable member AND a consumed
/// dependency, so `scope: own` has something to drop. Asserted through
/// `members`, which reports the role each scope filter derives from.
#[test]
fn multi_member_fixture_has_an_editable_member_and_a_consumed_dependency() {
    let (_dir, root) = multi_member_kb();
    let mut h = harness(&root);

    let members = h.payload("members", json!({}));
    let members = members.as_array().expect("members is an array");
    let by_name = |name: &str| {
        members
            .iter()
            .find(|m| m["repo"] == name)
            .unwrap_or_else(|| panic!("member {name} present: {members:?}"))
            .clone()
    };

    assert_eq!(by_name("v")["role"], "entry");
    assert_eq!(by_name("v")["editable"], true);

    assert_eq!(by_name("app")["role"], "edit");
    assert_eq!(by_name("app")["editable"], true, "app is own content");

    let base = by_name("base");
    assert_eq!(base["role"], "dep");
    assert_eq!(
        base["editable"], false,
        "base is consumed, so `own` must drop it"
    );
    assert_eq!(
        base["local"], true,
        "base is a co-present live tree — role, not location, is what `own` follows"
    );
}

/// Every scoped field has content on both sides of the `own` filter, and the
/// entry contributes none. Without this the scope tests could pass by filtering
/// nothing, or by filtering everything.
#[test]
fn multi_member_fixture_populates_both_sides_of_the_own_filter() {
    let (_dir, root) = multi_member_kb();
    let mut h = harness(&root);

    // Types: one per member, none in the entry.
    let counts = h.payload("type_counts", json!({}));
    let by_repo = &counts["by_repo"];
    assert_eq!(by_repo["app"], 1, "app owns `task`: {by_repo}");
    assert_eq!(by_repo["base"], 1, "base owns `thing`: {by_repo}");
    assert!(
        by_repo.get("v").is_none(),
        "the entry is thin, it owns no types: {by_repo}"
    );

    // Top-level dirs: each member has `type/` and `content/`. The entry holds
    // no content of its own, but the members sit physically inside it, so their
    // folders ARE directories of the entry and are reported for it too — tagged
    // `member`, which is how a consumer tells the overlap from real content.
    let dirs = h.payload("top_level_dirs", json!({ "scope": "all" }));
    let dirs = dirs.as_array().unwrap();
    let of = |repo: &str| -> Vec<String> {
        let mut v: Vec<String> = dirs
            .iter()
            .filter(|d| d["repo"] == repo)
            .map(|d| d["name"].as_str().unwrap().to_string())
            .collect();
        v.sort();
        v
    };
    assert_eq!(of("app"), vec!["content", "type"]);
    assert_eq!(of("base"), vec!["content", "type"]);
    assert_eq!(
        of("v"),
        vec!["app", "base"],
        "the entry's only dirs are the nested members' own folders: {dirs:?}"
    );

    let tagged = |repo: &str, name: &str| -> Value {
        dirs.iter()
            .find(|d| d["repo"] == repo && d["name"] == name)
            .unwrap_or_else(|| panic!("{repo}/{name} present: {dirs:?}"))["member"]
            .clone()
    };
    assert_eq!(tagged("v", "app"), json!("app"), "nested member named");
    assert_eq!(tagged("v", "base"), json!("base"), "nested member named");
    assert_eq!(
        tagged("app", "content"),
        Value::Null,
        "ordinary content dir carries no member tag"
    );

    // Diagnostics: one dangling reference per member, so a repo filter has
    // something to keep AND something to drop.
    let diags = h.payload("diagnostics", json!({ "scope": "all" }));
    let in_repo = |repo: &str| {
        diags
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| {
                d["span"]["file"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("/{repo}/"))
            })
            .count()
    };
    assert!(in_repo("app") >= 1, "app owns a diagnostic: {diags}");
    assert!(in_repo("base") >= 1, "base owns a diagnostic: {diags}");

    // Hubs: one per member, so scoping is observable in the ranking. Asked
    // UNSCOPED — `overview` defaults to `own`, so it is the wrong instrument
    // for checking that the fixture has content on both sides of that filter.
    let hubs = h.payload("hubs", json!({ "scope": "all" }));
    let hubs = hubs.as_array().unwrap();
    let ends_with = |suffix: &str| {
        hubs.iter()
            .any(|hb| hb["path"].as_str().unwrap().ends_with(suffix))
    };
    assert!(ends_with("/app-hub.md"), "app hub ranked: {hubs:?}");
    assert!(ends_with("/base-hub.md"), "base hub ranked: {hubs:?}");
}

/// The crowding fixture really crowds: TODAY, before any scoping lands, the
/// unscoped hub ranking is 100% dependency and own content is absent entirely.
///
/// This is the assertion that makes the eventual scoping test non-vacuous. If
/// this ever stops holding, the fixture no longer crowds and the scoping test
/// downstream of it proves nothing.
#[test]
fn hub_crowding_fixture_fills_the_whole_ranking_with_the_dependency() {
    let (_dir, root) = hub_crowding_kb();
    let mut h = harness(&root);

    // UNSCOPED, the pre-filter ranking: `overview` now defaults to `own`, and
    // asking it here would assert the fix rather than the crowding the fix
    // exists to close.
    let hubs = h.payload("hubs", json!({ "scope": "all" }));
    let hubs = hubs.as_array().expect("hubs is an array");

    assert_eq!(hubs.len(), 20, "the ranking is truncated to HUB_LIMIT");
    assert!(
        hubs.iter().all(|hb| hb["repo"] == "base"),
        "every ranked hub is the dependency's: {:?}",
        hubs.iter().map(|hb| &hb["repo"]).collect::<Vec<_>>()
    );
    assert!(
        !hubs
            .iter()
            .any(|hb| hb["path"].as_str().unwrap().ends_with("/app-hub.md")),
        "own content is crowded OUT of the top-N, which is the bug the scoping \
         fix closes: {hubs:?}"
    );

    // The crowding is real, not an artifact of a broken fixture: the
    // dependency's hubs outrank own content on the actual ranking key.
    assert!(
        hubs.iter().all(|hb| hb["refs_structural"] == 2),
        "each dependency hub carries two structural edges: {hubs:?}"
    );
    assert!(
        CROWDING_HUBS > 20,
        "the dependency must exceed HUB_LIMIT, else it merely shortens the list"
    );
}
