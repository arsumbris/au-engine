//! Snapshot tests over `scenarios/<slug>/` — each scenario's `scenario.yaml`
//! `command:` selects which engine projection to snapshot (graph / validate /
//! candidates / subtypes). The output is the engine's daemon-visible shape: the
//! same `wire::introspect_*` projections the daemon serves over the socket.
//!
//! `subtypes` additionally reads a `base:` field naming the base type to query,
//! the one argument-carrying command.
//!
//! A new scenario shows up automatically: drop a `scenarios/<slug>/` with a
//! `scenario.yaml` and re-run the suite — insta will produce a pending
//! snapshot for `cargo insta review` to accept.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use au_diagnostics::Severity;
use au_parser::RealFileSystem;

/// Serialize a `Severity` to the same lowercase token scenario.yaml uses
/// (`error` / `drift` / `warning` / `hint`) so the strict-check test can
/// compare engine emit to parsed expectations as plain `(code, severity-str)`
/// pairs — avoids needing `Ord` on `Severity` itself.
fn severity_token(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Drift => "drift",
        Severity::Warning => "warning",
        Severity::Hint => "hint",
    }
}

/// Codes the scenario corpus suppresses from the compared / snapshotted stream.
///
/// `repo-missing-readme` fires on every entry repo that lacks a root README, which
/// is the default state of nearly every curated fixture. It is an entry-level
/// obligation orthogonal to what each fixture tests, so surfacing it here would
/// bury every scenario's actual point under one uniform warning. It is covered by
/// a dedicated au-engine integration test instead.
const SCENARIO_SUPPRESSED_CODES: &[&str] = &["repo-missing-readme"];

fn is_suppressed(d: &au_diagnostics::Diagnostic) -> bool {
    SCENARIO_SUPPRESSED_CODES.contains(&d.code.0.as_ref())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn scrub(s: String, root: &Path) -> String {
    s.replace(root.to_str().unwrap(), "<root>")
}

/// Read the `command:` field out of `scenario.yaml` without pulling in a
/// full YAML parser — the file is conventional and trivially line-shaped.
fn read_command(scenario_yaml: &Path) -> String {
    let src = fs::read_to_string(scenario_yaml).expect("read scenario.yaml");
    for line in src.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("command:") {
            return rest.trim().to_string();
        }
    }
    panic!(
        "scenario.yaml missing 'command:' line at {}",
        scenario_yaml.display()
    );
}

/// Read an optional top-level scalar field (e.g. `base:`) out of `scenario.yaml`,
/// the same line-shaped read as `read_command`. `None` when the field is absent.
fn read_optional_field(scenario_yaml: &Path, key: &str) -> Option<String> {
    let src = fs::read_to_string(scenario_yaml).expect("read scenario.yaml");
    let prefix = format!("{key}:");
    for line in src.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Project a scenario's built knowledge base into the engine's daemon-visible JSON, the
/// `command:` selecting which read's shape to freeze: `graph` snapshots the
/// type graph, `validate` the per-instance introspection, `candidates` the
/// implicit-identity scan, `types` the `types` read (each def's `parents[]`
/// verbatim; an optional `repo:` field scopes it to one member). Diagnostics
/// ride every shape, the way the daemon
/// pairs the `diagnostics` read with each. The projections are exactly
/// `wire::introspect_kb_*`, the ones the daemon serves.
fn render_scenario_engine(scenario_dir: &Path, command: &str, root: &Path) -> String {
    let kb = au_engine::build(scenario_dir, &RealFileSystem).expect("engine build");
    let payload = match command {
        "validate" => serde_json::json!({
            "diagnostics": kb.diagnostics().filter(|d| !is_suppressed(d)).collect::<Vec<_>>(),
            "instances": {
                "count": kb.instances.size(),
                "aborted_at_load": kb.any_aborted(),
                "entries": au_engine::wire::introspect_kb_instances(&kb),
            },
        }),
        "graph" => serde_json::json!({
            "diagnostics": kb.diagnostics().filter(|d| !is_suppressed(d)).collect::<Vec<_>>(),
            "graph": au_engine::wire::introspect_kb_graph(&kb),
        }),
        "candidates" => serde_json::json!({
            "diagnostics": kb.diagnostics().filter(|d| !is_suppressed(d)).collect::<Vec<_>>(),
            "candidates": {
                "aborted_at_load": kb.any_aborted(),
                // The full, unpaged projection the daemon serves for an unarged
                // `candidates` read; the untagged Full variant renders as the
                // same array the old `introspect_kb_candidates` produced.
                "files": au_engine::wire::introspect_kb_candidates_paged(&kb, 0, None, false),
            },
        }),
        "candidate_counts" => serde_json::json!({
            "diagnostics": kb.diagnostics().filter(|d| !is_suppressed(d)).collect::<Vec<_>>(),
            "candidate_counts": au_engine::wire::introspect_candidate_counts(&kb),
        }),
        "instance_counts" => serde_json::json!({
            "diagnostics": kb.diagnostics().filter(|d| !is_suppressed(d)).collect::<Vec<_>>(),
            "instance_counts": au_engine::wire::introspect_instance_counts(&kb, None, au_engine::wire::TypeScope::all()),
        }),
        "subtypes" => {
            let base = read_optional_field(&scenario_dir.join("scenario.yaml"), "base")
                .expect("a `subtypes` scenario needs a `base:` field naming the base type");
            serde_json::json!({
                "diagnostics": kb.diagnostics().filter(|d| !is_suppressed(d)).collect::<Vec<_>>(),
                "subtypes": au_engine::wire::introspect_subtypes(&kb, &base, au_engine::wire::TypeScope::all()),
            })
        }
        "types" => {
            // The `types` read the daemon serves, so the snapshot freezes each
            // def's `parents[]` verbatim — a cross-repo `extends:` parent reads
            // `name::repo`. An optional `repo:` field scopes to one member (the
            // reporter's `readTypes(client, { repo })`, and keeps the snapshot to
            // that member's own defs, off the hardwired `au.engine.*` set);
            // absent, it is the whole-workspace read.
            let repo = read_optional_field(&scenario_dir.join("scenario.yaml"), "repo");
            let types = match repo {
                Some(r) => au_engine::wire::introspect_repo_types_paged(
                    &kb, &r, 0, None, false, au_engine::wire::TypeScope::all(),
                )
                .unwrap_or_else(|| panic!("`types` scenario names unknown repo '{r}'")),
                None => au_engine::wire::introspect_workspace_types_paged(
                    &kb, 0, None, false, au_engine::wire::TypeScope::all(),
                ),
            };
            serde_json::json!({
                "diagnostics": kb.diagnostics().filter(|d| !is_suppressed(d)).collect::<Vec<_>>(),
                "types": types,
            })
        }
        other => panic!(
            "unknown scenario command '{}' (expected 'validate', 'graph', 'candidates', 'candidate_counts', 'instance_counts', 'subtypes', or 'types')",
            other
        ),
    };
    let rendered = serde_json::to_string_pretty(&payload).expect("payload serializes");
    scrub(rendered, root)
}

#[test]
fn scenarios_snapshot() {
    let root = workspace_root();
    let scenarios_dir = root.join("scenarios");

    let mut entries: Vec<PathBuf> = fs::read_dir(&scenarios_dir)
        .expect("read scenarios dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    entries.sort();

    assert!(!entries.is_empty(), "no scenarios found under scenarios/");

    // Every scenario is asserted, even after one diverges.
    //
    // A bare `assert_snapshot!` in this loop panics on the FIRST divergence, so
    // every later fixture goes unrun and unwritten. N diverging fixtures then
    // cost N full gate rounds, each revealing exactly one more. Catching per
    // scenario keeps the loop going, so insta writes every `.snap.new` in ONE
    // run and the gate's PENDING SNAPSHOTS block lists them all.
    //
    // The panic payload is not swallowed: the default hook still prints insta's
    // diff for each divergence as it happens. This only defers the FAILURE.
    let mut diverged: Vec<String> = Vec::new();

    for scenario_dir in entries {
        let slug = scenario_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let scenario_yaml = scenario_dir.join("scenario.yaml");
        if !scenario_yaml.exists() {
            continue;
        }
        let command = read_command(&scenario_yaml);
        let rendered = render_scenario_engine(&scenario_dir, &command, &root);
        let reported = slug.clone();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            insta::assert_snapshot!(slug, rendered);
        }));
        if outcome.is_err() {
            diverged.push(reported);
        }
    }

    assert!(
        diverged.is_empty(),
        "{} scenario snapshot(s) diverged: {}\n\
         each wrote a `.snap.new` beside its `.snap` — review the diffs, then accept with\n\
         `cargo insta accept` (or move each `.snap.new` over its `.snap`)",
        diverged.len(),
        diverged.join(", ")
    );
}

/// What `scenario.yaml`'s `expected:` block declares.
///
/// `Clean` ⇔ `clean: true` (engine MUST emit zero diagnostics).
/// `Codes(set)` ⇔ `codes: [...]` (engine emit set MUST equal `set` —
/// strict, by `(code, severity)` pair; multiplicities are folded away).
/// `Unspecified` ⇔ neither — the scenario is skipped by this check.
#[derive(Debug)]
enum ExpectedCodes {
    Clean,
    Codes(BTreeSet<(String, String)>),
    Unspecified,
}

/// Parse the `expected:` block from `scenario.yaml`. The format is
/// rigid enough to line-scan without pulling in a YAML parser; the two
/// shapes that exist in the corpus are `expected:\n  clean: true` and
/// `expected:\n  codes:\n    - { code: X, severity: Y }` (one entry per
/// line). Anything else returns `Unspecified`.
fn parse_expected_codes(scenario_yaml: &Path) -> ExpectedCodes {
    let src = fs::read_to_string(scenario_yaml).expect("read scenario.yaml");
    let mut lines = src.lines();
    while let Some(line) = lines.next() {
        if line.trim_start().starts_with("expected:") {
            // Collect indented continuation lines (anything starting with
            // whitespace; first non-indented line ends the block).
            let mut block: Vec<&str> = Vec::new();
            for next in lines.by_ref() {
                if next.is_empty() {
                    continue;
                }
                if next.starts_with(' ') || next.starts_with('\t') {
                    block.push(next);
                } else {
                    break;
                }
            }
            if block.iter().any(|l| l.trim() == "clean: true") {
                return ExpectedCodes::Clean;
            }
            if block.iter().any(|l| l.trim() == "codes:") {
                let mut set: BTreeSet<(String, String)> = BTreeSet::new();
                for entry in &block {
                    let trimmed = entry.trim();
                    let Some(rest) = trimmed.strip_prefix("- {") else {
                        continue;
                    };
                    let rest = rest.trim_end_matches('}').trim();
                    let mut code: Option<String> = None;
                    let mut sev: Option<String> = None;
                    for pair in rest.split(',') {
                        let pair = pair.trim();
                        if let Some(v) = pair.strip_prefix("code:") {
                            code = Some(v.trim().to_string());
                        } else if let Some(v) = pair.strip_prefix("severity:") {
                            let token = v.trim();
                            if !matches!(token, "error" | "drift" | "warning" | "hint") {
                                panic!(
                                    "unknown severity '{}' in {}",
                                    token,
                                    scenario_yaml.display()
                                );
                            }
                            sev = Some(token.to_string());
                        }
                    }
                    if let (Some(c), Some(s)) = (code, sev) {
                        set.insert((c, s));
                    }
                }
                return ExpectedCodes::Codes(set);
            }
            return ExpectedCodes::Unspecified;
        }
    }
    ExpectedCodes::Unspecified
}

/// Run the scenario against the engine and collect the `(code, severity)` pairs
/// it emits. One pipeline now: `au_engine::build` produces the full diagnostic
/// stream the daemon's `diagnostics` read serves. The graph scenarios are
/// type-def-only, so the full build emits exactly the vocabulary diagnostics
/// the old graph-only pipeline did. Mirrors what a diagnostics consumer inspects
/// when it cross-checks scenarios.
fn collect_emitted_codes(scenario_dir: &Path, _command: &str) -> BTreeSet<(String, String)> {
    let kb = au_engine::build(scenario_dir, &RealFileSystem).expect("engine build");
    kb.diagnostics()
        .filter(|d| !is_suppressed(d))
        .cloned()
        .map(|d| {
            (
                d.code.0.into_owned(),
                severity_token(d.severity).to_string(),
            )
        })
        .collect()
}

/// Strict cross-check: engine emit matches `scenario.yaml`'s declared
/// `expected:` block.
///
/// Snapshot tests freeze the formatted output but accept whatever's
/// there — that's how a noisy `malformed-attribution-marker` warning
/// can slip past a snapshot that accepts whatever is there. This test enforces
/// the scenario's stated intent directly, by `(code, severity)` set
/// equality. Collects all mismatches before failing so the report
/// names every drifting scenario in one run.
#[test]
fn scenarios_emit_expected_codes() {
    let root = workspace_root();
    let scenarios_dir = root.join("scenarios");

    let mut entries: Vec<PathBuf> = fs::read_dir(&scenarios_dir)
        .expect("read scenarios dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    entries.sort();
    assert!(!entries.is_empty(), "no scenarios found under scenarios/");

    let mut failures: Vec<String> = Vec::new();
    for scenario_dir in entries {
        let slug = scenario_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let scenario_yaml = scenario_dir.join("scenario.yaml");
        if !scenario_yaml.exists() {
            continue;
        }
        let command = read_command(&scenario_yaml);
        let expected = parse_expected_codes(&scenario_yaml);
        let actual = collect_emitted_codes(&scenario_dir, &command);
        match expected {
            ExpectedCodes::Clean => {
                if !actual.is_empty() {
                    failures.push(format!(
                        "{slug}: declares `clean: true` but engine emitted {actual:?}"
                    ));
                }
            }
            ExpectedCodes::Codes(set) => {
                let missing: Vec<_> = set.difference(&actual).cloned().collect();
                let surprises: Vec<_> = actual.difference(&set).cloned().collect();
                if !missing.is_empty() || !surprises.is_empty() {
                    failures.push(format!(
                        "{slug}: missing={missing:?} surprises={surprises:?}"
                    ));
                }
            }
            ExpectedCodes::Unspecified => {}
        }
    }
    if !failures.is_empty() {
        panic!(
            "{} scenario(s) drift between `expected:` and engine emit:\n  {}",
            failures.len(),
            failures.join("\n  ")
        );
    }
}
