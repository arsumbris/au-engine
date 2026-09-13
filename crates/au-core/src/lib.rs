//! Type-graph, closure resolution, validation, and candidate scanning.

pub mod body;
pub mod body_validate;
pub mod candidates;
pub mod canonical;
pub mod closure;
pub mod closure_id;
pub mod codes;
pub mod graph;
pub mod instance;
pub mod load_checks;
pub mod location;
pub mod location_check;
pub mod meta;
pub mod provenance;
pub mod record_targets;
pub mod resolution;
pub mod typedef;
pub mod validate;

pub use body::{
    is_body_declaring, parse_body_value, splice_effective_body, BodyItem, BodyTemplate, FieldClaim,
    FillsContract,
};
pub use body_validate::{
    collect_body_typed_block_claims, collect_body_typed_block_seeds, compute_section_presence,
    validate_body, validate_docstring_links, SectionPresenceInfo,
};
pub use candidates::{rank, scan, scan_top_level, Candidate, CandidateScope};
pub use canonical::{canonical_form, CanonicalHash};
pub use closure::{
    closure_of, effective_shape, effective_shape_resolved, field_type_refs, folded_closure_ids,
    EffectiveShape, EffectiveShapeError, FieldOrigin, OriginId, OriginInfo,
};
pub use closure_id::ClosureHash;
pub use graph::{build_graph, GraphBuildResult, TypeGraph};
pub use instance::{
    parse_block_record, parse_instance, parse_instance_stamped, parse_note, DocOrigin,
    DocstringLink, InlineValue, Instance, InstanceField, InstanceParseResult, InstanceValue,
    NavLink, Note, SequenceElement, TypeClaim,
};
pub use load_checks::{
    check_redundant_claims, is_valid_type_name, run_body_typing_checks, run_graph_structure_checks,
    run_inheritance_checks, run_location_checks,
};
pub use meta::lookup_meta;
pub use provenance::{
    effective_values, elaborate_fields, Contribution, ContributionValue, Location, Surface,
    TupleElement, ValueContainer,
};
pub use record_targets::{
    collect_record_target_occurrences, collect_record_targets, enumerate_nested_records,
    locate_field_path, record_slot_admits_reference_at, shape_is_inline_or_reference,
    slot_shape_at, FieldSpan, Located, LocatedRecord, LocatedSequence, NestedRecord, PathSegment,
    RecordTarget, RecordTargets, ValueKind,
};
pub use resolution::{
    cross_repo_type_chain_cycles, PeerGraphResolver, ResolutionGraph, ResolvedNode, TypeId,
};
pub use typedef::{
    parse_type_def, type_name_from_path, FieldDecl, FieldName, MetaBlock, ParentClaim,
    ParentClaimForm, ParseResult, TypeDef, TypeName, TypeNameClaim,
};
pub use validate::{
    unmet_required_meta, validate, validate_meta_bodies, CrossRepoResolver, CrossRepoTarget,
    MapRefData, MetaMarker, PeerType, QualifiedDemand, RefData, UnmetRequiredMeta, ValidateContext,
};
