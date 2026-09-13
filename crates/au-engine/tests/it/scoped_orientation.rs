//! `overview` scoped to the user's own repos by default, the `repo` / `scope`
//! args the mirror property forces onto `diagnostics` / `diagnostic_counts`,
//! and the `hubs` read promoted out of `overview`.
//!
//! Every assertion here runs on the MULTI-MEMBER fixture, never a single-repo
//! one: the entry is editable, so `scope: own` is a no-op on a single repo and
//! each of these tests would pass vacuously. The hub-crowding assertions run on
//! the crowding fixture for the same reason — below `HUB_LIMIT` nothing is
//! crowded out, so scoping-before-truncation is unfalsifiable.

#![cfg(unix)]

use serde_json::{json, Value};

use crate::wire_fixtures::{harness, hub_crowding_kb, multi_member_kb};

/// The `repo` of every entry in an array, deduplicated and sorted.
fn repos_of(entries: &Value) -> Vec<String> {
    let mut v: Vec<String> = entries
        .as_array()
        .expect("an array")
        .iter()
        .map(|e| e["repo"].as_str().expect("a repo tag").to_string())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Which repos own the diagnostics in a list, by path containment.
fn diag_repos(diags: &Value) -> Vec<String> {
    let mut v: Vec<String> = diags
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| {
            let f = d["span"]["file"].as_str().unwrap();
            ["app", "base"]
                .iter()
                .find(|r| f.contains(&format!("/{r}/")))
                .map(|r| r.to_string())
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

/// The default `overview` answers "where am I", so it excludes the mounted
/// dependency from every CONTENT field — while `members` still reports it,
/// which is how a consumer sees what `own` selected.
#[test]
fn overview_defaults_to_own_and_excludes_the_dependency_from_content() {
    let (_dir, root) = multi_member_kb();
    let mut h = harness(&root);

    let o = h.payload("overview", json!({}));

    assert_eq!(
        repos_of(&o["top_level_dirs"]),
        vec!["app", "v"],
        "own dirs only, the dependency's are dropped"
    );
    assert!(
        o["type_counts"]["by_repo"].get("base").is_none(),
        "the dependency's types are out of scope: {}",
        o["type_counts"]
    );
    assert_eq!(
        repos_of(&o["hubs"]),
        vec!["app"],
        "only own hubs are ranked"
    );

    // Diagnostics: the dependency's dangling ref is excluded from the counts.
    let own_total = o["diagnostic_counts"]["total"].as_u64().unwrap();
    let all = h.payload("overview", json!({ "scope": "all" }));
    let all_total = all["diagnostic_counts"]["total"].as_u64().unwrap();
    assert!(
        all_total > own_total,
        "the dependency owns diagnostics the own-scoped count excludes \
         (own {own_total}, all {all_total})"
    );

    // members is the TOPOLOGY field and is NOT scope-filtered: the dependency
    // stays visible, and `editable: false` is what explains the filtering above.
    let members = repos_of(&o["members"]);
    assert!(
        members.contains(&"base".to_string()),
        "members still reports the dependency under `own`: {members:?}"
    );
}

/// `scope: all` restores the workspace-wide answer, so the default is a filter
/// and not a change in what the engine can see.
#[test]
fn overview_scope_all_includes_the_dependency() {
    let (_dir, root) = multi_member_kb();
    let mut h = harness(&root);

    let o = h.payload("overview", json!({ "scope": "all" }));

    assert!(repos_of(&o["top_level_dirs"]).contains(&"base".to_string()));
    assert!(o["type_counts"]["by_repo"].get("base").is_some());
    assert!(repos_of(&o["hubs"]).contains(&"base".to_string()));
}

/// `repo` selects one member, and narrows `members` too — that is a selection,
/// not a class filter, so the topology exemption does not apply to it.
#[test]
fn overview_repo_selects_one_member_including_members() {
    let (_dir, root) = multi_member_kb();
    let mut h = harness(&root);

    let o = h.payload("overview", json!({ "repo": "base", "scope": "all" }));

    assert_eq!(repos_of(&o["members"]), vec!["base"]);
    assert_eq!(repos_of(&o["top_level_dirs"]), vec!["base"]);
    assert_eq!(repos_of(&o["hubs"]), vec!["base"]);
}

/// THE mirror property: every `overview` field is reproducible by one call to
/// its own read. It is what caught both forced changes — `diagnostic_counts`
/// could not express repo scoping at all, and `hubs` had no read — so it is
/// worth asserting rather than assuming.
///
/// The mirror is over RESOLVED args, and the result ECHOES them, so this test
/// reads `overview.repo` / `overview.scope` back rather than hand-maintaining
/// an args→resolved table. That echo is the consumer-facing point: most reads
/// now share the `own` default, but `type_counts` deliberately keeps `all` (you
/// author against peer types), so a consumer drilling into that one field must
/// pass the resolved scope rather than assume its own default matches.
#[test]
fn every_overview_field_mirrors_its_own_read() {
    let (_dir, root) = multi_member_kb();
    let mut h = harness(&root);

    for call in [
        json!({}),
        json!({ "scope": "all" }),
        json!({ "repo": "app", "scope": "all" }),
    ] {
        let o = h.payload("overview", call.clone());

        // The resolved args, read off the echo — the same handle a consumer has.
        let mut args = json!({ "scope": o["scope"].clone() });
        if let Some(repo) = o["repo"].as_str() {
            args["repo"] = json!(repo);
        }

        // `members` honors `repo` but NOT `scope`, so it mirrors via its own
        // `repo` arg — no client-side filtering. This is the closed mirror hole:
        // before `members` took a `repo`, `overview({repo}).members` could not be
        // reproduced by any single `members` call.
        let member_args = match args.get("repo") {
            Some(repo) => json!({ "repo": repo }),
            None => json!({}),
        };
        assert_eq!(
            o["members"],
            h.payload("members", member_args),
            "members mirrors via its own repo arg ({args})"
        );
        assert_eq!(
            o["top_level_dirs"],
            h.payload("top_level_dirs", args.clone()),
            "top_level_dirs mirrors ({args})"
        );
        assert_eq!(
            o["type_counts"],
            h.payload("type_counts", args.clone()),
            "type_counts mirrors ({args})"
        );
        assert_eq!(
            o["diagnostic_counts"],
            h.payload("diagnostic_counts", args.clone()),
            "diagnostic_counts mirrors ({args})"
        );
        assert_eq!(
            o["hubs"],
            h.payload("hubs", args.clone()),
            "hubs mirrors — the field that had no read at all ({args})"
        );
        assert_eq!(
            o["graph_shape"],
            h.payload("graph_shape", args.clone()),
            "graph_shape mirrors an argless call at the same scope ({args})"
        );

        // The echo is the whole point: it must describe the call that produced
        // this map, not merely be present.
        assert_eq!(
            o["scope"],
            call.get("scope").cloned().unwrap_or(json!("own")),
            "the echo reports the resolved scope ({call})"
        );
        assert_eq!(
            o["repo"],
            call.get("repo").cloned().unwrap_or(Value::Null),
            "the echo reports the resolved repo ({call})"
        );
    }
}

/// The reads that share the `own` default mirror `overview` with the LITERAL
/// args too, not just the resolved ones — that alignment is what removes the
/// drill-down trap for them. `type_counts` is the one deliberate exception, and
/// is asserted to differ so the exception cannot rot into an accident.
#[test]
fn the_aligned_defaults_make_an_argless_drill_down_agree() {
    let (_dir, root) = multi_member_kb();
    let mut h = harness(&root);

    let o = h.payload("overview", json!({}));

    for field in ["top_level_dirs", "diagnostic_counts", "hubs"] {
        assert_eq!(
            o[field],
            h.payload(field, json!({})),
            "{field} shares overview's `own` default, so an argless drill-down agrees"
        );
    }

    // The exception, deliberate: the vocabulary reads answer "what exists to
    // author against", which includes peer types, so they stay `all`.
    assert_ne!(
        o["type_counts"],
        h.payload("type_counts", json!({})),
        "type_counts deliberately keeps the `all` default; if this ever matches, \
         the exception was removed and the echo's reason with it"
    );
}

/// `diagnostics` and `diagnostic_counts` gained a repo axis, which
/// `DiagnosticsScope` had none of: the filter resolves each diagnostic's
/// `span.file` to its owning member.
#[test]
fn diagnostics_filter_by_owning_repo_and_by_own_scope() {
    let (_dir, root) = multi_member_kb();
    let mut h = harness(&root);

    // Unfiltered: both members' diagnostics are present. Asked explicitly at
    // `all`, since the read now defaults to `own`.
    assert_eq!(
        diag_repos(&h.payload("diagnostics", json!({ "scope": "all" }))),
        vec!["app", "base"],
        "the fixture has diagnostics on both sides of the filter"
    );

    // repo: exactly one member's.
    assert_eq!(
        diag_repos(&h.payload("diagnostics", json!({ "repo": "base" }))),
        vec!["base"]
    );

    // scope own: the dependency's are dropped.
    assert_eq!(
        diag_repos(&h.payload("diagnostics", json!({ "scope": "own" }))),
        vec!["app"]
    );

    // The counts read agrees with the list it drills from.
    let counts = h.payload("diagnostic_counts", json!({ "repo": "base" }));
    let listed = h.payload("diagnostics", json!({ "repo": "base" }));
    assert_eq!(
        counts["total"].as_u64().unwrap() as usize,
        listed.as_array().unwrap().len(),
        "counts and list agree under the same filter"
    );
}

/// A location pin (`path` / `path_prefix`) defaults `scope` to `all`, the same
/// guard `repo` gets. Naming a single file that lives in a read-only dependency
/// must return ITS diagnostics, not empty out because the `own` worklist default
/// dropped it — the open-file-in-a-dependency case.
#[test]
fn a_path_pin_does_not_empty_a_dependency_files_diagnostics() {
    let (_dir, root) = multi_member_kb();
    let mut h = harness(&root);

    // `base` is a dependency (not editable), and `base/content/base-broken.md`
    // owns a dangling-reference diagnostic. With no scope, the file pin must
    // surface it; under the old `own` default it silently returned [].
    let by_path = h.payload(
        "diagnostics",
        json!({ "path": "base/content/base-broken.md" }),
    );
    assert_eq!(
        diag_repos(&by_path),
        vec!["base"],
        "a pinned dependency file returns its own diagnostics: {by_path}"
    );

    // The subtree pin behaves the same.
    let by_prefix = h.payload("diagnostics", json!({ "path_prefix": "base/content" }));
    assert_eq!(diag_repos(&by_prefix), vec!["base"], "{by_prefix}");

    // An EXPLICIT own scope still wins and legitimately empties the pin: the
    // caller stated both filters. Only the DEFAULT is flipped by the pin.
    let pinned_own = h.payload(
        "diagnostics",
        json!({ "path": "base/content/base-broken.md", "scope": "own" }),
    );
    assert!(
        pinned_own.as_array().unwrap().is_empty(),
        "explicit scope:own AND a dependency path is an empty intersection the caller asked for: {pinned_own}"
    );
}

/// The `hubs` read ranks and truncates AFTER scoping, which closes the
/// crowding bug: a dependency larger than `HUB_LIMIT` used to fill the ranking
/// and push own content out entirely.
///
/// The fixture's own self-check pins that the unscoped ranking really is 100%
/// dependency, so this test cannot pass by nothing being crowded.
#[test]
fn hubs_scopes_before_truncating_so_own_content_survives_a_large_dependency() {
    let (_dir, root) = hub_crowding_kb();
    let mut h = harness(&root);

    // Unscoped: the dependency fills every slot (the bug, still true by design).
    let all = h.payload("hubs", json!({ "scope": "all" }));
    assert_eq!(repos_of(&all), vec!["base"]);

    // Own-scoped: own content is ranked, where a client-side filter over the
    // truncated top-N would have returned nothing at all.
    let own = h.payload("hubs", json!({ "scope": "own" }));
    assert_eq!(repos_of(&own), vec!["app"]);
    assert!(
        own.as_array()
            .unwrap()
            .iter()
            .any(|hb| hb["path"].as_str().unwrap().ends_with("/app-hub.md")),
        "the entry's own hub is ranked once the dependency is out of scope: {own}"
    );
}

/// `hubs` pages, so a consumer can walk past the top-N `overview` carries.
#[test]
fn hubs_pages_the_ranking() {
    let (_dir, root) = hub_crowding_kb();
    let mut h = harness(&root);

    let first = h.payload("hubs", json!({ "scope": "all", "limit": 5 }));
    assert_eq!(first.as_array().unwrap().len(), 5);

    let second = h.payload("hubs", json!({ "scope": "all", "limit": 5, "offset": 5 }));
    assert_eq!(second.as_array().unwrap().len(), 5);
    assert_ne!(first[0]["path"], second[0]["path"], "the page advanced");

    // Past the end is empty, never an error.
    let past = h.payload(
        "hubs",
        json!({ "scope": "all", "limit": 5, "offset": 10_000 }),
    );
    assert!(past.as_array().unwrap().is_empty());
}

/// The subscription channel shares the read's filter struct, so it takes the
/// same `repo` / `scope`. A divergence there would be its own drift.
#[test]
fn the_diagnostics_channel_takes_the_same_repo_and_scope_filters() {
    let (_dir, root) = multi_member_kb();
    let mut h = harness(&root);

    // Accepting the arg proves nothing on its own; the channel must actually
    // APPLY it. So subscribe scoped to the dependency and read the initial
    // value, which must carry that member's diagnostics and no others.
    let ack = h
        .client
        .query(&json!({ "subscribe": "diagnostics", "repo": "base" }))
        .expect("subscribe");
    assert_eq!(ack["type"], "ack", "subscribed: {ack}");
    assert_eq!(ack["accepted"], json!(true), "subscribed: {ack}");

    let initial = h.client.recv().expect("frame").expect("a frame");
    assert_eq!(initial["type"], "initial_value");
    assert_eq!(
        diag_repos(&initial["result"]),
        vec!["base"],
        "the channel applies the same repo filter as the read: {initial}"
    );

    // And a typo'd filter is REJECTED rather than silently unfiltered — the
    // channel parses through the read's own args struct, so the strictness
    // WIRE.md promises holds on both surfaces.
    let bogus = h
        .client
        .query(&json!({ "subscribe": "diagnostics", "reop": "base" }))
        .expect("query");
    assert_ne!(
        bogus["accepted"],
        json!(true),
        "a typo'd filter is rejected, never silently unfiltered: {bogus}"
    );
}

/// `files` is a VOCABULARY read, so it defaults to `all`: a wikilink into a
/// dependency is legal (only a TYPE crossing gates on a declared dep), and a
/// consumer completing `[[` must be able to offer a target the validator
/// accepts. Defaulting to `own`, as the actionability reads do, would hide
/// exactly those targets.
#[test]
fn files_defaults_to_all_and_narrows_on_request() {
    let (_dir, root) = multi_member_kb();
    let mut h = harness(&root);

    // `v` is the entry repo: its own `.arsumbris/` engine-schema files are
    // catalogued and are resolvable wikilink targets, so they belong here.
    let default = h.payload("files", json!({}));
    assert_eq!(
        repos_of(&default),
        vec!["app", "base", "v"],
        "the default spans the dependency, unlike overview's own-scoped fields",
    );

    let own = h.payload("files", json!({ "scope": "own" }));
    assert_eq!(
        repos_of(&own),
        vec!["app", "v"],
        "explicit own keeps the entry and its edit members, drops the consumed one",
    );

    let scoped = h.payload("files", json!({ "repo": "base" }));
    assert_eq!(repos_of(&scoped), vec!["base"], "repo selects one member");
}

/// The stem is what a bare `[[name]]` resolves by, so serving it saves every
/// consumer re-deriving the strip-ONE-extension rule ([[type reference::au-type-system]]).
#[test]
fn a_files_entry_carries_the_stem_a_bare_wikilink_resolves_by() {
    let (_dir, root) = multi_member_kb();
    let mut h = harness(&root);

    let files = h.payload("files", json!({}));
    let entries = files.as_array().unwrap();

    let hub = entries
        .iter()
        .find(|f| f["path"].as_str().unwrap().ends_with("/app-hub.md"))
        .expect("the hub is catalogued");
    assert_eq!(hub["stem"], "app-hub", "one extension stripped");
    assert_eq!(hub["kind"], "instance");
    assert_eq!(hub["repo"], "app");

    // A type-def keeps the `.type` tail in its stem, which is exactly why a
    // wikilink by TYPE-NAME needs its own alias and cannot lean on the stem.
    let def = entries
        .iter()
        .find(|f| f["path"].as_str().unwrap().ends_with("/task.type.yaml"))
        .expect("the type-def is catalogued");
    assert_eq!(def["stem"], "task.type");
    assert_eq!(def["kind"], "type-def");

    // Every listed stem resolves back to its own file, so the served value is
    // usable as a bare target rather than merely informative.
    let resolved = h.payload("resolve_target", json!({ "target": hub["stem"] }));
    assert_eq!(resolved["path"], hub["path"], "the stem round-trips");
}

/// `limit` / `offset` page the path-sorted set, matching the rest of the
/// catalog's paging reads.
#[test]
fn files_pages_the_path_sorted_set() {
    let (_dir, root) = multi_member_kb();
    let mut h = harness(&root);

    let all = h.payload("files", json!({}));
    let all = all.as_array().unwrap();
    assert!(all.len() > 3, "the fixture has enough files to page");

    let paths: Vec<&str> = all.iter().map(|f| f["path"].as_str().unwrap()).collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(paths, sorted, "path-sorted");

    let page = h.payload("files", json!({ "limit": 2, "offset": 1 }));
    let page = page.as_array().unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0]["path"], all[1]["path"]);
    assert_eq!(page[1]["path"], all[2]["path"]);
}

/// A typo'd filter must not silently answer with the unfiltered set.
#[test]
fn files_rejects_an_unknown_arg() {
    let (_dir, root) = multi_member_kb();
    let mut h = harness(&root);
    let resp = h
        .client
        .query(&json!({ "read": "files", "scpe": "own" }))
        .expect("query");
    assert_eq!(
        resp["type"], "error",
        "an unknown arg is an error frame: {resp}"
    );
}
