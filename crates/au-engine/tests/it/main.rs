//! The au-engine integration suite as one linked binary.
//!
//! Every former `tests/<name>.rs` is a submodule here, so a core/engine edit
//! relinks one binary instead of ~32. Run one former file with
//! `cargo test -p au-engine --test it <name>::`.

/// Make a directory a folder-repo: the engine's entry MUST carry a valid, named
/// `.arsumbris/repo.yaml`, else `build` refuses it. A single-repo fixture seeds
/// its build root with this; a multi-repo fixture also writes a
/// `.arsumbris/workspace.yaml` composing its members. Idempotent.
#[allow(dead_code)]
pub fn seed_repo(root: &std::path::Path) {
    std::fs::create_dir_all(root.join(".arsumbris")).unwrap();
    // Self-describe so the seeded repo.yaml carries no `engine-schema-type-unwritten`
    // drift, matching the engine-writer norm.
    std::fs::write(
        root.join(".arsumbris/repo.yaml"),
        "name: v\ntype: au.engine.repo::au-engine\n",
    )
    .unwrap();
    // Self-describe through a root README too, so the seeded repo carries no
    // `repo-missing-readme` warning, matching a well-formed repo.
    seed_readme(root);
}

/// Write a conformant root `README.md`: self-declares `au.engine.readme` and
/// carries the `Repo Overview` section with its three required sub-sections.
/// Used by [`seed_repo`] and by multi-repo fixtures for their member roots
/// (each an `edit` member on the hook for one).
#[allow(dead_code)]
pub fn seed_readme(root: &std::path::Path) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("README.md"),
        "---\ntype: au.engine.readme::au-engine\ntldr: t\n---\n\n# Repo Overview\n\n## What this is\n\nt\n\n## How to use this\n\nt\n\n## How to extend this\n\nt\n",
    )
    .unwrap();
}

/// Like [`seed_repo`] but the entry composes the named `members` (each a repo
/// subdirectory) via `.arsumbris/workspace.yaml`, listing the entry (`v`) plus
/// the members in `edit`. For a multi-repo fixture whose entry is the parent of
/// its member repos.
#[allow(dead_code)]
pub fn seed_workspace(root: &std::path::Path, members: &[&str]) {
    seed_repo(root);
    let mut edit = String::from("edit:\n  - v\n");
    for m in members {
        edit.push_str(&format!("  - {m}\n"));
    }
    edit.push_str("type: au.engine.workspace::au-engine\n");
    std::fs::write(root.join(".arsumbris/workspace.yaml"), edit).unwrap();
}

mod backlinks;
mod build;
mod catalog_reads;
mod commit_meta;
mod config_channel;
mod config_write;
mod cross_repo_body_scan;
mod cross_repo_brands;
mod cross_repo_claim_reads;
mod cross_repo_location;
mod cross_repo_mutation_guards;
mod cross_repo_references;
mod cross_repo_typed_references;
mod device_config;
mod diagnostics_scope;
mod divergent_body_contribution;
mod edge_direction_agreement;
mod engine;
mod engine_schema;
mod envelope;
mod field_key_span;
mod file_history;
mod instances_of;
mod malformed_claim_edges;
mod member_reads;
mod mutate;
mod neighborhood;
mod nested_record_mutations;
mod note_reads;
mod overview;
mod per_repo_graphs;
mod perf_baseline;
mod pins;
mod port_contracts;
mod preview_mutation;
mod readme;
mod recent_commits;
mod reference_reads;
mod references_out_kinds;
mod register_wire;
mod repo_consistency;
mod repo_local_references;
mod repo_name_collision;
mod repo_scoped_reads;
mod required_subtype_meta;
mod resolve_e2e;
mod resolve_wire;
mod scale_fuzz;
mod scope_management;
mod scoped_orientation;
mod semantic_tokens;
mod serve;
mod subscribe;
mod subtypes_read;
mod type_closure;
mod type_def_classification;
mod type_graph_reads;
mod type_reads;
mod type_scope;
mod value_layer_references;
mod value_validate;
mod wire_fixtures;
mod workspace_assembly;
mod workspace_type_reads;
mod write_gate;
