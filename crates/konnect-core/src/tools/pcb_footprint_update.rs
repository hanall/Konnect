use crate::mcp::protocol::{CallToolResult, ToolContent};
use crate::tool;
use crate::tools::{
    pcb_board::{attempt_ipc_write, BoardWrite},
    ToolContext, ToolDef,
};
use anyhow::{bail, Context, Result};
use konnect_ipc::gen::kiapi;
use prost::Message;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use konnect_sexp::writer::{
    apply_edits, find_direct_child_blocks, read_consistent, write_atomic_if_unchanged,
};
use konnect_sexp::SexpEdit;

#[derive(Debug, Clone)]
struct LibraryFootprint {
    library_id: String,
    definition: kiapi::board::types::Footprint,
    attributes: kiapi::board::types::FootprintAttributes,
    datasheet: Option<String>,
    description_field: Option<String>,
    properties: Vec<kiapi::board::types::Field>,
    pads: Vec<konnect_ipc::IpcPadDefinition>,
    graphics: Vec<konnect_ipc::IpcGraphicDefinition>,
    models: Vec<kiapi::board::types::Footprint3DModel>,
}

#[derive(Debug)]
pub(crate) struct ParsedLibraryProperties {
    pub(crate) datasheet: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) custom: Vec<kiapi::board::types::Field>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum ChangedDomain {
    Pads,
    Graphics,
    Attributes,
    Metadata,
    Models,
    ObjectGraph,
}

#[derive(Debug)]
struct PreparedUpdate {
    item: prost_types::Any,
    changed_domains: BTreeSet<ChangedDomain>,
    preserved: PreservedState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct UpdateFilters {
    references: Option<BTreeSet<String>>,
    library_ids: Option<BTreeSet<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArgumentError {
    field: String,
    reason: String,
}

impl std::fmt::Display for ArgumentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.field, self.reason)
    }
}

impl std::error::Error for ArgumentError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PlanStatus {
    Ready,
    Noop,
    Conflict,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
struct CountPair {
    planned: usize,
    applied: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct UpdateCoverage {
    selected: CountPair,
    changed: CountPair,
    unchanged: CountPair,
    skipped_unlinked: CountPair,
    conflicts: CountPair,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PreservedState {
    position: bool,
    rotation: bool,
    layer: bool,
    locked: bool,
    kiid: bool,
    symbol_path: bool,
    pad_nets: bool,
    instance_overrides: bool,
}

impl PreservedState {
    /// Derived by comparing the rebuilt instance against the one read from
    /// the board — never asserted from policy. If a future change to
    /// `build_updated_instance` stops carrying one of these across, the
    /// response says so instead of echoing the intent.
    fn derive(
        current: &kiapi::board::types::FootprintInstance,
        updated: &kiapi::board::types::FootprintInstance,
        old_nets: &BTreeMap<String, kiapi::board::types::Net>,
    ) -> Self {
        let new_nets: BTreeMap<String, String> = updated
            .definition
            .iter()
            .flat_map(|definition| definition.items.iter())
            .filter(|item| item.type_url.ends_with("kiapi.board.types.Pad"))
            .filter_map(|item| kiapi::board::types::Pad::decode(item.value.as_slice()).ok())
            .filter_map(|pad| pad.net.map(|net| (pad.number, net.name)))
            .collect();
        Self {
            position: updated.position == current.position,
            rotation: updated.orientation == current.orientation,
            layer: updated.layer == current.layer,
            locked: updated.locked == current.locked,
            kiid: updated.id == current.id,
            symbol_path: updated.symbol_path == current.symbol_path
                && updated.symbol_sheet_name == current.symbol_sheet_name
                && updated.symbol_sheet_filename == current.symbol_sheet_filename,
            pad_nets: old_nets.iter().all(|(number, net)| {
                new_nets.get(number).map(String::as_str) == Some(net.name.as_str())
            }),
            instance_overrides: updated.overrides == current.overrides,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PlannedUpdate {
    reference: String,
    library_id: String,
    changed_domains: BTreeSet<ChangedDomain>,
    preserved: PreservedState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct UpdateDiagnostic {
    code: String,
    message: String,
    reference: Option<String>,
}

#[derive(Debug)]
struct UpdatePlan {
    status: PlanStatus,
    plan_revision: String,
    coverage: UpdateCoverage,
    changes: Vec<PlannedUpdate>,
    diagnostics: Vec<UpdateDiagnostic>,
    prepared_items: Vec<prost_types::Any>,
}

pub(crate) fn tool() -> ToolDef {
    tool!(
        "update_footprints_from_library",
        "Plan or atomically apply KiCad's Update Footprints from Library operation. The default \
         uses typed IPC on the live board. An explicit closed-board opt-in can instead preserve \
         the complete library S-expression object graph, including children KiCad 10 IPC cannot \
         represent. Apply requires the exact dry-run revision.",
        json!({
            "type": "object",
            "properties": {
                "board": {
                    "type": "string",
                    "description": "Open .kicad_pcb path whose placed footprints will be refreshed"
                },
                "references": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional exact reference allowlist; omitted means all eligible footprints"
                },
                "library_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional exact Library:Footprint allowlist"
                },
                "dry_run": {
                    "type": "boolean",
                    "default": true,
                    "description": "Plan and report changes without mutating the board"
                },
                "expected_plan_revision": {
                    "type": "string",
                    "description": "Required for apply; exact revision returned by the reviewed dry run"
                },
                "allow_closed_board_file": {
                    "type": "boolean",
                    "default": false,
                    "description": "When no live KiCad holds this board, allow a guarded revision-bound atomic saved-file update that preserves the complete library object graph"
                }
            },
            "required": ["board"],
            "additionalProperties": false
        }),
        |args, ctx| async move { handle_update_footprints_from_library(args, ctx).await }
    )
    .with_board_access(crate::tools::BoardAccess::LivePreferredWithFallback)
}

pub(crate) async fn handle_update_footprints_from_library(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> Result<CallToolResult> {
    use kiapi::common::types::KiCadObjectType as ObjectType;

    let board_path = crate::tools::get_path(args, "board")?;
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);
    let allow_closed_board_file = args["allow_closed_board_file"].as_bool().unwrap_or(false);
    let expected_revision = args["expected_plan_revision"].as_str().map(str::to_string);
    if !dry_run && expected_revision.is_none() {
        return Ok(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::InvalidArgument {
                field: "expected_plan_revision".to_string(),
                reason: "required when dry_run is false".to_string(),
            },
            "Apply requires the plan revision returned by a current dry run.",
        ));
    }
    if !board_path.is_file() {
        return Ok(CallToolResult::error_kind(
            crate::mcp::error::ToolErrorKind::FileNotFound {
                path: board_path.display().to_string(),
            },
            format!("Board file not found: {}", board_path.display()),
        ));
    }
    let filters = match parse_filters(args) {
        Ok(filters) => filters,
        Err(error) => {
            return Ok(CallToolResult::error_kind(
                crate::mcp::error::ToolErrorKind::InvalidArgument {
                    field: error.field.clone(),
                    reason: error.reason.clone(),
                },
                format!("Argument '{}' is invalid: {}", error.field, error.reason),
            ))
        }
    };

    let ipc_board_path = board_path.clone();
    let planning_board_path = board_path.clone();
    let live_filters = filters.clone();
    let live_expected_revision = expected_revision.clone();
    let operation = if dry_run {
        "footprint library update dry run"
    } else {
        "footprint library update apply"
    };
    let result = attempt_ipc_write(
        ctx,
        &board_path,
        operation,
        move |client| {
            let document = client.find_open_board(&ipc_board_path)?;
            let footprint_items =
                client.get_items_in(document.clone(), ObjectType::KotPcbFootprint)?;
            let nets = client.get_nets_in(document.clone())?;
            let net_codes = nets
                .into_iter()
                .map(|net| (net.name, net.netcode))
                .collect::<BTreeMap<_, _>>();
            let routed_nets = snapshot_routed_nets(client, document.clone())?;
            let mut plan = plan_updates(
                &planning_board_path,
                &footprint_items,
                &net_codes,
                &routed_nets,
                &live_filters,
            );

            if dry_run || plan.status == PlanStatus::Conflict {
                return Ok(plan_response(&plan, false));
            }
            if live_expected_revision.as_deref() != Some(plan.plan_revision.as_str()) {
                plan.status = PlanStatus::Conflict;
                plan.coverage.conflicts.planned += 1;
                plan.diagnostics.push(UpdateDiagnostic {
                    code: "stale_plan_revision".to_string(),
                    message: "The live board, selected libraries, or filters changed; rerun dry run and apply its new plan revision."
                        .to_string(),
                    reference: None,
                });
                plan.changes.clear();
                plan.prepared_items.clear();
                return Ok(plan_response(&plan, false));
            }
            if plan.status == PlanStatus::Noop {
                return Ok(plan_response(&plan, false));
            }

            let update_count = plan.prepared_items.len();
            let update_document = document.clone();
            client.run_commit_recovering_in(
                document,
                "Update footprints from libraries",
                |client| {
                    client.update_items_in(
                        update_document,
                        std::mem::take(&mut plan.prepared_items),
                    )
                },
            )?;
            plan.coverage.selected.applied = plan.coverage.selected.planned;
            plan.coverage.changed.applied = update_count;
            plan.coverage.unchanged.applied = plan.coverage.unchanged.planned;
            plan.coverage.skipped_unlinked.applied = plan.coverage.skipped_unlinked.planned;
            Ok(plan_response(&plan, true))
        },
    )
    .await?;

    Ok(match result {
        BoardWrite::Ipc(response) => response,
        BoardWrite::File(reason) if allow_closed_board_file => handle_closed_board_update(
            &board_path,
            &filters,
            dry_run,
            expected_revision.as_deref(),
            reason.evidence(),
        ),
        BoardWrite::File(reason) => preflight_conflict(format!(
            "{} The complete saved-file path was not enabled. Open the requested board in KiCad, \
             or retry with allow_closed_board_file=true after confirming the saved file is authoritative.",
            reason.premise()
        )),
        BoardWrite::Refused(result) => refused_live_update_result(result),
    })
}

fn refused_live_update_result(result: CallToolResult) -> CallToolResult {
    if matches!(
        crate::mcp::error::extract_error_kind(&result).as_deref(),
        Some("ipc_outcome_unknown" | "ipc_batch_recovered")
    ) {
        return result;
    }
    let message = result
        .content
        .into_iter()
        .find_map(|content| match content {
            ToolContent::Text { text } => Some(text),
            _ => None,
        })
        .unwrap_or_else(|| "KiCad refused the footprint library update".to_string());
    preflight_conflict(message)
}

fn snapshot_routed_nets(
    client: &konnect_ipc::KiCadIpcClient,
    document: kiapi::common::types::DocumentSpecifier,
) -> Result<BTreeSet<String>> {
    use kiapi::common::types::KiCadObjectType as ObjectType;

    let mut routed = BTreeSet::new();
    for item in client.get_items_in(document.clone(), ObjectType::KotPcbTrace)? {
        if let Ok(track) = kiapi::board::types::Track::decode(item.value.as_slice()) {
            if let Some(net) = track.net.filter(|net| !net.name.is_empty()) {
                routed.insert(net.name);
            }
        }
    }
    for item in client.get_items_in(document.clone(), ObjectType::KotPcbArc)? {
        if let Ok(arc) = kiapi::board::types::Arc::decode(item.value.as_slice()) {
            if let Some(net) = arc.net.filter(|net| !net.name.is_empty()) {
                routed.insert(net.name);
            }
        }
    }
    for item in client.get_items_in(document.clone(), ObjectType::KotPcbVia)? {
        if let Ok(via) = kiapi::board::types::Via::decode(item.value.as_slice()) {
            if let Some(net) = via.net.filter(|net| !net.name.is_empty()) {
                routed.insert(net.name);
            }
        }
    }
    if !client
        .get_items_in(document.clone(), ObjectType::KotPcbZone)?
        .is_empty()
    {
        routed.extend(
            client
                .get_nets_in(document)?
                .into_iter()
                .filter(|net| !net.name.is_empty())
                .map(|net| net.name),
        );
    }
    Ok(routed)
}

fn plan_response(plan: &UpdatePlan, applied: bool) -> CallToolResult {
    CallToolResult::json(&json!({
        "status": if applied {
            "applied"
        } else {
            match plan.status {
                PlanStatus::Ready => "ready",
                PlanStatus::Noop => "noop",
                PlanStatus::Conflict => "conflict",
            }
        },
        "plan_revision": plan.plan_revision,
        "coverage": {
            "transport": "live_kicad_ipc",
            "atomicity": "single_kicad_undo_commit",
            "selected": plan.coverage.selected,
            "changed": plan.coverage.changed,
            "unchanged": plan.coverage.unchanged,
            "skipped_unlinked": plan.coverage.skipped_unlinked,
            "conflicts": plan.coverage.conflicts
        },
        "changes": plan.changes,
        "diagnostics": plan.diagnostics,
        "undo": if applied {
            Some("Ctrl-Z reverses the whole footprint library update.")
        } else {
            None
        }
    }))
}

fn preflight_conflict(message: impl Into<String>) -> CallToolResult {
    CallToolResult::json(&json!({
        "status": "conflict",
        "coverage": {
            "transport": "live_kicad_ipc",
            "atomicity": "single_kicad_undo_commit",
            "selected": CountPair::default(),
            "changed": CountPair::default(),
            "unchanged": CountPair::default(),
            "skipped_unlinked": CountPair::default(),
            "conflicts": CountPair { planned: 1, applied: 0 }
        },
        "changes": [],
        "diagnostics": [{
            "code": "preflight_conflict",
            "message": message.into()
        }],
        "undo": null
    }))
}

fn parse_filters(args: &serde_json::Value) -> std::result::Result<UpdateFilters, ArgumentError> {
    fn parse(
        args: &serde_json::Value,
        field: &str,
    ) -> std::result::Result<Option<BTreeSet<String>>, ArgumentError> {
        let Some(value) = args.get(field) else {
            return Ok(None);
        };
        let values = value.as_array().ok_or_else(|| ArgumentError {
            field: field.to_string(),
            reason: "must be an array of strings when supplied".to_string(),
        })?;
        let parsed = values
            .iter()
            .map(|value| {
                let value = value.as_str().ok_or_else(|| ArgumentError {
                    field: field.to_string(),
                    reason: "must contain only strings".to_string(),
                })?;
                if value.is_empty() {
                    return Err(ArgumentError {
                        field: field.to_string(),
                        reason: "must not contain empty strings".to_string(),
                    });
                }
                Ok(value.to_string())
            })
            .collect::<std::result::Result<BTreeSet<_>, _>>()
            .map(Some)?;
        if field == "library_ids" {
            if let Some(invalid) = parsed.as_ref().and_then(|values| {
                values.iter().find(|value| {
                    value.split_once(':').is_none_or(|(nickname, entry)| {
                        nickname.is_empty()
                            || entry.is_empty()
                            || value.contains('/')
                            || value.contains('\\')
                    })
                })
            }) {
                return Err(ArgumentError {
                    field: field.to_string(),
                    reason: format!(
                        "'{invalid}' must use a non-empty Library:Footprint identifier"
                    ),
                });
            }
        }
        Ok(parsed)
    }

    Ok(UpdateFilters {
        references: parse(args, "references")?,
        library_ids: parse(args, "library_ids")?,
    })
}

#[derive(Debug, Clone)]
struct RawChild {
    tag: String,
    block: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClosedPadInvariant {
    number: String,
    net: Option<String>,
    uuid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClosedFootprintInvariant {
    reference: String,
    footprint_uuid: String,
    placement: String,
    layer: String,
    protected_properties: BTreeMap<String, String>,
    pads: Vec<ClosedPadInvariant>,
    points: Vec<String>,
}

#[derive(Debug)]
struct ClosedPreparedUpdate {
    replacement: String,
    invariant: ClosedFootprintInvariant,
}

#[derive(Debug)]
struct ClosedBoardPlan {
    target_path: String,
    status: PlanStatus,
    plan_revision: String,
    coverage: UpdateCoverage,
    changes: Vec<PlannedUpdate>,
    diagnostics: Vec<UpdateDiagnostic>,
    original: String,
    replacement: String,
    invariants: Vec<ClosedFootprintInvariant>,
}

fn handle_closed_board_update(
    board_path: &Path,
    filters: &UpdateFilters,
    dry_run: bool,
    expected_revision: Option<&str>,
    fallback_reason: serde_json::Value,
) -> CallToolResult {
    let mut plan = match plan_closed_board_updates(board_path, filters) {
        Ok(plan) => plan,
        Err(error) => {
            return closed_preflight_conflict(
                format!("Closed-board footprint planning failed before mutation: {error:#}"),
                board_path,
                &fallback_reason,
            )
        }
    };

    if dry_run || plan.status == PlanStatus::Conflict {
        return closed_plan_response(&plan, false, &fallback_reason, None);
    }
    if expected_revision != Some(plan.plan_revision.as_str()) {
        plan.status = PlanStatus::Conflict;
        plan.coverage.conflicts.planned += 1;
        plan.changes.clear();
        plan.diagnostics.push(UpdateDiagnostic {
            code: "stale_plan_revision".to_string(),
            message: "The saved board, selected libraries, or filters changed; rerun dry run and apply its new plan revision."
                .to_string(),
            reference: None,
        });
        return closed_plan_response(&plan, false, &fallback_reason, None);
    }
    if plan.status == PlanStatus::Noop {
        return closed_plan_response(&plan, false, &fallback_reason, None);
    }

    match persist_closed_board_plan(board_path, &plan) {
        Ok(()) => {
            plan.coverage.selected.applied = plan.coverage.selected.planned;
            plan.coverage.changed.applied = plan.coverage.changed.planned;
            plan.coverage.unchanged.applied = plan.coverage.unchanged.planned;
            plan.coverage.skipped_unlinked.applied = plan.coverage.skipped_unlinked.planned;
            closed_plan_response(&plan, true, &fallback_reason, None)
        }
        Err(ClosedPersistFailure::BeforeMutation(reason)) => {
            plan.status = PlanStatus::Conflict;
            plan.coverage.conflicts.planned += 1;
            plan.diagnostics.push(UpdateDiagnostic {
                code: "closed_board_write_refused".to_string(),
                message: reason,
                reference: None,
            });
            closed_plan_response(&plan, false, &fallback_reason, Some("unchanged"))
        }
        Err(ClosedPersistFailure::RolledBack(reason)) => {
            plan.status = PlanStatus::Conflict;
            plan.coverage.conflicts.planned += 1;
            plan.diagnostics.push(UpdateDiagnostic {
                code: "closed_board_readback_rolled_back".to_string(),
                message: reason,
                reference: None,
            });
            closed_plan_response(&plan, false, &fallback_reason, Some("rolled_back"))
        }
        Err(ClosedPersistFailure::Uncertain(reason)) => {
            plan.status = PlanStatus::Conflict;
            plan.coverage.conflicts.planned += 1;
            plan.diagnostics.push(UpdateDiagnostic {
                code: "closed_board_persistence_uncertain".to_string(),
                message: reason,
                reference: None,
            });
            closed_plan_response(&plan, false, &fallback_reason, Some("uncertain"))
        }
    }
}

fn closed_preflight_conflict(
    message: impl Into<String>,
    board_path: &Path,
    fallback_reason: &serde_json::Value,
) -> CallToolResult {
    CallToolResult::json(&json!({
        "status": "conflict",
        "coverage": {
            "transport": "closed_board_file",
            "atomicity": "compare_and_swap_atomic_replace_with_readback_and_rollback",
            "selected": CountPair::default(),
            "changed": CountPair::default(),
            "unchanged": CountPair::default(),
            "skipped_unlinked": CountPair::default(),
            "conflicts": CountPair { planned: 1, applied: 0 }
        },
        "changes": [],
        "target": { "board": board_path, "source": "saved_file" },
        "diagnostics": [{
            "code": "closed_board_preflight_conflict",
            "message": message.into()
        }],
        "fallback_reason": fallback_reason,
        "recovery": "unchanged",
        "undo": null
    }))
}

fn closed_plan_response(
    plan: &ClosedBoardPlan,
    applied: bool,
    fallback_reason: &serde_json::Value,
    recovery: Option<&str>,
) -> CallToolResult {
    CallToolResult::json(&json!({
        "status": if applied {
            "applied"
        } else {
            match plan.status {
                PlanStatus::Ready => "ready",
                PlanStatus::Noop => "noop",
                PlanStatus::Conflict => "conflict",
            }
        },
        "plan_revision": plan.plan_revision,
        "coverage": {
            "transport": "closed_board_file",
            "atomicity": "compare_and_swap_atomic_replace_with_readback_and_rollback",
            "selected": plan.coverage.selected,
            "changed": plan.coverage.changed,
            "unchanged": plan.coverage.unchanged,
            "skipped_unlinked": plan.coverage.skipped_unlinked,
            "conflicts": plan.coverage.conflicts
        },
        "changes": plan.changes,
        "target": { "board": plan.target_path, "source": "saved_file" },
        "diagnostics": plan.diagnostics,
        "fallback_reason": fallback_reason,
        "recovery": recovery,
        "undo": null
    }))
}

fn plan_closed_board_updates(
    board_path: &Path,
    filters: &UpdateFilters,
) -> Result<ClosedBoardPlan> {
    #[derive(Debug)]
    struct Candidate {
        reference: String,
        library_id: String,
        start: usize,
        end: usize,
        block: String,
    }

    if std::fs::symlink_metadata(board_path)
        .context("failed to inspect the saved board path")?
        .file_type()
        .is_symlink()
    {
        bail!(
            "saved board path '{}' is a symlink; closed-board refresh requires the canonical regular file path so planning and persistence cannot target different directory entries",
            board_path.display()
        );
    }
    let original = read_consistent(board_path).context("failed to read a stable saved board")?;
    let target_path = board_path
        .canonicalize()
        .context("failed to establish the saved board identity")?;
    let root =
        konnect_sexp::parse_sexp(&original).context("saved board is not valid S-expression")?;
    if root.head() != Some("kicad_pcb") {
        bail!("saved document root is not kicad_pcb");
    }

    let mut candidates = Vec::new();
    let mut reference_counts = BTreeMap::<String, usize>::new();
    for (start, end) in find_direct_child_blocks(&original, "kicad_pcb") {
        let block = &original[start..end];
        let Ok(node) = konnect_sexp::parse_sexp(block) else {
            continue;
        };
        if node.head() != Some("footprint") {
            continue;
        }
        let reference = raw_footprint_reference(&node).unwrap_or_default();
        *reference_counts.entry(reference.clone()).or_insert(0) += 1;
        let library_id = node
            .get(1)
            .and_then(konnect_sexp::SexpNode::as_str)
            .unwrap_or_default()
            .to_string();
        candidates.push(Candidate {
            reference,
            library_id,
            start,
            end,
            block: block.to_string(),
        });
    }

    let mut diagnostics = Vec::new();
    if let Some(references) = filters.references.as_ref() {
        for reference in references {
            match reference_counts.get(reference).copied().unwrap_or(0) {
                0 => diagnostics.push(UpdateDiagnostic {
                    code: "reference_not_found".to_string(),
                    message: format!("footprint reference '{reference}' was not found"),
                    reference: Some(reference.clone()),
                }),
                count if count > 1 => diagnostics.push(UpdateDiagnostic {
                    code: "duplicate_reference".to_string(),
                    message: format!("footprint reference '{reference}' appears {count} times"),
                    reference: Some(reference.clone()),
                }),
                _ => {}
            }
        }
    } else {
        for (reference, count) in &reference_counts {
            if *count > 1 {
                diagnostics.push(UpdateDiagnostic {
                    code: "duplicate_reference".to_string(),
                    message: format!("footprint reference '{reference}' appears {count} times"),
                    reference: Some(reference.clone()),
                });
            }
        }
    }

    candidates.sort_by(|left, right| {
        left.reference
            .cmp(&right.reference)
            .then_with(|| left.library_id.cmp(&right.library_id))
    });
    let selected = candidates
        .into_iter()
        .filter(|candidate| {
            filters
                .references
                .as_ref()
                .is_none_or(|references| references.contains(&candidate.reference))
        })
        .filter(|candidate| {
            filters
                .library_ids
                .as_ref()
                .is_none_or(|ids| ids.contains(&candidate.library_id))
        })
        .collect::<Vec<_>>();

    let mut coverage = UpdateCoverage::default();
    coverage.skipped_unlinked.planned = selected
        .iter()
        .filter(|candidate| candidate.library_id.is_empty())
        .count();
    coverage.selected.planned = selected
        .iter()
        .filter(|candidate| !candidate.library_id.is_empty())
        .count();

    let mut hasher = Sha256::new();
    hasher.update(b"closed_board_lossless_v1\0");
    let target_bytes = target_path.as_os_str().as_encoded_bytes();
    hasher.update((target_bytes.len() as u64).to_le_bytes());
    hasher.update(target_bytes);
    hasher.update(original.as_bytes());
    hasher.update(serde_json::to_vec(filters).expect("filters serialize"));
    let mut changes = Vec::new();
    let mut edits = Vec::new();
    let mut invariants = Vec::new();

    for candidate in selected {
        if candidate.library_id.is_empty() {
            continue;
        }
        let library_path = match super::library::resolve_footprint_path(
            &candidate.library_id,
            board_path.parent(),
        ) {
            Ok(path) => path,
            Err(message) => {
                diagnostics.push(UpdateDiagnostic {
                    code: "footprint_library_resolution_failed".to_string(),
                    message,
                    reference: Some(candidate.reference),
                });
                continue;
            }
        };
        let source = match std::fs::read_to_string(&library_path) {
            Ok(source) => source,
            Err(error) => {
                diagnostics.push(UpdateDiagnostic {
                    code: "footprint_library_read_failed".to_string(),
                    message: format!("failed to read {}: {error}", library_path.display()),
                    reference: Some(candidate.reference),
                });
                continue;
            }
        };
        hasher.update(candidate.library_id.as_bytes());
        hasher.update(library_path.as_os_str().as_encoded_bytes());
        hasher.update(source.as_bytes());

        let prepared = match prepare_closed_board_footprint(
            &candidate.block,
            &candidate.library_id,
            &source,
            &library_path,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                diagnostics.push(UpdateDiagnostic {
                    code: "closed_board_footprint_conflict".to_string(),
                    message: format!("{error:#}"),
                    reference: Some(candidate.reference),
                });
                continue;
            }
        };

        if prepared.replacement == candidate.block {
            coverage.unchanged.planned += 1;
        } else {
            coverage.changed.planned += 1;
            changes.push(PlannedUpdate {
                reference: candidate.reference,
                library_id: candidate.library_id,
                changed_domains: BTreeSet::from([ChangedDomain::ObjectGraph]),
                preserved: PreservedState {
                    position: true,
                    rotation: true,
                    layer: true,
                    locked: true,
                    kiid: true,
                    symbol_path: true,
                    pad_nets: true,
                    instance_overrides: true,
                },
            });
            edits.push(SexpEdit::replace(
                candidate.start,
                candidate.end,
                prepared.replacement,
            ));
            invariants.push(prepared.invariant);
        }
    }

    if !diagnostics.is_empty() {
        coverage.conflicts.planned = diagnostics.len();
        coverage.changed.planned = 0;
        changes.clear();
        invariants.clear();
        return Ok(ClosedBoardPlan {
            target_path: target_path.display().to_string(),
            status: PlanStatus::Conflict,
            plan_revision: format!("{:x}", hasher.finalize()),
            coverage,
            changes,
            diagnostics,
            original: original.clone(),
            replacement: original,
            invariants,
        });
    }

    let replacement = apply_edits(original.clone(), edits);
    let replacement_root = konnect_sexp::parse_sexp(&replacement)
        .context("closed-board update would produce invalid S-expression")?;
    if replacement_root.head() != Some("kicad_pcb") {
        bail!("closed-board update would replace the board root");
    }
    validate_unique_document_uuids(&replacement_root)?;
    verify_closed_board_invariants(&replacement, &invariants)?;
    let status = if changes.is_empty() {
        PlanStatus::Noop
    } else {
        PlanStatus::Ready
    };
    Ok(ClosedBoardPlan {
        target_path: target_path.display().to_string(),
        status,
        plan_revision: format!("{:x}", hasher.finalize()),
        coverage,
        changes,
        diagnostics,
        original,
        replacement,
        invariants,
    })
}

fn validate_unique_document_uuids(root: &konnect_sexp::SexpNode) -> Result<()> {
    fn visit(node: &konnect_sexp::SexpNode, seen: &mut BTreeSet<String>) -> Result<()> {
        if matches!(node.head(), Some("uuid" | "tstamp")) {
            if let Some(value) = node.get(1).and_then(konnect_sexp::SexpNode::as_str) {
                if !seen.insert(value.to_string()) {
                    bail!("written board would contain duplicate UUID/tstamp '{value}'");
                }
            }
        }
        for child in node.children().unwrap_or_default() {
            visit(child, seen)?;
        }
        Ok(())
    }

    visit(root, &mut BTreeSet::new())
}

fn raw_direct_children(source: &str, root_tag: &str) -> Result<Vec<RawChild>> {
    let root = konnect_sexp::parse_sexp(source)
        .with_context(|| format!("invalid {root_tag} S-expression"))?;
    if root.head() != Some(root_tag) {
        bail!("expected {root_tag} root");
    }
    find_direct_child_blocks(source, root_tag)
        .into_iter()
        .map(|(start, end)| {
            let block = source[start..end].to_string();
            let child = konnect_sexp::parse_sexp(&block).context("invalid direct child")?;
            let tag = child.head().context("direct child has no tag")?.to_string();
            Ok(RawChild { tag, block })
        })
        .collect()
}

fn children_named(children: &[RawChild], tag: &str) -> Vec<String> {
    children
        .iter()
        .filter(|child| child.tag == tag)
        .map(|child| child.block.clone())
        .collect()
}

fn exactly_one_child(children: &[RawChild], tag: &str, subject: &str) -> Result<String> {
    let matches = children_named(children, tag);
    match matches.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("{subject} is missing its '{tag}' child"),
        _ => bail!("{subject} contains more than one '{tag}' child"),
    }
}

fn raw_footprint_reference(root: &konnect_sexp::SexpNode) -> Option<String> {
    root.find_all("property")
        .into_iter()
        .find(|property| {
            property.get(1).and_then(konnect_sexp::SexpNode::as_str) == Some("Reference")
        })
        .and_then(|property| property.get(2))
        .and_then(konnect_sexp::SexpNode::as_str)
        .or_else(|| {
            root.find_all("fp_text")
                .into_iter()
                .find(|text| {
                    text.get(1).and_then(konnect_sexp::SexpNode::as_str) == Some("reference")
                })
                .and_then(|text| text.get(2))
                .and_then(konnect_sexp::SexpNode::as_str)
        })
        .map(str::to_string)
}

fn property_name(block: &str) -> Result<String> {
    let node = konnect_sexp::parse_sexp(block).context("invalid property")?;
    node.get(1)
        .and_then(konnect_sexp::SexpNode::as_str)
        .map(str::to_string)
        .context("property is missing its name")
}

fn pad_number(block: &str) -> Result<String> {
    let node = konnect_sexp::parse_sexp(block).context("invalid pad")?;
    node.get(1)
        .and_then(konnect_sexp::SexpNode::as_str)
        .map(str::to_string)
        .context("pad is missing its number")
}

fn replace_direct_occurrence(
    source: &str,
    root_tag: &str,
    child_tag: &str,
    occurrence: usize,
    replacement: &str,
) -> Result<String> {
    let mut found = 0usize;
    for (start, end) in find_direct_child_blocks(source, root_tag) {
        let node = konnect_sexp::parse_sexp(&source[start..end])?;
        if node.head() != Some(child_tag) {
            continue;
        }
        if found == occurrence {
            return Ok(apply_edits(
                source.to_string(),
                vec![SexpEdit::replace(start, end, replacement)],
            ));
        }
        found += 1;
    }
    bail!("{root_tag} has no {child_tag} occurrence {occurrence}")
}

fn append_direct_child(source: &str, block: &str) -> Result<String> {
    let trimmed_end = source.trim_end().len();
    let close = source[..trimmed_end]
        .rfind(')')
        .context("root expression has no closing paren")?;
    Ok(apply_edits(
        source.to_string(),
        vec![SexpEdit::insert(close, format!("\n\t{block}"))],
    ))
}

fn replace_or_append_single_child(
    source: &str,
    root_tag: &str,
    child_tag: &str,
    replacement: &str,
) -> Result<String> {
    let count = raw_direct_children(source, root_tag)?
        .iter()
        .filter(|child| child.tag == child_tag)
        .count();
    match count {
        0 => append_direct_child(source, replacement),
        1 => replace_direct_occurrence(source, root_tag, child_tag, 0, replacement),
        _ => bail!("{root_tag} contains more than one '{child_tag}' child"),
    }
}

fn direct_child_block(source: &str, root_tag: &str, tag: &str) -> Result<Option<String>> {
    let matches = children_named(&raw_direct_children(source, root_tag)?, tag);
    match matches.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        _ => bail!("{root_tag} contains more than one '{tag}' child"),
    }
}

fn transplant_item_identity(mut replacement: String, current: &str) -> Result<String> {
    let current_tag = konnect_sexp::parse_sexp(current)?
        .head()
        .context("current item has no tag")?
        .to_string();
    let replacement_tag = konnect_sexp::parse_sexp(&replacement)?
        .head()
        .context("replacement item has no tag")?
        .to_string();
    if current_tag != replacement_tag {
        bail!("cannot transplant identity from '{current_tag}' to '{replacement_tag}'");
    }
    for tag in ["uuid", "tstamp"] {
        if let Some(block) = direct_child_block(current, &current_tag, tag)? {
            replacement =
                replace_or_append_single_child(&replacement, &replacement_tag, tag, &block)?;
        }
    }
    Ok(replacement)
}

fn one_graphic(block: &str) -> Result<konnect_ipc::IpcGraphicDefinition> {
    let source = format!("(footprint \"Guard\" (layer \"F.Cu\") {block})");
    let graphics = super::pcb_components::extract_graphic_definitions_without_properties(&source)?;
    match graphics.as_slice() {
        [graphic] => Ok(graphic.clone()),
        [] => bail!("graphic is hidden or has no typed semantic representation"),
        _ => bail!("one graphic block produced more than one semantic graphic"),
    }
}

fn graphic_semantically_equal(
    left: &konnect_ipc::IpcGraphicDefinition,
    right: &konnect_ipc::IpcGraphicDefinition,
) -> bool {
    use konnect_ipc::IpcGraphicDefinition as Graphic;
    let number = |a: f64, b: f64| (a - b).abs() <= 0.000_002;
    let point = |a: &(f64, f64), b: &(f64, f64)| number(a.0, b.0) && number(a.1, b.1);
    match (left, right) {
        (
            Graphic::Line {
                start: a_start,
                end: a_end,
                layer: a_layer,
                width: a_width,
            },
            Graphic::Line {
                start: b_start,
                end: b_end,
                layer: b_layer,
                width: b_width,
            },
        ) => {
            point(a_start, b_start)
                && point(a_end, b_end)
                && a_layer == b_layer
                && number(*a_width, *b_width)
        }
        (
            Graphic::Rect {
                start: a_start,
                end: a_end,
                layer: a_layer,
                width: a_width,
                filled: a_filled,
            },
            Graphic::Rect {
                start: b_start,
                end: b_end,
                layer: b_layer,
                width: b_width,
                filled: b_filled,
            },
        ) => {
            point(a_start, b_start)
                && point(a_end, b_end)
                && a_layer == b_layer
                && number(*a_width, *b_width)
                && a_filled == b_filled
        }
        (
            Graphic::Circle {
                center: a_center,
                end: a_end,
                layer: a_layer,
                width: a_width,
                filled: a_filled,
            },
            Graphic::Circle {
                center: b_center,
                end: b_end,
                layer: b_layer,
                width: b_width,
                filled: b_filled,
            },
        ) => {
            point(a_center, b_center)
                && point(a_end, b_end)
                && a_layer == b_layer
                && number(*a_width, *b_width)
                && a_filled == b_filled
        }
        (
            Graphic::Arc {
                start: a_start,
                mid: a_mid,
                end: a_end,
                layer: a_layer,
                width: a_width,
            },
            Graphic::Arc {
                start: b_start,
                mid: b_mid,
                end: b_end,
                layer: b_layer,
                width: b_width,
            },
        ) => {
            point(a_start, b_start)
                && point(a_mid, b_mid)
                && point(a_end, b_end)
                && a_layer == b_layer
                && number(*a_width, *b_width)
        }
        (
            Graphic::Poly {
                points: a_points,
                layer: a_layer,
                width: a_width,
                filled: a_filled,
            },
            Graphic::Poly {
                points: b_points,
                layer: b_layer,
                width: b_width,
                filled: b_filled,
            },
        ) => {
            a_points.len() == b_points.len()
                && a_points
                    .iter()
                    .zip(b_points)
                    .all(|(left, right)| point(left, right))
                && a_layer == b_layer
                && number(*a_width, *b_width)
                && a_filled == b_filled
        }
        (
            Graphic::Text {
                text: a_text,
                position: a_position,
                rotation: a_rotation,
                layer: a_layer,
                size: a_size,
                stroke_width_mm: a_stroke,
            },
            Graphic::Text {
                text: b_text,
                position: b_position,
                rotation: b_rotation,
                layer: b_layer,
                size: b_size,
                stroke_width_mm: b_stroke,
            },
        ) => {
            a_text == b_text
                && point(a_position, b_position)
                && number(*a_rotation, *b_rotation)
                && a_layer == b_layer
                && number(*a_size, *b_size)
                && number(*a_stroke, *b_stroke)
        }
        _ => false,
    }
}

fn match_existing_graphics(
    reference: &str,
    tag: &str,
    current: &[String],
    library: &[String],
) -> Result<Vec<(usize, usize)>> {
    let current = current
        .iter()
        .map(|block| one_graphic(block))
        .collect::<Result<Vec<_>>>()?;
    let library = library
        .iter()
        .map(|block| one_graphic(block))
        .collect::<Result<Vec<_>>>()?;
    let mut used = vec![false; library.len()];
    let mut matches = Vec::with_capacity(current.len());
    for (current_index, current_graphic) in current.iter().enumerate() {
        let Some(library_index) = library.iter().enumerate().find_map(|(index, candidate)| {
            (!used[index] && graphic_semantically_equal(current_graphic, candidate))
                .then_some(index)
        }) else {
            bail!(
                "footprint '{reference}' has a '{tag}' graphic that does not match the current library within 2 nm; its board-user versus stale-library ownership is ambiguous, so it was not replaced"
            );
        };
        used[library_index] = true;
        matches.push((current_index, library_index));
    }
    Ok(matches)
}

fn user_fp_text_identity(block: &str) -> Result<String> {
    let node = konnect_sexp::parse_sexp(block).context("invalid fp_text")?;
    if node.head() != Some("fp_text") {
        bail!("expected fp_text root");
    }
    let kind = node
        .get(1)
        .and_then(konnect_sexp::SexpNode::as_str)
        .context("fp_text is missing its kind")?;
    if kind != "user" {
        bail!(
            "closed-board refresh only preserves existing fp_text user annotations; found '{kind}'"
        );
    }
    node.get(2)
        .and_then(konnect_sexp::SexpNode::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .context("fp_text user has no non-empty text identity")
}

fn match_preserved_user_texts(
    reference: &str,
    current: &[String],
    library: &[String],
) -> Result<Vec<(usize, usize)>> {
    let mut current_by_text = BTreeMap::new();
    for (index, block) in current.iter().enumerate() {
        let text = user_fp_text_identity(block)?;
        if current_by_text.insert(text.clone(), index).is_some() {
            bail!(
                "footprint '{reference}' contains duplicate fp_text user identity '{text}'; ownership is ambiguous"
            );
        }
    }
    let mut library_by_text = BTreeMap::new();
    for (index, block) in library.iter().enumerate() {
        let text = user_fp_text_identity(block)?;
        if library_by_text.insert(text.clone(), index).is_some() {
            bail!(
                "library for footprint '{reference}' contains duplicate fp_text user identity '{text}'; ownership is ambiguous"
            );
        }
    }
    if current_by_text.keys().ne(library_by_text.keys()) {
        bail!(
            "footprint '{reference}' and its library have different fp_text user identities; addition, deletion, or content change is ambiguous"
        );
    }
    Ok(current_by_text
        .into_iter()
        .map(|(text, current_index)| (current_index, library_by_text[&text]))
        .collect())
}

#[derive(Debug, Clone)]
struct SemanticPoint {
    x: f64,
    y: f64,
    size: f64,
    layer: String,
}

fn one_point(block: &str) -> Result<SemanticPoint> {
    let node = konnect_sexp::parse_sexp(block).context("invalid point")?;
    if node.head() != Some("point") {
        bail!("expected point root");
    }
    for child in node.children().unwrap_or_default().iter().skip(1) {
        let tag = child.head().context("point contains a direct atom")?;
        if !matches!(tag, "at" | "size" | "layer" | "uuid" | "tstamp") {
            bail!("point contains unsupported clause '{tag}'");
        }
    }
    let at = node.find("at").context("point is missing at")?;
    let size = node.find("size").context("point is missing size")?;
    Ok(SemanticPoint {
        x: at.get_f64(1).context("point X is invalid")?,
        y: at.get_f64(2).context("point Y is invalid")?,
        size: size.get_f64(1).context("point size is invalid")?,
        layer: node
            .find_str("layer")
            .context("point layer is missing")?
            .to_string(),
    })
}

fn point_semantically_equal(left: &SemanticPoint, right: &SemanticPoint) -> bool {
    let number = |a: f64, b: f64| (a - b).abs() <= 0.000_002;
    number(left.x, right.x)
        && number(left.y, right.y)
        && number(left.size, right.size)
        && left.layer == right.layer
}

fn match_existing_points(
    reference: &str,
    current: &[String],
    library: &[String],
) -> Result<Vec<(usize, usize)>> {
    let current = current
        .iter()
        .map(|block| one_point(block))
        .collect::<Result<Vec<_>>>()?;
    let library = library
        .iter()
        .map(|block| one_point(block))
        .collect::<Result<Vec<_>>>()?;
    let mut used = vec![false; library.len()];
    let mut matches = Vec::with_capacity(current.len());
    for (current_index, current_point) in current.iter().enumerate() {
        let Some(library_index) = library.iter().enumerate().find_map(|(index, candidate)| {
            (!used[index] && point_semantically_equal(current_point, candidate)).then_some(index)
        }) else {
            bail!(
                "footprint '{reference}' contains a board point without a distinct semantic library match; point ownership is ambiguous"
            );
        };
        used[library_index] = true;
        matches.push((current_index, library_index));
    }
    Ok(matches)
}

fn one_pad(block: &str) -> Result<konnect_ipc::IpcPadDefinition> {
    let source = format!("(footprint \"Guard\" (layer \"F.Cu\") {block})");
    let pads = super::pcb_components::extract_pad_definitions(&source)?;
    match pads.as_slice() {
        [pad] => Ok(pad.clone()),
        _ => bail!("one pad block did not produce exactly one semantic pad"),
    }
}

fn pad_semantically_equal(
    left: &konnect_ipc::IpcPadDefinition,
    right: &konnect_ipc::IpcPadDefinition,
) -> bool {
    let number = |a: f64, b: f64| (a - b).abs() <= 0.000_002;
    let optional = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) => number(a, b),
        (None, None) => true,
        _ => false,
    };
    left.number == right.number
        && left.pad_type == right.pad_type
        && left.shape == right.shape
        && number(left.x, right.x)
        && number(left.y, right.y)
        && number(left.rotation, right.rotation)
        && number(left.size_x, right.size_x)
        && number(left.size_y, right.size_y)
        && optional(left.drill_x, right.drill_x)
        && optional(left.drill_y, right.drill_y)
        && left.drill_oval == right.drill_oval
        && left.layers == right.layers
        && number(left.roundrect_ratio, right.roundrect_ratio)
}

fn item_identity(block: &str) -> Result<Option<String>> {
    let tag = konnect_sexp::parse_sexp(block)?
        .head()
        .context("item has no tag")?
        .to_string();
    Ok(direct_child_block(block, &tag, "uuid")?.or(direct_child_block(block, &tag, "tstamp")?))
}

fn deterministic_child_uuid(seed: &str, tag: &str, occurrence: usize) -> String {
    let mut hash = Sha256::new();
    hash.update(b"konnect-closed-footprint-child-v1\0");
    hash.update(seed.as_bytes());
    hash.update([0]);
    hash.update(tag.as_bytes());
    hash.update([0]);
    hash.update(occurrence.to_le_bytes());
    let digest = hash.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

fn set_item_uuid(block: &str, uuid: &str) -> Result<String> {
    let tag = konnect_sexp::parse_sexp(block)?
        .head()
        .context("item has no tag")?
        .to_string();
    let mut edits = Vec::new();
    for (start, end) in find_direct_child_blocks(block, &tag) {
        let child = konnect_sexp::parse_sexp(&block[start..end])?;
        if matches!(child.head(), Some("uuid" | "tstamp")) {
            edits.push(SexpEdit::delete(start, end));
        }
    }
    append_direct_child(
        &apply_edits(block.to_string(), edits),
        &format!("(uuid \"{uuid}\")"),
    )
}

fn assign_fresh_new_child_uuids(
    replacement: &str,
    current_children: &[RawChild],
    root_uuid: &str,
) -> Result<String> {
    let current_identities = current_children
        .iter()
        .filter_map(|child| item_identity(&child.block).ok().flatten())
        .collect::<BTreeSet<_>>();
    let mut result = replacement.to_string();
    for tag in [
        "property",
        "fp_text",
        "fp_line",
        "fp_rect",
        "fp_circle",
        "fp_arc",
        "fp_poly",
        "pad",
    ] {
        let count = raw_direct_children(&result, "footprint")?
            .iter()
            .filter(|child| child.tag == tag)
            .count();
        for occurrence in 0..count {
            let items = children_named(&raw_direct_children(&result, "footprint")?, tag);
            let item = &items[occurrence];
            if item_identity(item)?
                .as_ref()
                .is_some_and(|identity| current_identities.contains(identity))
            {
                continue;
            }
            let uuid = deterministic_child_uuid(root_uuid, tag, occurrence);
            let item = set_item_uuid(item, &uuid)?;
            result = replace_direct_occurrence(&result, "footprint", tag, occurrence, &item)?;
        }
    }
    Ok(result)
}

fn prepare_closed_board_footprint(
    current: &str,
    library_id: &str,
    library_source: &str,
    library_path: &Path,
) -> Result<ClosedPreparedUpdate> {
    let current_root = konnect_sexp::parse_sexp(current).context("invalid board footprint")?;
    if current_root.head() != Some("footprint") {
        bail!("board item is not a footprint");
    }
    let current_id = current_root
        .get(1)
        .and_then(konnect_sexp::SexpNode::as_str)
        .context("board footprint has no library id")?;
    if current_id != library_id {
        bail!("board footprint library id '{current_id}' does not match '{library_id}'");
    }
    let reference = raw_footprint_reference(&current_root)
        .context("board footprint has no Reference property")?;
    let current_children = raw_direct_children(current, "footprint")?;
    let layer_block = exactly_one_child(&current_children, "layer", "board footprint")?;
    let layer_node = konnect_sexp::parse_sexp(&layer_block)?;
    let layer = layer_node
        .get(1)
        .and_then(konnect_sexp::SexpNode::as_str)
        .context("board footprint layer is invalid")?;
    if layer != "F.Cu" {
        bail!(
            "closed-board lossless refresh currently supports F.Cu only; footprint '{reference}' is on '{layer}'"
        );
    }
    let at_block = exactly_one_child(&current_children, "at", "board footprint")?;
    let at = konnect_sexp::parse_sexp(&at_block)?;
    let x = at.get_f64(1).context("footprint placement X is invalid")?;
    let y = at.get_f64(2).context("footprint placement Y is invalid")?;
    let rotation = at.get_f64(3).unwrap_or(0.0);
    let uuid_block = exactly_one_child(&current_children, "uuid", "board footprint")?;

    let library_root = konnect_sexp::parse_sexp(library_source)
        .context("invalid library footprint S-expression")?;
    if library_root.head() != Some("footprint") {
        bail!("library source root is not a footprint");
    }
    let library_children = raw_direct_children(library_source, "footprint")?;
    if current_children
        .iter()
        .chain(library_children.iter())
        .any(|child| child.tag == "group")
    {
        bail!(
            "footprint '{reference}' or library '{library_id}' contains a group; closed-board refresh cannot yet remap group member UUIDs losslessly"
        );
    }

    let graphic_tags = [
        "fp_text",
        "fp_line",
        "fp_rect",
        "fp_circle",
        "fp_arc",
        "fp_poly",
    ];
    for unsupported in [
        "fp_text_box",
        "fp_curve",
        "table",
        "image",
        "barcode",
        "dimension",
        "zone",
    ] {
        if current_children
            .iter()
            .chain(library_children.iter())
            .any(|child| child.tag == unsupported)
        {
            bail!(
                "footprint '{reference}' contains '{unsupported}', which closed-board refresh does not yet preserve losslessly"
            );
        }
    }
    let current_points = children_named(&current_children, "point");
    let library_points = children_named(&library_children, "point");
    let point_matches = match_existing_points(&reference, &current_points, &library_points)?;

    let current_pad_blocks = children_named(&current_children, "pad");
    let library_pad_blocks = children_named(&library_children, "pad");
    let current_pads = unique_pads(&current_pad_blocks, "board footprint")?;
    let library_pads = unique_pads(&library_pad_blocks, "library footprint")?;
    for (number, block) in &current_pads {
        if library_pads.contains_key(number) {
            continue;
        }
        if direct_child_block(block, "pad", "net")?.is_some() {
            bail!(
                "library update removes connected pad {number} from footprint '{reference}'; routed or connected copper would lose its anchor"
            );
        }
        bail!(
            "library update removes unconnected pad {number} from footprint '{reference}'; board-user versus stale-library pad ownership is ambiguous"
        );
    }

    let mut replacement = super::pcb_components::board_footprint_sexp_from_source(
        library_source,
        library_id,
        library_path,
        x,
        y,
        rotation,
        layer,
        Some(&reference),
    )
    .map_err(anyhow::Error::msg)?;
    for (tag, block) in [
        ("layer", layer_block.as_str()),
        ("at", at_block.as_str()),
        ("uuid", uuid_block.as_str()),
    ] {
        replacement = replace_or_append_single_child(&replacement, "footprint", tag, block)?;
    }

    let instance_tags = [
        "locked",
        "placed",
        "path",
        "sheetname",
        "sheetfile",
        "units",
        "component_classes",
        "variant",
        "solder_mask_margin",
        "solder_paste_margin",
        "solder_paste_ratio",
        "solder_paste_margin_ratio",
        "clearance",
        "zone_connect",
        "thermal_width",
        "thermal_gap",
    ];
    for tag in instance_tags {
        let blocks = children_named(&current_children, tag);
        if blocks.len() > 1 {
            bail!("board footprint contains more than one '{tag}' child");
        }
        if let Some(block) = blocks.first() {
            replacement = replace_or_append_single_child(&replacement, "footprint", tag, block)?;
        }
    }

    replacement = merge_root_properties(&replacement, &current_children, &library_children)?;
    replacement = merge_root_attributes(&replacement, &current_children)?;

    for tag in graphic_tags {
        let current_items = children_named(&current_children, tag);
        let replacement_children = raw_direct_children(&replacement, "footprint")?;
        let replacement_items = children_named(&replacement_children, tag);
        if tag == "fp_text" {
            for (current_index, replacement_index) in
                match_preserved_user_texts(&reference, &current_items, &replacement_items)?
            {
                replacement = replace_direct_occurrence(
                    &replacement,
                    "footprint",
                    tag,
                    replacement_index,
                    &current_items[current_index],
                )?;
            }
            continue;
        }
        let matches = match_existing_graphics(&reference, tag, &current_items, &replacement_items)?;
        for (current_index, replacement_index) in matches {
            let current_item = &current_items[current_index];
            let replacement_item = &replacement_items[replacement_index];
            let item = transplant_item_identity(replacement_item.clone(), current_item)?;
            replacement = replace_direct_occurrence(
                &replacement,
                "footprint",
                tag,
                replacement_index,
                &item,
            )?;
        }
    }
    for (current_index, replacement_index) in point_matches {
        let replacement_points =
            children_named(&raw_direct_children(&replacement, "footprint")?, "point");
        let point = transplant_item_identity(
            replacement_points[replacement_index].clone(),
            &current_points[current_index],
        )?;
        replacement = replace_direct_occurrence(
            &replacement,
            "footprint",
            "point",
            replacement_index,
            &point,
        )?;
    }

    let replacement_children = raw_direct_children(&replacement, "footprint")?;
    let replacement_pad_blocks = children_named(&replacement_children, "pad");
    let replacement_pads = unique_pads(&replacement_pad_blocks, "prepared footprint")?;
    for (number, current_pad) in &current_pads {
        let Some(replacement_pad) = replacement_pads.get(number) else {
            continue;
        };
        if !pad_semantically_equal(&one_pad(current_pad)?, &one_pad(replacement_pad)?) {
            bail!(
                "footprint '{reference}' pad {number} geometry does not match the current library within 2 nm; board-user versus stale-library pad ownership is ambiguous, so it was not replaced"
            );
        }
        let mut pad = transplant_item_identity(replacement_pad.clone(), current_pad)?;
        for tag in [
            "net",
            "locked",
            "pinfunction",
            "pintype",
            "zone_layer_connections",
        ] {
            if let Some(block) = direct_child_block(current_pad, "pad", tag)? {
                pad = replace_or_append_single_child(&pad, "pad", tag, &block)?;
            }
        }
        let current_replacement_children = raw_direct_children(&replacement, "footprint")?;
        let occurrence = children_named(&current_replacement_children, "pad")
            .iter()
            .position(|block| pad_number(block).ok().as_deref() == Some(number.as_str()))
            .context("prepared pad disappeared")?;
        replacement =
            replace_direct_occurrence(&replacement, "footprint", "pad", occurrence, &pad)?;
    }
    replacement = assign_fresh_new_child_uuids(&replacement, &current_children, &uuid_block)?;

    let replacement_root = konnect_sexp::parse_sexp(&replacement)
        .context("prepared footprint is invalid S-expression")?;
    if raw_footprint_reference(&replacement_root).as_deref() != Some(reference.as_str()) {
        bail!("prepared footprint did not preserve reference '{reference}'");
    }
    let invariant = closed_footprint_invariant(&replacement, &library_children)?;
    let current_invariant = closed_footprint_invariant(current, &[])?;
    if invariant.footprint_uuid != current_invariant.footprint_uuid
        || invariant.placement != current_invariant.placement
        || invariant.layer != current_invariant.layer
    {
        bail!("prepared footprint did not preserve root identity and placement");
    }
    Ok(ClosedPreparedUpdate {
        replacement,
        invariant,
    })
}

fn unique_pads(blocks: &[String], subject: &str) -> Result<BTreeMap<String, String>> {
    let mut pads = BTreeMap::new();
    for block in blocks {
        let number = pad_number(block)?;
        if pads.insert(number.clone(), block.clone()).is_some() {
            bail!("{subject} contains duplicate pad number '{number}'");
        }
    }
    Ok(pads)
}

fn merge_root_properties(
    replacement: &str,
    current_children: &[RawChild],
    library_children: &[RawChild],
) -> Result<String> {
    let current_properties = children_named(current_children, "property");
    let library_properties = children_named(library_children, "property");
    let mut library_names = BTreeSet::new();
    for property in &library_properties {
        let name = property_name(property)?;
        if !library_names.insert(name.clone()) {
            bail!("library footprint contains duplicate property '{name}'");
        }
    }

    let mut result = replacement.to_string();
    let mut current_names = BTreeSet::new();
    for property in current_properties {
        let name = property_name(&property)?;
        if !current_names.insert(name.clone()) {
            bail!("board footprint contains duplicate property '{name}'");
        }
        let replacement_children = raw_direct_children(&result, "footprint")?;
        let replacement_properties = children_named(&replacement_children, "property");
        let matching = replacement_properties
            .iter()
            .enumerate()
            .filter_map(|(index, block)| {
                (property_name(block).ok().as_deref() == Some(name.as_str())).then_some(index)
            })
            .collect::<Vec<_>>();
        if matching.len() > 1 {
            bail!("prepared footprint contains duplicate property '{name}'");
        }
        if matches!(name.as_str(), "Reference" | "Value") {
            let index = matching
                .first()
                .copied()
                .with_context(|| format!("library footprint is missing property '{name}'"))?;
            result = replace_direct_occurrence(&result, "footprint", "property", index, &property)?;
        } else if !library_names.contains(&name) {
            result = append_direct_child(&result, &property)?;
        } else if let Some(index) = matching.first().copied() {
            let replacement_children = raw_direct_children(&result, "footprint")?;
            let replacement_properties = children_named(&replacement_children, "property");
            let with_identity =
                transplant_item_identity(replacement_properties[index].clone(), &property)?;
            result =
                replace_direct_occurrence(&result, "footprint", "property", index, &with_identity)?;
        }
    }
    Ok(result)
}

fn merge_root_attributes(replacement: &str, current_children: &[RawChild]) -> Result<String> {
    let mut instance_tokens = BTreeSet::new();
    if let Some(current) = children_named(current_children, "attr").first() {
        let node = konnect_sexp::parse_sexp(current)?;
        for child in node.children().unwrap_or_default().iter().skip(1) {
            if let Some(token) = child.as_str() {
                if matches!(
                    token,
                    "board_only" | "exclude_from_pos_files" | "exclude_from_bom" | "dnp"
                ) {
                    instance_tokens.insert(token.to_string());
                }
            }
        }
    }
    let replacement_children = raw_direct_children(replacement, "footprint")?;
    let attr = children_named(&replacement_children, "attr");
    if attr.len() > 1 {
        bail!("library footprint contains more than one attr child");
    }
    let mut tokens = Vec::new();
    if let Some(attr) = attr.first() {
        let node = konnect_sexp::parse_sexp(attr)?;
        for child in node.children().unwrap_or_default().iter().skip(1) {
            let token = child
                .as_str()
                .context("footprint attr contains a non-atom")?;
            if !tokens.iter().any(|existing| existing == token) {
                tokens.push(token.to_string());
            }
        }
    }
    for token in instance_tokens {
        if !tokens.contains(&token) {
            tokens.push(token);
        }
    }
    if tokens.is_empty() {
        return Ok(replacement.to_string());
    }
    replace_or_append_single_child(
        replacement,
        "footprint",
        "attr",
        &format!("(attr {})", tokens.join(" ")),
    )
}

fn closed_footprint_invariant(
    footprint: &str,
    library_children: &[RawChild],
) -> Result<ClosedFootprintInvariant> {
    let root = konnect_sexp::parse_sexp(footprint)?;
    let reference = raw_footprint_reference(&root).context("footprint has no Reference")?;
    let children = raw_direct_children(footprint, "footprint")?;
    let protected_properties = children_named(&children, "property")
        .into_iter()
        .map(|block| Ok((property_name(&block)?, block)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut pads = unique_pads(&children_named(&children, "pad"), "footprint")?
        .into_iter()
        .map(|(number, block)| {
            Ok(ClosedPadInvariant {
                number,
                net: direct_child_block(&block, "pad", "net")?,
                uuid: direct_child_block(&block, "pad", "uuid")?
                    .or(direct_child_block(&block, "pad", "tstamp")?),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    pads.sort_by(|left, right| left.number.cmp(&right.number));
    let points = if library_children.is_empty() {
        children_named(&children, "point")
    } else {
        let expected = children_named(library_children, "point");
        let actual = children_named(&children, "point");
        if actual.len() != expected.len()
            || match_existing_points(&reference, &actual, &expected)?.len() != actual.len()
        {
            bail!("footprint '{reference}' did not preserve every library point semantically");
        }
        actual
    };
    Ok(ClosedFootprintInvariant {
        reference,
        footprint_uuid: exactly_one_child(&children, "uuid", "footprint")?,
        placement: exactly_one_child(&children, "at", "footprint")?,
        layer: exactly_one_child(&children, "layer", "footprint")?,
        protected_properties,
        pads,
        points,
    })
}

fn verify_closed_board_invariants(
    board: &str,
    invariants: &[ClosedFootprintInvariant],
) -> Result<()> {
    let root = konnect_sexp::parse_sexp(board).context("written board is invalid S-expression")?;
    if root.head() != Some("kicad_pcb") {
        bail!("written document root is not kicad_pcb");
    }
    let blocks = find_direct_child_blocks(board, "kicad_pcb")
        .into_iter()
        .filter_map(|(start, end)| {
            let block = &board[start..end];
            let node = konnect_sexp::parse_sexp(block).ok()?;
            (node.head() == Some("footprint")).then(|| block.to_string())
        })
        .collect::<Vec<_>>();
    for expected in invariants {
        let matching = blocks
            .iter()
            .filter(|block| {
                konnect_sexp::parse_sexp(block)
                    .ok()
                    .and_then(|root| raw_footprint_reference(&root))
                    .as_deref()
                    == Some(expected.reference.as_str())
            })
            .collect::<Vec<_>>();
        let [block] = matching.as_slice() else {
            bail!(
                "readback contains {} footprints named '{}'",
                matching.len(),
                expected.reference
            );
        };
        let actual = closed_footprint_invariant(block, &[])?;
        if &actual != expected {
            bail!(
                "readback of footprint '{}' does not match the planned identity, placement, fields, pads, nets, UUIDs, and point children",
                expected.reference
            );
        }
    }
    Ok(())
}

#[derive(Debug)]
enum ClosedPersistFailure {
    BeforeMutation(String),
    RolledBack(String),
    Uncertain(String),
}

fn persist_closed_board_plan(
    board_path: &Path,
    plan: &ClosedBoardPlan,
) -> std::result::Result<(), ClosedPersistFailure> {
    persist_closed_board_plan_with(board_path, plan, |readback| {
        verify_closed_board_invariants(readback, &plan.invariants)
    })
}

fn persist_closed_board_plan_with(
    board_path: &Path,
    plan: &ClosedBoardPlan,
    verify: impl FnOnce(&str) -> Result<()>,
) -> std::result::Result<(), ClosedPersistFailure> {
    persist_closed_board_plan_with_hooks(board_path, plan, || {}, verify)
}

fn persist_closed_board_plan_with_hooks(
    board_path: &Path,
    plan: &ClosedBoardPlan,
    after_write: impl FnOnce(),
    verify: impl FnOnce(&str) -> Result<()>,
) -> std::result::Result<(), ClosedPersistFailure> {
    if let Err(error) = write_atomic_if_unchanged(board_path, &plan.original, &plan.replacement) {
        let observed = std::fs::read_to_string(board_path).ok();
        return if observed.as_deref() == Some(plan.original.as_str()) {
            Err(ClosedPersistFailure::BeforeMutation(format!("{error:#}")))
        } else {
            Err(ClosedPersistFailure::Uncertain(format!(
                "atomic replacement returned an error and the board is not byte-identical to the preflight source: {error:#}"
            )))
        };
    }
    after_write();
    let readback = match read_consistent(board_path) {
        Ok(readback) => readback,
        Err(error) => {
            return Err(ClosedPersistFailure::Uncertain(format!(
                "the board was replaced but could not be read back: {error:#}"
            )))
        }
    };
    if readback != plan.replacement {
        return Err(ClosedPersistFailure::Uncertain(
            "the post-write readback differs from the planned replacement; Konnect preserved the observed bytes because they may belong to another writer"
                .to_string(),
        ));
    }
    let verification = verify(&readback);
    let Err(reason) = verification else {
        return Ok(());
    };

    match write_atomic_if_unchanged(board_path, &plan.replacement, &plan.original) {
        Ok(()) => match read_consistent(board_path) {
            Ok(rolled_back) if rolled_back == plan.original => Err(ClosedPersistFailure::RolledBack(
                format!(
                    "post-write verification failed ({reason:#}); the original bytes were restored and read back"
                ),
            )),
            Ok(_) => Err(ClosedPersistFailure::Uncertain(format!(
                "post-write verification failed ({reason:#}); rollback returned success but original bytes were not read back"
            ))),
            Err(error) => Err(ClosedPersistFailure::Uncertain(format!(
                "post-write verification failed ({reason:#}); rollback could not be read back: {error:#}"
            ))),
        },
        Err(error) => Err(ClosedPersistFailure::Uncertain(format!(
            "post-write verification failed ({reason:#}) and compare-and-swap rollback failed: {error:#}"
        ))),
    }
}

fn plan_updates(
    board_path: &Path,
    footprint_items: &[prost_types::Any],
    net_codes: &BTreeMap<String, i32>,
    routed_nets: &BTreeSet<String>,
    filters: &UpdateFilters,
) -> UpdatePlan {
    #[derive(Debug)]
    struct Candidate {
        reference: String,
        library_id: String,
        item: prost_types::Any,
        instance: kiapi::board::types::FootprintInstance,
    }

    let mut coverage = UpdateCoverage::default();
    let mut diagnostics = Vec::new();
    let mut candidates = Vec::new();
    let mut reference_counts = BTreeMap::<String, usize>::new();

    for item in footprint_items {
        let instance = match kiapi::board::types::FootprintInstance::decode(item.value.as_slice()) {
            Ok(instance) => instance,
            Err(error) => {
                diagnostics.push(UpdateDiagnostic {
                    code: "invalid_board_footprint".to_string(),
                    message: format!("KiCad returned an invalid footprint: {error}"),
                    reference: None,
                });
                continue;
            }
        };
        let reference = field_text(&instance.reference_field);
        *reference_counts.entry(reference.clone()).or_insert(0) += 1;
        let library_id = instance
            .definition
            .as_ref()
            .and_then(|definition| definition.id.as_ref())
            .map(|id| format!("{}:{}", id.library_nickname, id.entry_name))
            .unwrap_or_default();
        candidates.push(Candidate {
            reference,
            library_id,
            item: item.clone(),
            instance,
        });
    }

    if let Some(references) = filters.references.as_ref() {
        for reference in references {
            match reference_counts.get(reference).copied().unwrap_or(0) {
                0 => diagnostics.push(UpdateDiagnostic {
                    code: "reference_not_found".to_string(),
                    message: format!("footprint reference '{reference}' was not found"),
                    reference: Some(reference.clone()),
                }),
                count if count > 1 => diagnostics.push(UpdateDiagnostic {
                    code: "duplicate_reference".to_string(),
                    message: format!("footprint reference '{reference}' appears {count} times"),
                    reference: Some(reference.clone()),
                }),
                _ => {}
            }
        }
    } else {
        for (reference, count) in &reference_counts {
            if *count > 1 {
                diagnostics.push(UpdateDiagnostic {
                    code: "duplicate_reference".to_string(),
                    message: format!("footprint reference '{reference}' appears {count} times"),
                    reference: Some(reference.clone()),
                });
            }
        }
    }

    candidates.sort_by(|left, right| {
        left.reference
            .cmp(&right.reference)
            .then_with(|| left.library_id.cmp(&right.library_id))
    });
    let selected = candidates
        .into_iter()
        .filter(|candidate| {
            filters
                .references
                .as_ref()
                .is_none_or(|references| references.contains(&candidate.reference))
        })
        .filter(|candidate| {
            filters
                .library_ids
                .as_ref()
                .is_none_or(|ids| ids.contains(&candidate.library_id))
        })
        .collect::<Vec<_>>();

    coverage.skipped_unlinked.planned = selected
        .iter()
        .filter(|candidate| candidate.library_id.is_empty())
        .count();
    coverage.selected.planned = selected
        .iter()
        .filter(|candidate| !candidate.library_id.is_empty())
        .count();
    if filters.references.is_some() {
        for candidate in selected
            .iter()
            .filter(|candidate| candidate.library_id.is_empty())
        {
            diagnostics.push(UpdateDiagnostic {
                code: "unlinked_footprint".to_string(),
                message: format!(
                    "footprint '{}' has no resolvable Library:Footprint identifier",
                    candidate.reference
                ),
                reference: Some(candidate.reference.clone()),
            });
        }
    }

    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(filters).expect("filters serialize"));
    let mut changes = Vec::new();
    let mut prepared_items = Vec::new();
    for candidate in selected {
        if candidate.library_id.is_empty() {
            continue;
        }
        let Some((nickname, entry)) = candidate.library_id.split_once(':') else {
            diagnostics.push(UpdateDiagnostic {
                code: "invalid_library_id".to_string(),
                message: format!(
                    "footprint '{}' has malformed library id '{}'",
                    candidate.reference, candidate.library_id
                ),
                reference: Some(candidate.reference),
            });
            continue;
        };
        if nickname.is_empty() || entry.is_empty() {
            diagnostics.push(UpdateDiagnostic {
                code: "invalid_library_id".to_string(),
                message: format!(
                    "footprint '{}' has malformed library id '{}'",
                    candidate.reference, candidate.library_id
                ),
                reference: Some(candidate.reference),
            });
            continue;
        }

        let library_path = match super::library::resolve_footprint_path(
            &candidate.library_id,
            board_path.parent(),
        ) {
            Ok(path) => path,
            Err(message) => {
                diagnostics.push(UpdateDiagnostic {
                    code: "footprint_library_resolution_failed".to_string(),
                    message,
                    reference: Some(candidate.reference),
                });
                continue;
            }
        };
        let source = match std::fs::read_to_string(&library_path) {
            Ok(source) => source,
            Err(error) => {
                diagnostics.push(UpdateDiagnostic {
                    code: "footprint_library_read_failed".to_string(),
                    message: format!("failed to read {}: {error}", library_path.display()),
                    reference: Some(candidate.reference),
                });
                continue;
            }
        };
        let library = match parse_library_footprint(&candidate.library_id, &source) {
            Ok(library) => library,
            Err(error) => {
                diagnostics.push(UpdateDiagnostic {
                    code: "unsupported_library_footprint".to_string(),
                    message: format!("{error:#}"),
                    reference: Some(candidate.reference),
                });
                continue;
            }
        };
        let prepared =
            match build_updated_instance(&candidate.instance, &library, net_codes, routed_nets) {
                Ok(prepared) => prepared,
                Err(error) => {
                    diagnostics.push(UpdateDiagnostic {
                        code: "footprint_update_conflict".to_string(),
                        message: format!("{error:#}"),
                        reference: Some(candidate.reference),
                    });
                    continue;
                }
            };

        hasher.update(candidate.item.type_url.as_bytes());
        hasher.update(&candidate.item.value);
        hasher.update(candidate.library_id.as_bytes());
        hasher.update(source.as_bytes());
        if prepared.changed_domains.is_empty() {
            coverage.unchanged.planned += 1;
        } else {
            coverage.changed.planned += 1;
            changes.push(PlannedUpdate {
                reference: candidate.reference,
                library_id: candidate.library_id,
                changed_domains: prepared.changed_domains,
                preserved: prepared.preserved,
            });
            prepared_items.push(prepared.item);
        }
    }

    if !diagnostics.is_empty() {
        coverage.conflicts.planned = diagnostics.len();
        changes.clear();
        prepared_items.clear();
        coverage.changed.planned = 0;
        return UpdatePlan {
            status: PlanStatus::Conflict,
            plan_revision: format!("{:x}", hasher.finalize()),
            coverage,
            changes,
            diagnostics,
            prepared_items,
        };
    }

    let status = if changes.is_empty() {
        PlanStatus::Noop
    } else {
        PlanStatus::Ready
    };
    UpdatePlan {
        status,
        plan_revision: format!("{:x}", hasher.finalize()),
        coverage,
        changes,
        diagnostics,
        prepared_items,
    }
}

fn parse_library_footprint(library_id: &str, source: &str) -> Result<LibraryFootprint> {
    let (library_nickname, entry_name) = library_id
        .split_once(':')
        .filter(|(nickname, entry)| !nickname.is_empty() && !entry.is_empty())
        .context("footprint identifier must use non-empty Library:Footprint syntax")?;
    let root = konnect_sexp::parse_sexp(source).context("invalid footprint S-expression")?;
    if root.head() != Some("footprint") {
        bail!("library source root must be a footprint");
    }

    validate_supported_children(&root)?;
    let properties = parse_library_properties(&root)?;
    let pads = super::pcb_components::extract_pad_definitions(source)?;
    // Custom properties travel as typed Field items below. Treating visible
    // properties as generic graphics as well would duplicate their text.
    let graphics = super::pcb_components::extract_graphic_definitions_without_properties(source)?;
    let models = super::library_footprint::models(&root)?;
    let attributes = super::library_footprint::attributes(&root)?;
    let definition = kiapi::board::types::Footprint {
        id: Some(kiapi::common::types::LibraryIdentifier {
            library_nickname: library_nickname.to_string(),
            entry_name: entry_name.to_string(),
        }),
        attributes: Some(super::library_footprint::description_and_keywords(&root)),
        ..Default::default()
    };

    Ok(LibraryFootprint {
        library_id: library_id.to_string(),
        definition,
        attributes,
        datasheet: properties.datasheet,
        description_field: properties.description,
        properties: properties.custom,
        pads,
        graphics,
        models,
    })
}

pub(crate) fn parse_library_properties(
    root: &konnect_sexp::SexpNode,
) -> Result<ParsedLibraryProperties> {
    let mut names = BTreeSet::new();
    let mut datasheet = None;
    let mut description = None;
    let mut custom = Vec::new();
    for property in root.find_all("property") {
        let name = property
            .get(1)
            .and_then(konnect_sexp::SexpNode::as_str)
            .context("property is missing its name")?;
        if !names.insert(name.to_string()) {
            bail!("property '{name}' appears more than once in the library footprint");
        }

        // Mandatory and custom properties share one lossless clause validator.
        // The mandatory values keep their existing first-class IPC fields; the
        // shared parser proves that none of their authored clauses would be
        // silently ignored without requiring a typed custom Field.
        let mandatory = matches!(name, "Reference" | "Value" | "Datasheet" | "Description");
        let parsed = parse_library_property(property, !mandatory)?;
        let value = property
            .get(2)
            .and_then(konnect_sexp::SexpNode::as_str)
            .with_context(|| format!("property '{name}' is missing its value"))?;
        match name {
            "Reference" | "Value" => {}
            "Datasheet" => datasheet = Some(value.to_string()),
            "Description" => description = Some(value.to_string()),
            _ => custom.push(parsed.context("custom property did not produce a typed field")?),
        }
    }

    Ok(ParsedLibraryProperties {
        datasheet,
        description,
        custom,
    })
}

pub(crate) fn parse_typed_library_fields(
    root: &konnect_sexp::SexpNode,
) -> Result<Vec<kiapi::board::types::Field>> {
    root.find_all("property")
        .into_iter()
        .map(|property| {
            parse_library_property(property, true)?
                .context("library property did not produce a typed field")
        })
        .collect()
}

/// Convert a footprint property into the typed `Field` shape carried by
/// KiCad's IPC model. Mandatory properties are validated through this same
/// path and then stored in their existing first-class fields. Unknown clauses
/// refuse here: accepting a property while dropping part of its authored
/// presentation would make the refresh lossy even when its value survived.
fn parse_library_property(
    property: &konnect_sexp::SexpNode,
    require_typed_field: bool,
) -> Result<Option<kiapi::board::types::Field>> {
    use kiapi::common::types::LockedState;

    let name = property
        .get(1)
        .and_then(konnect_sexp::SexpNode::as_str)
        .context("property is missing its name")?;
    let value = property
        .get(2)
        .and_then(konnect_sexp::SexpNode::as_str)
        .with_context(|| format!("property '{name}' is missing its value"))?;
    let mut position = None;
    let mut rotation = 0.0;
    let mut layer = None;
    let mut hidden = None;
    let mut knockout = None;
    let mut unlocked = None;
    let mut locked = None;
    let mut attributes = None;
    let mut identifier = None;

    for clause in property.children().unwrap_or_default().iter().skip(3) {
        let tag = clause
            .head()
            .with_context(|| format!("property '{name}' contains an unsupported atom"))?;
        match tag {
            "at" => {
                if position.is_some() {
                    bail!("property '{name}' contains duplicate 'at' clauses");
                }
                let count = clause.children().map_or(0, |children| children.len());
                if !(3..=5).contains(&count) {
                    bail!(
                        "property '{name}' 'at' must contain x, y, optional rotation, and optional legacy unlocked"
                    );
                }
                let x = clause
                    .get_f64(1)
                    .with_context(|| format!("property '{name}' has an invalid X position"))?;
                let y = clause
                    .get_f64(2)
                    .with_context(|| format!("property '{name}' has an invalid Y position"))?;
                let third = clause.get(3).and_then(konnect_sexp::SexpNode::as_str);
                rotation = match third {
                    None | Some("unlocked") => 0.0,
                    Some(_) => clause
                        .get_f64(3)
                        .with_context(|| format!("property '{name}' rotation is invalid"))?,
                };
                let legacy_unlocked = third == Some("unlocked")
                    || clause.get(4).and_then(konnect_sexp::SexpNode::as_str) == Some("unlocked");
                if legacy_unlocked {
                    if unlocked.replace(true).is_some() {
                        bail!("property '{name}' repeats unlocked state");
                    }
                } else if count == 5 {
                    bail!("property '{name}' legacy 'at' tail must be unlocked");
                }
                if !x.is_finite() || !y.is_finite() || !rotation.is_finite() {
                    bail!("property '{name}' position and rotation must be finite");
                }
                position = Some(konnect_ipc::builders::vec2(x, y));
            }
            "layer" => {
                if layer.is_some() {
                    bail!("property '{name}' contains duplicate 'layer' clauses");
                }
                let layer_name = clause
                    .get(1)
                    .and_then(konnect_sexp::SexpNode::as_str)
                    .filter(|name| !name.is_empty())
                    .with_context(|| format!("property '{name}' has no layer name"))?;
                if clause.children().map_or(0, |children| children.len()) != 2 {
                    bail!("property '{name}' 'layer' must name exactly one layer");
                }
                layer = Some(
                    konnect_ipc::builders::try_layer_from_name(layer_name)
                        .with_context(|| format!("property '{name}' has an unsupported layer"))?
                        as i32,
                );
            }
            "hide" => {
                if hidden.is_some() {
                    bail!("property '{name}' contains duplicate 'hide' clauses");
                }
                hidden = Some(property_yes_no(clause, name, "hide")?);
            }
            "knockout" => {
                if knockout.is_some() {
                    bail!("property '{name}' contains duplicate 'knockout' clauses");
                }
                knockout = Some(property_yes_no(clause, name, "knockout")?);
            }
            "unlocked" => {
                if unlocked.is_some() {
                    bail!("property '{name}' contains duplicate 'unlocked' clauses");
                }
                unlocked = Some(property_yes_no(clause, name, "unlocked")?);
            }
            "locked" => {
                if locked.is_some() {
                    bail!("property '{name}' contains duplicate 'locked' clauses");
                }
                locked = Some(property_yes_no(clause, name, "locked")?);
            }
            "uuid" | "tstamp" => {
                if let Some(previous) = identifier {
                    bail!(
                        "property '{name}' contains multiple identifier clauses ('{previous}' and '{tag}')"
                    );
                }
                identifier = Some(tag);
                if clause.children().map_or(0, |children| children.len()) != 2
                    || clause
                        .get(1)
                        .and_then(konnect_sexp::SexpNode::as_str)
                        .is_none_or(str::is_empty)
                {
                    bail!("property '{name}' '{tag}' must contain exactly one identifier");
                }
            }
            "effects" => {
                if attributes.is_some() {
                    bail!("property '{name}' contains duplicate 'effects' clauses");
                }
                attributes = Some(parse_property_effects(clause, name)?);
            }
            unsupported => {
                bail!("property '{name}' clause '{unsupported}' is not supported losslessly")
            }
        }
    }

    if !require_typed_field {
        return Ok(None);
    }

    let position =
        position.with_context(|| format!("property '{name}' is missing its 'at' clause"))?;
    let layer =
        layer.with_context(|| format!("property '{name}' is missing its 'layer' clause"))?;
    let mut attributes =
        attributes.with_context(|| format!("property '{name}' is missing its 'effects' clause"))?;
    attributes.angle = Some(kiapi::common::types::Angle {
        value_degrees: rotation,
    });
    attributes.keep_upright = !unlocked.unwrap_or(false);
    Ok(Some(kiapi::board::types::Field {
        id: None,
        name: name.to_string(),
        text: Some(kiapi::board::types::BoardText {
            // A library child's UUID is definition-local and cannot be reused
            // across placed instances. Let KiCad assign the board child ID.
            id: None,
            text: Some(kiapi::common::types::Text {
                position: Some(position),
                attributes: Some(attributes),
                text: value.to_string(),
                hyperlink: String::new(),
            }),
            layer,
            knockout: knockout.unwrap_or(false),
            locked: if locked.unwrap_or(false) {
                LockedState::LsLocked as i32
            } else {
                LockedState::LsUnlocked as i32
            },
            parent: None,
        }),
        visible: !hidden.unwrap_or(false),
    }))
}

fn property_yes_no(clause: &konnect_sexp::SexpNode, name: &str, tag: &str) -> Result<bool> {
    if clause.children().map_or(0, |children| children.len()) != 2 {
        bail!("property '{name}' '{tag}' must contain exactly one yes/no value");
    }
    match clause.get(1).and_then(konnect_sexp::SexpNode::as_str) {
        Some("yes") => Ok(true),
        Some("no") => Ok(false),
        _ => bail!("property '{name}' '{tag}' must be yes or no"),
    }
}

fn parse_property_effects(
    effects: &konnect_sexp::SexpNode,
    name: &str,
) -> Result<kiapi::common::types::TextAttributes> {
    use kiapi::common::types::{HorizontalAlignment, VerticalAlignment};

    let mut font = None;
    let mut horizontal = HorizontalAlignment::HaCenter;
    let mut vertical = VerticalAlignment::VaCenter;
    let mut mirrored = false;
    for clause in effects.children().unwrap_or_default().iter().skip(1) {
        let tag = clause
            .head()
            .with_context(|| format!("property '{name}' effects contain an unsupported atom"))?;
        match tag {
            "font" => {
                if font.replace(clause).is_some() {
                    bail!("property '{name}' contains duplicate font clauses");
                }
            }
            "justify" => {
                for value in clause.children().unwrap_or_default().iter().skip(1) {
                    match value.as_str().with_context(|| {
                        format!("property '{name}' justify contains a non-atom")
                    })? {
                        "left" if horizontal == HorizontalAlignment::HaCenter => {
                            horizontal = HorizontalAlignment::HaLeft
                        }
                        "right" if horizontal == HorizontalAlignment::HaCenter => {
                            horizontal = HorizontalAlignment::HaRight
                        }
                        "top" if vertical == VerticalAlignment::VaCenter => {
                            vertical = VerticalAlignment::VaTop
                        }
                        "bottom" if vertical == VerticalAlignment::VaCenter => {
                            vertical = VerticalAlignment::VaBottom
                        }
                        "mirror" if !mirrored => mirrored = true,
                        "left" | "right" => bail!(
                            "property '{name}' has conflicting horizontal justification"
                        ),
                        "top" | "bottom" => {
                            bail!("property '{name}' has conflicting vertical justification")
                        }
                        "mirror" => bail!("property '{name}' repeats mirrored justification"),
                        unsupported => bail!(
                            "property '{name}' justification '{unsupported}' is not supported losslessly"
                        ),
                    }
                }
            }
            unsupported => bail!(
                "property '{name}' effects clause '{unsupported}' is not supported losslessly"
            ),
        }
    }
    let font =
        font.with_context(|| format!("property '{name}' effects are missing the font clause"))?;

    let mut font_name = String::new();
    let mut size = None;
    let mut thickness = None;
    let mut bold = None;
    let mut italic = None;
    let mut line_spacing = None;
    for clause in font.children().unwrap_or_default().iter().skip(1) {
        let tag = clause
            .head()
            .with_context(|| format!("property '{name}' font contains an unsupported atom"))?;
        match tag {
            "face" => {
                if !font_name.is_empty() {
                    bail!("property '{name}' contains duplicate font face clauses");
                }
                font_name = clause
                    .get(1)
                    .and_then(konnect_sexp::SexpNode::as_str)
                    .filter(|face| !face.is_empty())
                    .with_context(|| format!("property '{name}' font face is invalid"))?
                    .to_string();
            }
            "size" => {
                if size.is_some() {
                    bail!("property '{name}' contains duplicate font size clauses");
                }
                if clause.children().map_or(0, |children| children.len()) != 3 {
                    bail!("property '{name}' font size must contain width and height");
                }
                let width = clause
                    .get_f64(1)
                    .with_context(|| format!("property '{name}' font width is invalid"))?;
                let height = clause
                    .get_f64(2)
                    .with_context(|| format!("property '{name}' font height is invalid"))?;
                if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
                    bail!("property '{name}' font size must be finite and positive");
                }
                size = Some((width, height));
            }
            "thickness" => {
                if thickness.is_some() {
                    bail!("property '{name}' contains duplicate font thickness clauses");
                }
                let value = clause
                    .get_f64(1)
                    .with_context(|| format!("property '{name}' font thickness is invalid"))?;
                if clause.children().map_or(0, |children| children.len()) != 2
                    || !value.is_finite()
                    || value <= 0.0
                {
                    bail!("property '{name}' font thickness must be finite and positive");
                }
                thickness = Some(value);
            }
            "bold" => {
                if bold.is_some() {
                    bail!("property '{name}' contains duplicate font 'bold' clauses");
                }
                bold = Some(property_yes_no(clause, name, "font bold")?);
            }
            "italic" => {
                if italic.is_some() {
                    bail!("property '{name}' contains duplicate font 'italic' clauses");
                }
                italic = Some(property_yes_no(clause, name, "font italic")?);
            }
            "line_spacing" => {
                if line_spacing.is_some() {
                    bail!("property '{name}' contains duplicate font 'line_spacing' clauses");
                }
                let value = clause
                    .get_f64(1)
                    .with_context(|| format!("property '{name}' line spacing is invalid"))?;
                if clause.children().map_or(0, |children| children.len()) != 2
                    || !value.is_finite()
                    || value <= 0.0
                {
                    bail!("property '{name}' line spacing must be finite and positive");
                }
                line_spacing = Some(value);
            }
            unsupported => {
                bail!("property '{name}' font clause '{unsupported}' is not supported losslessly")
            }
        }
    }
    let (width, height) =
        size.with_context(|| format!("property '{name}' font is missing its size"))?;
    Ok(kiapi::common::types::TextAttributes {
        font_name,
        horizontal_alignment: horizontal as i32,
        vertical_alignment: vertical as i32,
        angle: None,
        line_spacing: line_spacing.unwrap_or(1.0),
        stroke_width: Some(konnect_ipc::builders::distance(
            thickness.unwrap_or(width * 0.15),
        )),
        italic: italic.unwrap_or(false),
        bold: bold.unwrap_or(false),
        underlined: false,
        visible: true,
        mirrored,
        multiline: false,
        keep_upright: false,
        size: Some(konnect_ipc::builders::vec2(width, height)),
    })
}

pub(crate) fn validate_supported_children(root: &konnect_sexp::SexpNode) -> Result<()> {
    for child in root.children().unwrap_or_default().iter().skip(2) {
        let Some(tag) = child.head() else {
            continue;
        };
        match tag {
            "version" | "generator" | "generator_version" | "layer" | "descr" | "tags" | "attr"
            | "property" | "fp_text" | "fp_line" | "fp_rect" | "fp_circle" | "fp_arc"
            | "fp_poly" | "pad" | "model" => {}
            // KiCad 10 writes these two flags into every footprint it saves
            // (all 15,428 official library files carry them, every one at its
            // default). At the default the typed rebuild loses nothing;
            // a non-default value carries semantics it cannot represent.
            "embedded_fonts" | "duplicate_pad_numbers_are_jumpers" => {
                let value = child.get(1).and_then(konnect_sexp::SexpNode::as_str);
                if value != Some("no") {
                    bail!(
                        "footprint '{tag} {}' is not supported by typed library refresh",
                        value.unwrap_or("")
                    );
                }
            }
            unsupported => {
                bail!("footprint child '{unsupported}' is not supported by typed library refresh")
            }
        }
        match tag {
            "pad" => validate_pad(child)?,
            "fp_line" | "fp_rect" | "fp_circle" | "fp_arc" | "fp_poly" => {
                validate_graphic(child, tag)?
            }
            "fp_text" => {
                let kind = child
                    .get(1)
                    .and_then(konnect_sexp::SexpNode::as_str)
                    .context("fp_text is missing its kind")?;
                match kind {
                    "reference" | "value" => {}
                    "user" => validate_user_text(child)?,
                    _ => {
                        bail!(
                            "fp_text kind '{kind}' is not supported losslessly by typed library refresh"
                        );
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Accept only the `fp_text user` subset the typed IPC rebuild can reproduce.
/// Refusing an unknown clause is intentional: silently dropping even a visual
/// modifier would make a refreshed board differ from its library definition.
fn validate_user_text(text: &konnect_sexp::SexpNode) -> Result<()> {
    text.get(2)
        .and_then(konnect_sexp::SexpNode::as_str)
        .context("fp_text user is missing its text")?;

    let mut saw_at = false;
    let mut saw_layer = false;
    let mut saw_effects = false;
    let mut identifier = None;
    for clause in text.children().unwrap_or_default().iter().skip(3) {
        let tag = clause
            .head()
            .context("fp_text user contains an unsupported atom")?;
        match tag {
            "at" => {
                if saw_at {
                    bail!("fp_text user contains duplicate 'at' clauses");
                }
                saw_at = true;
                let count = clause.children().map_or(0, |children| children.len());
                if !matches!(count, 3 | 4) {
                    bail!("fp_text user 'at' must contain x, y, and optional rotation");
                }
                for index in 1..count {
                    let value = clause
                        .get_f64(index)
                        .with_context(|| format!("fp_text user 'at' value {index} is invalid"))?;
                    if !value.is_finite() {
                        bail!("fp_text user 'at' values must be finite");
                    }
                }
            }
            "layer" => {
                if saw_layer {
                    bail!("fp_text user contains duplicate 'layer' clauses");
                }
                saw_layer = true;
                if clause.children().map_or(0, |children| children.len()) != 2
                    || clause
                        .get(1)
                        .and_then(konnect_sexp::SexpNode::as_str)
                        .is_none_or(str::is_empty)
                {
                    bail!("fp_text user 'layer' must name exactly one layer");
                }
            }
            "uuid" | "tstamp" => {
                if let Some(previous) = identifier {
                    bail!(
                        "fp_text user contains multiple identifier clauses ('{previous}' and '{tag}')"
                    );
                }
                identifier = Some(tag);
                if clause.children().map_or(0, |children| children.len()) != 2
                    || clause
                        .get(1)
                        .and_then(konnect_sexp::SexpNode::as_str)
                        .is_none_or(str::is_empty)
                {
                    bail!("fp_text user '{tag}' must contain exactly one identifier");
                }
            }
            "effects" => {
                if saw_effects {
                    bail!("fp_text user contains duplicate 'effects' clauses");
                }
                saw_effects = true;
                validate_user_text_effects(clause)?;
            }
            unsupported => {
                bail!("fp_text user clause '{unsupported}' is not supported losslessly");
            }
        }
    }

    if !saw_at {
        bail!("fp_text user is missing its 'at' clause");
    }
    if !saw_layer {
        bail!("fp_text user is missing its 'layer' clause");
    }
    if !saw_effects {
        bail!("fp_text user is missing its 'effects' clause");
    }
    Ok(())
}

fn validate_user_text_effects(effects: &konnect_sexp::SexpNode) -> Result<()> {
    let mut font = None;
    for clause in effects.children().unwrap_or_default().iter().skip(1) {
        let tag = clause
            .head()
            .context("fp_text user effects contain an unsupported atom")?;
        if tag != "font" {
            bail!("fp_text user effects clause '{tag}' is not supported losslessly");
        }
        if font.replace(clause).is_some() {
            bail!("fp_text user contains duplicate font clauses");
        }
    }
    let font = font.context("fp_text user effects are missing the font clause")?;

    let mut size = None;
    let mut thickness = None;
    for clause in font.children().unwrap_or_default().iter().skip(1) {
        let tag = clause
            .head()
            .context("fp_text user font contains an unsupported atom")?;
        match tag {
            "size" => {
                if size.is_some() {
                    bail!("fp_text user contains duplicate font size clauses");
                }
                if clause.children().map_or(0, |children| children.len()) != 3 {
                    bail!("fp_text user font size must contain width and height");
                }
                let width = clause
                    .get_f64(1)
                    .context("fp_text user font width is invalid")?;
                let height = clause
                    .get_f64(2)
                    .context("fp_text user font height is invalid")?;
                if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
                    bail!("fp_text user font size must be finite and positive");
                }
                if width != height {
                    bail!("fp_text user non-square font size is not supported losslessly");
                }
                size = Some(width);
            }
            "thickness" => {
                if thickness.is_some() {
                    bail!("fp_text user contains duplicate font thickness clauses");
                }
                if clause.children().map_or(0, |children| children.len()) != 2 {
                    bail!("fp_text user font thickness must contain exactly one value");
                }
                let value = clause
                    .get_f64(1)
                    .context("fp_text user font thickness is invalid")?;
                if !value.is_finite() || value <= 0.0 {
                    bail!("fp_text user font thickness must be finite and positive");
                }
                thickness = Some(value);
            }
            unsupported => {
                bail!("fp_text user font clause '{unsupported}' is not supported losslessly");
            }
        }
    }
    size.context("fp_text user font is missing its size")?;
    thickness.context("fp_text user font is missing its thickness")?;
    Ok(())
}

fn validate_graphic(graphic: &konnect_sexp::SexpNode, kind: &str) -> Result<()> {
    let allowed: &[&str] = match kind {
        "fp_line" => &["start", "end", "stroke", "layer", "uuid", "tstamp"],
        "fp_rect" => &["start", "end", "stroke", "fill", "layer", "uuid", "tstamp"],
        "fp_circle" => &["center", "end", "stroke", "fill", "layer", "uuid", "tstamp"],
        "fp_arc" => &["start", "mid", "end", "stroke", "layer", "uuid", "tstamp"],
        "fp_poly" => &["pts", "stroke", "fill", "layer", "uuid", "tstamp"],
        _ => bail!("unsupported footprint graphic '{kind}'"),
    };
    for child in graphic.children().unwrap_or_default().iter().skip(1) {
        let Some(tag) = child.head() else {
            bail!("{kind} contains an unsupported atom");
        };
        if !allowed.contains(&tag) {
            bail!("{kind} clause '{tag}' is not supported by typed library refresh");
        }
        match tag {
            "stroke" => {
                for stroke_child in child.children().unwrap_or_default().iter().skip(1) {
                    let stroke_tag = stroke_child
                        .head()
                        .context("graphic stroke contains an unsupported atom")?;
                    match stroke_tag {
                        "width" => {}
                        "type" => {
                            let stroke_type = stroke_child
                                .get(1)
                                .and_then(konnect_sexp::SexpNode::as_str)
                                .context("graphic stroke type is missing")?;
                            if !matches!(stroke_type, "solid" | "default") {
                                bail!(
                                    "graphic stroke type '{stroke_type}' is not supported losslessly"
                                );
                            }
                        }
                        unsupported => bail!(
                            "graphic stroke clause '{unsupported}' is not supported losslessly"
                        ),
                    }
                }
            }
            "fill" => {
                let fill = child
                    .get(1)
                    .and_then(konnect_sexp::SexpNode::as_str)
                    .context("graphic fill is missing")?;
                if !matches!(fill, "none" | "no" | "solid" | "yes") {
                    bail!("graphic fill '{fill}' is not supported losslessly");
                }
            }
            "pts"
                if child
                    .children()
                    .unwrap_or_default()
                    .iter()
                    .skip(1)
                    .any(|point| point.head() != Some("xy")) =>
            {
                bail!("fp_poly contains a non-xy point");
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_pad(pad: &konnect_sexp::SexpNode) -> Result<()> {
    let shape = pad
        .get(3)
        .and_then(konnect_sexp::SexpNode::as_str)
        .context("footprint pad is missing its shape")?;
    if !matches!(shape, "circle" | "rect" | "oval" | "roundrect") {
        bail!("pad shape '{shape}' is not supported by typed library refresh");
    }
    for child in pad.children().unwrap_or_default().iter().skip(4) {
        let Some(tag) = child.head() else {
            continue;
        };
        if !matches!(
            tag,
            "at" | "size"
                | "layers"
                | "drill"
                | "roundrect_rratio"
                | "uuid"
                | "tstamp"
                | "remove_unused_layers"
        ) {
            bail!("pad clause '{tag}' is not supported by typed library refresh");
        }
        if tag == "remove_unused_layers" {
            // The typed rebuild always keeps unused layers (`UlrKeep`), so a
            // pad that actually removes them cannot be rebuilt losslessly.
            let value = child.get(1).and_then(konnect_sexp::SexpNode::as_str);
            if value != Some("no") {
                bail!("pad 'remove_unused_layers yes' is not supported by typed library refresh");
            }
        }
        if tag == "drill" {
            for nested in child.children().unwrap_or_default().iter().skip(1) {
                if nested.head().is_some() {
                    bail!("nested drill clauses are not supported by typed library refresh");
                }
            }
        }
    }
    Ok(())
}

fn build_updated_instance(
    current: &kiapi::board::types::FootprintInstance,
    library: &LibraryFootprint,
    net_codes: &BTreeMap<String, i32>,
    routed_nets: &BTreeSet<String>,
) -> Result<PreparedUpdate> {
    let current_definition = current
        .definition
        .as_ref()
        .context("board footprint has no embedded definition")?;
    let current_id = current_definition
        .id
        .as_ref()
        .map(|id| format!("{}:{}", id.library_nickname, id.entry_name))
        .unwrap_or_default();
    if current_id != library.library_id {
        bail!(
            "board footprint library id '{current_id}' does not match '{}'",
            library.library_id
        );
    }

    let mut old_nets = BTreeMap::<String, kiapi::board::types::Net>::new();
    for item in &current_definition.items {
        if !item.type_url.ends_with("kiapi.board.types.Pad") {
            continue;
        }
        let pad = kiapi::board::types::Pad::decode(item.value.as_slice())
            .context("board footprint contains an invalid pad")?;
        let Some(net) = pad.net.filter(|net| !net.name.is_empty()) else {
            continue;
        };
        if let Some(existing) = old_nets.insert(pad.number.clone(), net.clone()) {
            if existing.name != net.name {
                bail!(
                    "logical pad {} carries multiple nets ('{}' and '{}')",
                    pad.number,
                    existing.name,
                    net.name
                );
            }
        }
    }
    let new_numbers = library
        .pads
        .iter()
        .map(|pad| pad.number.as_str())
        .collect::<BTreeSet<_>>();
    for (number, net) in &old_nets {
        if !new_numbers.contains(number.as_str()) {
            let routed = routed_nets.contains(&net.name);
            bail!(
                "library update removes connected pad {number} on net '{}'{}",
                net.name,
                if routed { " with routed copper" } else { "" }
            );
        }
    }

    let position = current.position.as_ref().cloned().unwrap_or_default();
    let rotation = current
        .orientation
        .as_ref()
        .map(|angle| angle.value_degrees)
        .unwrap_or(0.0);
    let is_back = current.layer == kiapi::board::types::BoardLayer::BlBCu as i32;
    let layer = if is_back { "B.Cu" } else { "F.Cu" };
    let (pads, graphics) = if is_back {
        (
            library
                .pads
                .iter()
                .map(mirror_pad)
                .collect::<Result<Vec<_>>>()?,
            library
                .graphics
                .iter()
                .map(mirror_graphic)
                .collect::<Result<Vec<_>>>()?,
        )
    } else {
        (library.pads.clone(), library.graphics.clone())
    };
    let packed = konnect_ipc::KiCadIpcClient::build_footprint_item(
        &library.library_id,
        &field_text(&current.reference_field),
        &field_text(&current.value_field),
        &pads,
        &graphics,
        &Default::default(),
        konnect_ipc::builders::nm_to_mm(position.x_nm),
        konnect_ipc::builders::nm_to_mm(position.y_nm),
        rotation,
        layer,
    )?;
    let built = kiapi::board::types::FootprintInstance::decode(packed.value.as_slice())
        .context("typed footprint builder returned an invalid item")?;

    let mut updated = current.clone();
    let mut definition = built
        .definition
        .context("typed footprint builder returned no definition")?;
    definition.attributes = library.definition.attributes.clone();
    definition.reference_field = current_definition.reference_field.clone();
    definition.value_field = current_definition.value_field.clone();
    definition.datasheet_field = current_definition.datasheet_field.clone();
    definition.description_field = current_definition.description_field.clone();
    apply_field_value(
        &mut definition.datasheet_field,
        library.datasheet.as_deref(),
    );
    apply_field_value(
        &mut definition.description_field,
        library.description_field.as_deref(),
    );
    definition.items.extend(
        library.models.iter().map(|model| {
            konnect_ipc::builders::pack_any(model, "kiapi.board.types.Footprint3DModel")
        }),
    );
    for item in &mut definition.items {
        if item.type_url.ends_with("kiapi.board.types.Pad") {
            let mut pad = kiapi::board::types::Pad::decode(item.value.as_slice())?;
            pad.net = old_nets
                .get(&pad.number)
                .map(|old| kiapi::board::types::Net {
                    code: net_codes
                        .get(&old.name)
                        .copied()
                        .or_else(|| old.code.as_ref().map(|code| code.value))
                        .map(|value| kiapi::board::types::NetCode { value }),
                    name: old.name.clone(),
                });
            *item = konnect_ipc::builders::pack_any(&pad, "kiapi.board.types.Pad");
        } else if is_back && item.type_url.ends_with("kiapi.board.types.BoardText") {
            let mut text = kiapi::board::types::BoardText::decode(item.value.as_slice())?;
            if is_side_specific_layer(text.layer) {
                if let Some(attributes) =
                    text.text.as_mut().and_then(|text| text.attributes.as_mut())
                {
                    attributes.mirrored = true;
                }
            }
            *item = konnect_ipc::builders::pack_any(&text, "kiapi.board.types.BoardText");
        }
    }
    merge_custom_properties(
        &mut definition,
        current_definition,
        &library.properties,
        &position,
        rotation,
        is_back,
    )?;
    updated.definition = Some(definition);
    apply_field_value(&mut updated.datasheet_field, library.datasheet.as_deref());
    apply_field_value(
        &mut updated.description_field,
        library.description_field.as_deref(),
    );
    let mut attributes = library.attributes.clone();
    if let Some(current_attributes) = current.attributes.as_ref() {
        attributes.not_in_schematic = current_attributes.not_in_schematic;
        attributes.do_not_populate = current_attributes.do_not_populate;
    }
    updated.attributes = Some(attributes);

    let changed_domains = changed_domains(current, &updated)?;
    let preserved = PreservedState::derive(current, &updated, &old_nets);
    Ok(PreparedUpdate {
        item: konnect_ipc::builders::pack_any(&updated, "kiapi.board.types.FootprintInstance"),
        changed_domains,
        preserved,
    })
}

fn apply_field_value(field: &mut Option<kiapi::board::types::Field>, value: Option<&str>) {
    let Some(value) = value else {
        return;
    };
    field
        .get_or_insert_with(Default::default)
        .text
        .get_or_insert_with(Default::default)
        .text
        .get_or_insert_with(Default::default)
        .text = value.to_string();
}

fn merge_custom_properties(
    updated: &mut kiapi::board::types::Footprint,
    current: &kiapi::board::types::Footprint,
    library_properties: &[kiapi::board::types::Field],
    footprint_position: &kiapi::common::types::Vector2,
    footprint_rotation: f64,
    is_back: bool,
) -> Result<()> {
    let library_names = library_properties
        .iter()
        .map(|field| field.name.as_str())
        .collect::<BTreeSet<_>>();
    let mut current_names = BTreeSet::new();
    for item in current
        .items
        .iter()
        .filter(|item| item.type_url.ends_with("kiapi.board.types.Field"))
    {
        let field = kiapi::board::types::Field::decode(item.value.as_slice())
            .context("board footprint contains an invalid custom property")?;
        if field.name.is_empty() {
            bail!("board footprint contains a custom property without a name");
        }
        if !current_names.insert(field.name.clone()) {
            bail!(
                "board footprint contains more than one custom property named '{}'",
                field.name
            );
        }
        if !library_names.contains(field.name.as_str()) {
            // A field that exists only on the placed instance belongs to that
            // instance. Preserve its complete typed representation verbatim.
            updated.items.push(item.clone());
        }
    }
    for property in library_properties {
        let property =
            transform_library_property(property, footprint_position, footprint_rotation, is_back)?;
        updated.items.push(konnect_ipc::builders::pack_any(
            &property,
            "kiapi.board.types.Field",
        ));
    }
    Ok(())
}

pub(crate) fn transform_library_property(
    property: &kiapi::board::types::Field,
    footprint_position: &kiapi::common::types::Vector2,
    footprint_rotation: f64,
    is_back: bool,
) -> Result<kiapi::board::types::Field> {
    let mut property = property.clone();
    property.id = None;
    let board_text = property
        .text
        .as_mut()
        .with_context(|| format!("property '{}' has no board text", property.name))?;
    board_text.id = None;
    let text = board_text
        .text
        .as_mut()
        .with_context(|| format!("property '{}' has no text value", property.name))?;
    let local_position = text
        .position
        .as_ref()
        .with_context(|| format!("property '{}' has no position", property.name))?;
    let local_x = konnect_ipc::builders::nm_to_mm(local_position.x_nm);
    let mut local_y = konnect_ipc::builders::nm_to_mm(local_position.y_nm);
    if is_back {
        local_y = -local_y;
    }
    let (board_x, board_y) = konnect_sexp::geometry::transform_pad(
        local_x,
        local_y,
        konnect_ipc::builders::nm_to_mm(footprint_position.x_nm),
        konnect_ipc::builders::nm_to_mm(footprint_position.y_nm),
        footprint_rotation,
    );
    text.position = Some(konnect_ipc::builders::vec2(board_x, board_y));

    let attributes = text
        .attributes
        .as_mut()
        .with_context(|| format!("property '{}' has no text attributes", property.name))?;
    let local_angle = attributes
        .angle
        .as_ref()
        .map(|angle| angle.value_degrees)
        .unwrap_or(0.0);
    let local_angle = if is_back {
        180.0 - local_angle
    } else {
        local_angle
    };
    attributes.angle = Some(kiapi::common::types::Angle {
        value_degrees: readable_property_angle(local_angle + footprint_rotation),
    });
    if is_back {
        attributes.mirrored = !attributes.mirrored;
        let layer = kiapi::board::types::BoardLayer::try_from(board_text.layer)
            .with_context(|| format!("property '{}' has an invalid layer", property.name))?;
        let layer_name = konnect_ipc::builders::layer_name(layer)
            .with_context(|| format!("property '{}' has an unnamed layer", property.name))?;
        board_text.layer =
            konnect_ipc::builders::try_layer_from_name(&flip_layer_name(layer_name)?)? as i32;
    }
    Ok(property)
}

fn readable_property_angle(degrees: f64) -> f64 {
    let mut angle = degrees.rem_euclid(360.0);
    if angle > 90.0 && angle <= 270.0 {
        angle -= 180.0;
    }
    angle
}

fn mirror_pad(pad: &konnect_ipc::IpcPadDefinition) -> Result<konnect_ipc::IpcPadDefinition> {
    let mut mirrored = pad.clone();
    mirrored.y = -mirrored.y;
    mirrored.rotation = -mirrored.rotation;
    mirrored.layers = mirrored
        .layers
        .iter()
        .map(|layer| flip_layer_name(layer))
        .collect::<Result<_>>()?;
    Ok(mirrored)
}

fn mirror_graphic(
    graphic: &konnect_ipc::IpcGraphicDefinition,
) -> Result<konnect_ipc::IpcGraphicDefinition> {
    use konnect_ipc::IpcGraphicDefinition as Graphic;

    let point = |(x, y): (f64, f64)| (x, -y);
    Ok(match graphic {
        Graphic::Line {
            start,
            end,
            layer,
            width,
        } => Graphic::Line {
            start: point(*start),
            end: point(*end),
            layer: flip_layer_name(layer)?,
            width: *width,
        },
        Graphic::Rect {
            start,
            end,
            layer,
            width,
            filled,
        } => Graphic::Rect {
            start: point(*start),
            end: point(*end),
            layer: flip_layer_name(layer)?,
            width: *width,
            filled: *filled,
        },
        Graphic::Circle {
            center,
            end,
            layer,
            width,
            filled,
        } => Graphic::Circle {
            center: point(*center),
            end: point(*end),
            layer: flip_layer_name(layer)?,
            width: *width,
            filled: *filled,
        },
        Graphic::Arc {
            start,
            mid,
            end,
            layer,
            width,
        } => Graphic::Arc {
            start: point(*end),
            mid: point(*mid),
            end: point(*start),
            layer: flip_layer_name(layer)?,
            width: *width,
        },
        Graphic::Poly {
            points,
            layer,
            width,
            filled,
        } => Graphic::Poly {
            points: points.iter().copied().map(point).collect(),
            layer: flip_layer_name(layer)?,
            width: *width,
            filled: *filled,
        },
        Graphic::Text {
            text,
            position,
            rotation,
            layer,
            size,
            stroke_width_mm,
        } => Graphic::Text {
            text: text.clone(),
            position: point(*position),
            rotation: 180.0 - rotation,
            layer: flip_layer_name(layer)?,
            size: *size,
            stroke_width_mm: *stroke_width_mm,
        },
    })
}

fn flip_layer_name(layer: &str) -> Result<String> {
    let flipped = match layer {
        "F.Cu" => "B.Cu",
        "B.Cu" => "F.Cu",
        "F.Adhes" => "B.Adhes",
        "B.Adhes" => "F.Adhes",
        "F.Paste" => "B.Paste",
        "B.Paste" => "F.Paste",
        "F.SilkS" | "F.Silkscreen" => "B.SilkS",
        "B.SilkS" | "B.Silkscreen" => "F.SilkS",
        "F.Mask" => "B.Mask",
        "B.Mask" => "F.Mask",
        "F.CrtYd" | "F.Courtyard" => "B.CrtYd",
        "B.CrtYd" | "B.Courtyard" => "F.CrtYd",
        "F.Fab" => "B.Fab",
        "B.Fab" => "F.Fab",
        "*.Cu" | "*.Mask" | "*.Paste" => layer,
        other if other.starts_with("F.") || other.starts_with("B.") => {
            bail!("unsupported side-specific footprint layer '{other}'")
        }
        other => other,
    };
    Ok(flipped.to_string())
}

fn is_side_specific_layer(layer: i32) -> bool {
    use kiapi::board::types::BoardLayer;

    matches!(
        BoardLayer::try_from(layer).ok(),
        Some(
            BoardLayer::BlFCu
                | BoardLayer::BlBCu
                | BoardLayer::BlFAdhes
                | BoardLayer::BlBAdhes
                | BoardLayer::BlFPaste
                | BoardLayer::BlBPaste
                | BoardLayer::BlFSilkS
                | BoardLayer::BlBSilkS
                | BoardLayer::BlFMask
                | BoardLayer::BlBMask
                | BoardLayer::BlFCrtYd
                | BoardLayer::BlBCrtYd
                | BoardLayer::BlFFab
                | BoardLayer::BlBFab
        )
    )
}

fn changed_domains(
    current: &kiapi::board::types::FootprintInstance,
    updated: &kiapi::board::types::FootprintInstance,
) -> Result<BTreeSet<ChangedDomain>> {
    let mut changed = BTreeSet::new();
    let current_definition = current
        .definition
        .as_ref()
        .context("missing current definition")?;
    let updated_definition = updated
        .definition
        .as_ref()
        .context("missing updated definition")?;

    let current_pads = normalized_items(current_definition, "Pad")?;
    let updated_pads = normalized_items(updated_definition, "Pad")?;
    if current_pads != updated_pads {
        changed.insert(ChangedDomain::Pads);
    }
    let current_graphics = normalized_graphics(current_definition)?;
    let updated_graphics = normalized_graphics(updated_definition)?;
    if current_graphics != updated_graphics {
        changed.insert(ChangedDomain::Graphics);
    }
    if normalized_items(current_definition, "Footprint3DModel")?
        != normalized_items(updated_definition, "Footprint3DModel")?
    {
        changed.insert(ChangedDomain::Models);
    }
    if current_definition.attributes != updated_definition.attributes
        || field_text(&current_definition.datasheet_field)
            != field_text(&updated_definition.datasheet_field)
        || field_text(&current_definition.description_field)
            != field_text(&updated_definition.description_field)
        || normalized_items(current_definition, "Field")?
            != normalized_items(updated_definition, "Field")?
    {
        changed.insert(ChangedDomain::Metadata);
    }
    if library_owned_attributes(current.attributes.as_ref())
        != library_owned_attributes(updated.attributes.as_ref())
    {
        changed.insert(ChangedDomain::Attributes);
    }
    Ok(changed)
}

fn normalized_items(
    definition: &kiapi::board::types::Footprint,
    suffix: &str,
) -> Result<Vec<Vec<u8>>> {
    let mut items = definition
        .items
        .iter()
        .filter(|item| item.type_url.ends_with(suffix))
        .map(|item| {
            if suffix == "Pad" {
                let mut pad = kiapi::board::types::Pad::decode(item.value.as_slice())?;
                pad.id = None;
                pad.net = None;
                pad.pad_to_die_length = None;
                pad.symbol_pin = None;
                pad.pad_to_die_delay = None;
                if let Some(stack) = pad.pad_stack.as_mut() {
                    stack.layers.sort_unstable();
                    if stack.drill.as_ref().is_some_and(|drill| {
                        drill.start_layer == kiapi::board::types::BoardLayer::BlUndefined as i32
                            && drill.end_layer
                                == kiapi::board::types::BoardLayer::BlUndefined as i32
                            && drill
                                .diameter
                                .as_ref()
                                .is_none_or(|diameter| diameter.x_nm == 0 && diameter.y_nm == 0)
                    }) {
                        stack.drill = None;
                    }
                    if stack.zone_settings.as_ref().is_some_and(|settings| {
                        settings.zone_connection
                            == kiapi::board::types::ZoneConnectionStyle::ZcsInherited as i32
                    }) {
                        stack.zone_settings = None;
                    }
                }
                Ok(pad.encode_to_vec())
            } else if suffix == "Field" {
                let mut field = kiapi::board::types::Field::decode(item.value.as_slice())?;
                field.id = None;
                if let Some(text) = field.text.as_mut() {
                    text.id = None;
                }
                Ok(field.encode_to_vec())
            } else {
                Ok(item.value.clone())
            }
        })
        .collect::<Result<Vec<_>>>()?;
    items.sort();
    Ok(items)
}

fn normalized_graphics(definition: &kiapi::board::types::Footprint) -> Result<Vec<Vec<u8>>> {
    let mut graphics = definition
        .items
        .iter()
        .filter_map(|item| {
            if item.type_url.ends_with("BoardGraphicShape") {
                Some(
                    kiapi::board::types::BoardGraphicShape::decode(item.value.as_slice()).map(
                        |mut shape| {
                            shape.id = None;
                            shape.net = None;
                            shape.locked = kiapi::common::types::LockedState::LsUnlocked as i32;
                            if let Some(graphic) = shape.shape.as_mut() {
                                if let Some(stroke) = graphic
                                    .attributes
                                    .as_mut()
                                    .and_then(|attributes| attributes.stroke.as_mut())
                                {
                                    if matches!(
                                        stroke.style(),
                                        kiapi::common::types::StrokeLineStyle::SlsUnknown
                                            | kiapi::common::types::StrokeLineStyle::SlsDefault
                                            | kiapi::common::types::StrokeLineStyle::SlsSolid
                                    ) {
                                        stroke.style =
                                            kiapi::common::types::StrokeLineStyle::SlsSolid as i32;
                                    }
                                    stroke.color = None;
                                }
                                if let Some(
                                    kiapi::common::types::graphic_shape::Geometry::Polygon(polyset),
                                ) = graphic.geometry.as_mut()
                                {
                                    normalize_polyset(polyset);
                                }
                                if let Some(
                                    kiapi::common::types::graphic_shape::Geometry::Rectangle(
                                        rectangle,
                                    ),
                                ) = graphic.geometry.as_mut()
                                {
                                    if rectangle
                                        .corner_radius
                                        .as_ref()
                                        .is_some_and(|radius| radius.value_nm == 0)
                                    {
                                        rectangle.corner_radius = None;
                                    }
                                }
                            }
                            shape.encode_to_vec()
                        },
                    ),
                )
            } else if item.type_url.ends_with("BoardText") {
                Some(
                    kiapi::board::types::BoardText::decode(item.value.as_slice()).map(
                        |mut text| {
                            text.id = None;
                            text.encode_to_vec()
                        },
                    ),
                )
            } else {
                None
            }
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    graphics.sort();
    Ok(graphics)
}

fn normalize_polyset(polyset: &mut kiapi::common::types::PolySet) {
    for polygon in &mut polyset.polygons {
        if let Some(outline) = polygon.outline.as_mut() {
            normalize_polyline(outline);
        }
        for hole in &mut polygon.holes {
            normalize_polyline(hole);
        }
        polygon.holes.sort_by_key(prost::Message::encode_to_vec);
    }
    polyset.polygons.sort_by_key(prost::Message::encode_to_vec);
}

fn normalize_polyline(polyline: &mut kiapi::common::types::PolyLine) {
    if !polyline.closed || polyline.nodes.len() < 2 {
        return;
    }
    let candidates = [
        polyline.nodes.clone(),
        polyline.nodes.iter().cloned().rev().collect(),
    ];
    let mut best: Option<(Vec<u8>, Vec<kiapi::common::types::PolyLineNode>)> = None;
    for nodes in candidates {
        for offset in 0..nodes.len() {
            let rotated = nodes[offset..]
                .iter()
                .chain(nodes[..offset].iter())
                .cloned()
                .collect::<Vec<_>>();
            let encoded = rotated
                .iter()
                .flat_map(prost::Message::encode_to_vec)
                .collect::<Vec<_>>();
            if best
                .as_ref()
                .is_none_or(|(best_encoded, _)| encoded < *best_encoded)
            {
                best = Some((encoded, rotated));
            }
        }
    }
    if let Some((_, nodes)) = best {
        polyline.nodes = nodes;
    }
}

fn library_owned_attributes(
    attributes: Option<&kiapi::board::types::FootprintAttributes>,
) -> Option<(bool, bool, bool, bool, i32, bool)> {
    attributes.map(|attributes| {
        let mounting_style = match attributes.mounting_style() {
            kiapi::board::types::FootprintMountingStyle::FmsUnknown
            | kiapi::board::types::FootprintMountingStyle::FmsUnspecified => {
                kiapi::board::types::FootprintMountingStyle::FmsUnspecified as i32
            }
            style => style as i32,
        };
        (
            attributes.exclude_from_position_files,
            attributes.exclude_from_bill_of_materials,
            attributes.exempt_from_courtyard_requirement,
            attributes.allow_soldermask_bridges,
            mounting_style,
            attributes.not_in_schematic,
        )
    })
}

fn field_text(field: &Option<kiapi::board::types::Field>) -> String {
    field
        .as_ref()
        .and_then(|field| field.text.as_ref())
        .and_then(|text| text.text.as_ref())
        .map(|text| text.text.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use konnect_ipc::builders;
    use konnect_ipc::gen::kiapi;
    use prost::Message;
    use std::collections::{BTreeMap, BTreeSet};

    /// The serialization KiCad 10 actually writes — produced by running this
    /// module's hand-written fixture through `kicad-cli fp upgrade` — parses,
    /// and the two universal flags KiCad stamps into every saved footprint
    /// (`embedded_fonts no`, `duplicate_pad_numbers_are_jumpers no`, plus
    /// `remove_unused_layers no` on pads) do not refuse it. The hand-written
    /// fixture alone cannot catch this class: it shares this module's own
    /// assumptions about the format (see the fixtures-from-KiCad rule in
    /// CONTRIBUTING).
    #[test]
    fn a_footprint_kicad_saved_is_accepted_not_refused() {
        let source = KICAD_LIBRARY_FOOTPRINT;
        let library = parse_library_footprint("Konnect:Socket", source)
            .expect("KiCad's own serialization of the fixture must parse");
        assert_eq!(library.pads.len(), 4, "two '1' variants plus '2' and '3'");
        assert_eq!(
            library.graphics.len(),
            3,
            "one fp_line, one fp_poly, and KiCad's standard fab reference text"
        );
        assert!(library.graphics.iter().any(|graphic| {
            matches!(
                graphic,
                konnect_ipc::IpcGraphicDefinition::Text {
                    text,
                    position,
                    rotation,
                    layer,
                    size,
                    stroke_width_mm,
                } if text == "${REFERENCE}"
                    && *position == (0.0, 1.5)
                    && *rotation == 0.0
                    && layer == "F.Fab"
                    && *size == 1.0
                    && *stroke_width_mm == 0.15
            )
        }));
        assert_eq!(library.datasheet.as_deref(), Some("new-datasheet.pdf"));
        assert_eq!(
            library
                .properties
                .iter()
                .map(|property| (property.name.as_str(), field_text_value(property)))
                .collect::<Vec<_>>(),
            vec![
                ("KiLib_Generator", "konnect_test_generator"),
                ("AssemblyVendor", "Example Assembly"),
            ]
        );
        assert!(library.properties.iter().all(|property| !property.visible));
        assert_eq!(library.models.len(), 1);

        // The same flags at a non-default value carry semantics the typed
        // rebuild cannot represent, so they refuse rather than silently drop.
        for (from, to) in [
            ("(embedded_fonts no)", "(embedded_fonts yes)"),
            ("(remove_unused_layers no)", "(remove_unused_layers yes)"),
        ] {
            let flipped = source.replace(from, to);
            assert_ne!(flipped, source, "fixture must contain {from}");
            let error = parse_library_footprint("Konnect:Socket", &flipped)
                .expect_err("a non-default flag must refuse")
                .to_string();
            assert!(
                error.contains("not supported"),
                "refusal names the unsupported clause: {error}"
            );
        }
    }

    #[test]
    fn stock_kicad_generator_property_is_preserved_as_a_typed_field() {
        // Unmodified KiCad 10.0.5 standard-library output from
        // Capacitor_SMD.pretty/C_0603_1608Metric.kicad_mod. This is the stock
        // footprint that exposed #373; keeping its real generator-authored
        // shape prevents a synthetic fixture from agreeing with this parser.
        let source = include_str!("../../tests/fixtures/c_0603_1608metric_kicad10.kicad_mod");
        let library = parse_library_footprint("Capacitor_SMD:C_0603_1608Metric", source)
            .expect("the stock KiCad generator footprint must parse losslessly");

        assert_eq!(library.pads.len(), 2);
        let generator = library
            .properties
            .iter()
            .find(|property| property.name == "KiLib_Generator")
            .expect("KiCad's generator property must remain a typed Field");
        assert_eq!(field_text_value(generator), "SMD_2terminal_chip_molded");
        assert!(!generator.visible);
        assert_eq!(library.models.len(), 1);
    }

    #[test]
    fn visible_property_is_a_field_only_and_clause_order_does_not_change_its_angle() {
        let source = KICAD_LIBRARY_FOOTPRINT.replace(
            "\t\t(at 0.5 0.75 15)\n\t\t(layer \"F.Fab\")\n\t\t(hide yes)",
            "\t\t(layer \"F.Fab\")\n\t\t(hide no)\n\t\t(at 0.5 0.75 15)",
        );
        assert_ne!(source, KICAD_LIBRARY_FOOTPRINT);

        let library = parse_library_footprint("Konnect:Socket", &source).unwrap();
        let property = library
            .properties
            .iter()
            .find(|property| property.name == "AssemblyVendor")
            .unwrap();
        assert!(property.visible);
        assert_eq!(
            property
                .text
                .as_ref()
                .unwrap()
                .text
                .as_ref()
                .unwrap()
                .attributes
                .as_ref()
                .unwrap()
                .angle
                .as_ref()
                .unwrap()
                .value_degrees,
            15.0
        );
        assert!(!library.graphics.iter().any(|graphic| matches!(
            graphic,
            konnect_ipc::IpcGraphicDefinition::Text { text, .. }
                if text == "Example Assembly"
        )));
    }

    /// `preserved` must be a comparison of the rebuilt instance against the
    /// board's, never a policy constant: when the two instances genuinely
    /// diverge, the flags have to say so.
    #[test]
    fn preserved_flags_come_from_comparison_not_policy() {
        let pad = |number: &str, net: Option<&str>| {
            let item = kiapi::board::types::Pad {
                number: number.to_string(),
                net: net.map(|name| kiapi::board::types::Net {
                    code: Some(kiapi::board::types::NetCode { value: 3 }),
                    name: name.to_string(),
                }),
                ..Default::default()
            };
            builders::pack_any(&item, "kiapi.board.types.Pad")
        };
        let current = kiapi::board::types::FootprintInstance {
            position: Some(builders::vec2(10.0, 20.0)),
            orientation: Some(kiapi::common::types::Angle {
                value_degrees: 90.0,
            }),
            layer: kiapi::board::types::BoardLayer::BlFCu as i32,
            definition: Some(kiapi::board::types::Footprint {
                items: vec![pad("1", Some("GND"))],
                ..Default::default()
            }),
            ..Default::default()
        };
        let old_nets = BTreeMap::from([(
            "1".to_string(),
            kiapi::board::types::Net {
                code: Some(kiapi::board::types::NetCode { value: 3 }),
                name: "GND".to_string(),
            },
        )]);

        let identical = PreservedState::derive(&current, &current.clone(), &old_nets);
        assert!(
            identical.position
                && identical.rotation
                && identical.layer
                && identical.locked
                && identical.kiid
                && identical.symbol_path
                && identical.pad_nets
                && identical.instance_overrides,
            "an untouched instance preserves everything: {identical:?}"
        );

        let mut moved = current.clone();
        moved.position = Some(builders::vec2(99.0, 99.0));
        moved.definition.as_mut().unwrap().items = vec![pad("1", None)];
        let diverged = PreservedState::derive(&current, &moved, &old_nets);
        assert!(
            !diverged.position,
            "a moved instance must report position unpreserved"
        );
        assert!(
            !diverged.pad_nets,
            "a dropped pad net must report pad_nets unpreserved"
        );
        assert!(
            diverged.rotation && diverged.layer && diverged.kiid,
            "untouched fields stay preserved: {diverged:?}"
        );
    }

    const KICAD_LIBRARY_FOOTPRINT: &str =
        include_str!("../../tests/fixtures/socket_kicad10.kicad_mod");

    const LIBRARY_FOOTPRINT: &str = r#"
(footprint "Socket"
  (version 20240108)
  (generator "konnect")
  (layer "F.Cu")
  (descr "updated description")
  (tags "keyboard socket")
  (attr smd exclude_from_pos_files)
  (fp_line (start -1 -2) (end 3 4)
    (stroke (width 0.12) (type solid))
    (layer "F.SilkS"))
  (fp_poly (pts (xy -2 -1) (xy 2 -1) (xy 2 1) (xy -2 1))
    (stroke (width 0.05) (type solid))
    (fill none)
    (layer "B.CrtYd"))
  (pad "1" smd roundrect (at -2 0 15) (size 2 1)
    (layers "B.Cu" "B.Paste" "B.Mask")
    (roundrect_rratio 0.2))
  (pad "1" smd rect (at -1 0) (size 1 1) (layers "B.Cu"))
  (pad "2" thru_hole circle (at 2 0) (size 3 3)
    (layers "*.Cu" "*.Mask") (drill 1))
  (pad "3" smd rect (at 0 3) (size 1 1) (layers "F.Cu"))
  (fp_text reference "REF**" (at 0 -4 0) (layer "F.SilkS")
    (effects (font (size 1 1) (thickness 0.15))))
  (fp_text value "Socket" (at 0 4 0) (layer "F.Fab")
    (effects (font (size 1 1) (thickness 0.15))))
  (fp_text user "${REFERENCE}" (at 0 1.5 0) (layer "F.Fab")
    (uuid "f96c2efe-5925-4f74-81d2-f89a56f57e13")
    (effects (font (size 0.8 0.8) (thickness 0.11))))
  (property "Datasheet" "new-datasheet.pdf" (at 0 0) (layer "F.Fab") (hide yes))
  (property "Description" "new field description" (at 0 0) (layer "F.Fab") (hide yes))
  (model "../models/Socket.step"
    (offset (xyz 1 2 3))
    (scale (xyz -1 1 1))
    (rotate (xyz 90 0 45)))
)
"#;

    fn field(name: &str, value: &str, x: f64, y: f64, visible: bool) -> kiapi::board::types::Field {
        kiapi::board::types::Field {
            name: name.to_string(),
            visible,
            text: Some(kiapi::board::types::BoardText {
                text: Some(kiapi::common::types::Text {
                    position: Some(builders::vec2(x, y)),
                    attributes: Some(kiapi::common::types::TextAttributes {
                        size: Some(builders::vec2(1.25, 1.25)),
                        angle: Some(kiapi::common::types::Angle {
                            value_degrees: 17.0,
                        }),
                        mirrored: true,
                        ..Default::default()
                    }),
                    text: value.to_string(),
                    ..Default::default()
                }),
                layer: kiapi::board::types::BoardLayer::BlBSilkS as i32,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn current_pad(number: &str, net_name: &str, net_code: i32) -> prost_types::Any {
        builders::pack_any(
            &kiapi::board::types::Pad {
                id: Some(kiapi::common::types::Kiid {
                    value: format!("old-pad-{number}-{net_code}"),
                }),
                number: number.to_string(),
                net: Some(kiapi::board::types::Net {
                    code: Some(kiapi::board::types::NetCode { value: net_code }),
                    name: net_name.to_string(),
                }),
                position: Some(builders::vec2(100.0, 50.0)),
                ..Default::default()
            },
            "kiapi.board.types.Pad",
        )
    }

    fn current_instance(
        layer: kiapi::board::types::BoardLayer,
    ) -> kiapi::board::types::FootprintInstance {
        let reference = field("Reference", "SW1", 101.0, 48.0, true);
        let value = field("Value", "Socket Value", 99.0, 52.0, false);
        kiapi::board::types::FootprintInstance {
            id: Some(kiapi::common::types::Kiid {
                value: "instance-kiid".to_string(),
            }),
            position: Some(builders::vec2(100.0, 50.0)),
            orientation: Some(kiapi::common::types::Angle {
                value_degrees: 37.0,
            }),
            layer: layer as i32,
            locked: kiapi::common::types::LockedState::LsLocked as i32,
            definition: Some(kiapi::board::types::Footprint {
                id: Some(kiapi::common::types::LibraryIdentifier {
                    library_nickname: "Test".to_string(),
                    entry_name: "Socket".to_string(),
                }),
                reference_field: Some(reference.clone()),
                value_field: Some(value.clone()),
                items: vec![current_pad("1", "ROW1", 11), current_pad("2", "COL1", 12)],
                ..Default::default()
            }),
            reference_field: Some(reference),
            value_field: Some(value),
            datasheet_field: Some(field("Datasheet", "placed-datasheet", 0.0, 0.0, false)),
            description_field: Some(field("Description", "placed-description", 0.0, 0.0, false)),
            attributes: Some(kiapi::board::types::FootprintAttributes {
                not_in_schematic: false,
                exclude_from_bill_of_materials: true,
                do_not_populate: true,
                ..Default::default()
            }),
            overrides: Some(kiapi::board::types::FootprintDesignRuleOverrides {
                copper_clearance: Some(builders::distance(0.3)),
                ..Default::default()
            }),
            symbol_path: Some(kiapi::common::types::SheetPath {
                path: vec![kiapi::common::types::Kiid {
                    value: "sheet-kiid".to_string(),
                }],
                path_human_readable: "/Keyboard".to_string(),
            }),
            symbol_sheet_name: "Keyboard".to_string(),
            symbol_sheet_filename: "keyboard.kicad_sch".to_string(),
            symbol_footprint_filters: "Test:*".to_string(),
            parent: None,
        }
    }

    fn decoded_pads(
        instance: &kiapi::board::types::FootprintInstance,
    ) -> Vec<kiapi::board::types::Pad> {
        instance
            .definition
            .as_ref()
            .unwrap()
            .items
            .iter()
            .filter(|item| item.type_url.ends_with("kiapi.board.types.Pad"))
            .map(|item| kiapi::board::types::Pad::decode(item.value.as_slice()).unwrap())
            .collect()
    }

    fn decoded_custom_fields(
        instance: &kiapi::board::types::FootprintInstance,
    ) -> Vec<kiapi::board::types::Field> {
        instance
            .definition
            .as_ref()
            .unwrap()
            .items
            .iter()
            .filter(|item| item.type_url.ends_with("kiapi.board.types.Field"))
            .map(|item| {
                kiapi::board::types::Field::decode(item.value.as_slice())
                    .expect("custom field must decode")
            })
            .collect()
    }

    fn field_text_value(field: &kiapi::board::types::Field) -> &str {
        field
            .text
            .as_ref()
            .and_then(|text| text.text.as_ref())
            .map(|text| text.text.as_str())
            .unwrap_or_default()
    }

    #[test]
    fn parses_supported_library_definition_without_dropping_domains() {
        let library = parse_library_footprint("Test:Socket", LIBRARY_FOOTPRINT).unwrap();

        assert_eq!(library.library_id, "Test:Socket");
        assert_eq!(
            library.definition.attributes.as_ref().unwrap().description,
            "updated description"
        );
        assert_eq!(
            library.definition.attributes.as_ref().unwrap().keywords,
            "keyboard socket"
        );
        assert_eq!(library.pads.len(), 4);
        assert_eq!(library.graphics.len(), 3);
        assert!(library.graphics.iter().any(|graphic| matches!(
            graphic,
            konnect_ipc::IpcGraphicDefinition::Text {
                text,
                size,
                stroke_width_mm,
                ..
            } if text == "${REFERENCE}" && *size == 0.8 && *stroke_width_mm == 0.11
        )));
        assert_eq!(library.models.len(), 1);
        let model = &library.models[0];
        assert_eq!(model.filename, "../models/Socket.step");
        assert_eq!(model.offset.as_ref().unwrap().x_nm, 1.0);
        assert_eq!(model.scale.as_ref().unwrap().x_nm, -1.0);
        assert_eq!(model.rotation.as_ref().unwrap().z_nm, 45.0);
    }

    #[test]
    fn merge_preserves_instance_state_and_nets_by_logical_pad_number() {
        let current = current_instance(kiapi::board::types::BoardLayer::BlBCu);
        let library = parse_library_footprint("Test:Socket", KICAD_LIBRARY_FOOTPRINT).unwrap();
        let prepared = build_updated_instance(
            &current,
            &library,
            &BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]),
            &BTreeSet::from(["ROW1".to_string(), "COL1".to_string()]),
        )
        .unwrap();
        let updated =
            kiapi::board::types::FootprintInstance::decode(prepared.item.value.as_slice()).unwrap();

        assert_eq!(updated.id, current.id);
        assert_eq!(updated.position, current.position);
        assert_eq!(updated.orientation, current.orientation);
        assert_eq!(updated.layer, current.layer);
        assert_eq!(updated.locked, current.locked);
        assert_eq!(updated.reference_field, current.reference_field);
        assert_eq!(updated.value_field, current.value_field);
        assert_eq!(field_text(&updated.datasheet_field), "new-datasheet.pdf");
        assert_eq!(
            updated
                .datasheet_field
                .as_ref()
                .and_then(|field| field.text.as_ref())
                .and_then(|text| text.text.as_ref())
                .and_then(|text| text.attributes.as_ref()),
            current
                .datasheet_field
                .as_ref()
                .and_then(|field| field.text.as_ref())
                .and_then(|text| text.text.as_ref())
                .and_then(|text| text.attributes.as_ref())
        );
        assert_eq!(
            field_text(&updated.description_field),
            "new field description"
        );
        let attributes = updated.attributes.as_ref().unwrap();
        let current_attributes = current.attributes.as_ref().unwrap();
        assert_eq!(
            attributes.not_in_schematic,
            current_attributes.not_in_schematic
        );
        assert_eq!(
            attributes.do_not_populate,
            current_attributes.do_not_populate
        );
        assert!(attributes.exclude_from_position_files);
        assert!(!attributes.exclude_from_bill_of_materials);
        assert_eq!(
            attributes.mounting_style,
            kiapi::board::types::FootprintMountingStyle::FmsSmd as i32
        );
        assert_eq!(updated.overrides, current.overrides);
        assert_eq!(updated.symbol_path, current.symbol_path);
        assert_eq!(updated.symbol_sheet_name, current.symbol_sheet_name);
        assert_eq!(updated.symbol_sheet_filename, current.symbol_sheet_filename);
        assert_eq!(
            updated.symbol_footprint_filters,
            current.symbol_footprint_filters
        );

        let properties = decoded_custom_fields(&updated);
        assert_eq!(
            properties
                .iter()
                .map(|property| (property.name.as_str(), field_text_value(property)))
                .collect::<Vec<_>>(),
            vec![
                ("KiLib_Generator", "konnect_test_generator"),
                ("AssemblyVendor", "Example Assembly"),
            ]
        );
        let assembly = properties
            .iter()
            .find(|property| property.name == "AssemblyVendor")
            .unwrap();
        let assembly_text = assembly.text.as_ref().unwrap();
        assert_eq!(
            assembly_text.layer,
            kiapi::board::types::BoardLayer::BlBFab as i32
        );
        let text = assembly_text.text.as_ref().unwrap();
        let expected = konnect_sexp::geometry::transform_pad(0.5, -0.75, 100.0, 50.0, 37.0);
        let actual = text.position.as_ref().unwrap();
        let actual = (
            builders::nm_to_mm(actual.x_nm),
            builders::nm_to_mm(actual.y_nm),
        );
        assert!((actual.0 - expected.0).abs() <= 0.000_001);
        assert!((actual.1 - expected.1).abs() <= 0.000_001);
        let attributes = text.attributes.as_ref().unwrap();
        assert!(attributes.mirrored);
        assert_eq!(
            attributes.angle.as_ref().unwrap().value_degrees,
            readable_property_angle(180.0 - 15.0 + 37.0)
        );

        let pads = decoded_pads(&updated);
        assert_eq!(pads.iter().filter(|pad| pad.number == "1").count(), 2);
        assert!(pads.iter().filter(|pad| pad.number == "1").all(|pad| pad
            .net
            .as_ref()
            .map(|net| net.name.as_str())
            == Some("ROW1")));
        assert_eq!(
            pads.iter()
                .find(|pad| pad.number == "2")
                .and_then(|pad| pad.net.as_ref())
                .map(|net| net.name.as_str()),
            Some("COL1")
        );
        assert!(pads
            .iter()
            .find(|pad| pad.number == "3")
            .unwrap()
            .net
            .is_none());
        let flipped_pad = pads
            .iter()
            .find(|pad| pad.number == "3")
            .expect("new pad 3");
        let flipped_stack = flipped_pad.pad_stack.as_ref().expect("pad stack");
        assert_eq!(
            flipped_stack.layers,
            vec![kiapi::board::types::BoardLayer::BlBCu as i32],
            "a front-copper library pad moves to back copper with a B.Cu instance"
        );
        let expected = konnect_sexp::geometry::transform_pad(0.0, -3.0, 100.0, 50.0, 37.0);
        let actual = flipped_pad.position.as_ref().expect("pad position");
        let actual = (
            builders::nm_to_mm(actual.x_nm),
            builders::nm_to_mm(actual.y_nm),
        );
        assert!((actual.0 - expected.0).abs() <= 0.000_001);
        assert!((actual.1 - expected.1).abs() <= 0.000_001);
        assert!(prepared.changed_domains.contains(&ChangedDomain::Pads));
        assert!(prepared.changed_domains.contains(&ChangedDomain::Graphics));
        assert!(prepared.changed_domains.contains(&ChangedDomain::Metadata));
        assert!(prepared.changed_domains.contains(&ChangedDomain::Models));
    }

    #[test]
    fn merge_preserves_instance_only_properties_and_refreshes_library_properties() {
        let mut current = current_instance(kiapi::board::types::BoardLayer::BlFCu);
        let instance_only = field("InstanceNote", "keep this", 105.0, 55.0, false);
        let stale_library_property = field("AssemblyVendor", "old library value", 99.0, 49.0, true);
        current.definition.as_mut().unwrap().items.extend([
            builders::pack_any(&instance_only, "kiapi.board.types.Field"),
            builders::pack_any(&stale_library_property, "kiapi.board.types.Field"),
        ]);
        let library = parse_library_footprint("Test:Socket", KICAD_LIBRARY_FOOTPRINT).unwrap();

        let prepared = build_updated_instance(
            &current,
            &library,
            &BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]),
            &BTreeSet::new(),
        )
        .unwrap();
        let updated =
            kiapi::board::types::FootprintInstance::decode(prepared.item.value.as_slice()).unwrap();
        let properties = decoded_custom_fields(&updated);

        assert_eq!(
            properties
                .iter()
                .map(|property| property.name.as_str())
                .collect::<Vec<_>>(),
            vec!["InstanceNote", "KiLib_Generator", "AssemblyVendor"]
        );
        assert_eq!(
            properties
                .iter()
                .find(|property| property.name == "InstanceNote")
                .unwrap(),
            &instance_only
        );
        assert_eq!(
            field_text_value(
                properties
                    .iter()
                    .find(|property| property.name == "AssemblyVendor")
                    .unwrap()
            ),
            "Example Assembly"
        );
        assert!(prepared.changed_domains.contains(&ChangedDomain::Metadata));
    }

    #[test]
    fn merge_conflicts_when_library_removes_a_connected_logical_pad() {
        let current = current_instance(kiapi::board::types::BoardLayer::BlFCu);
        let without_pad_two = LIBRARY_FOOTPRINT.replace(
            "  (pad \"2\" thru_hole circle (at 2 0) (size 3 3)\n    (layers \"*.Cu\" \"*.Mask\") (drill 1))\n",
            "",
        );
        let library = parse_library_footprint("Test:Socket", &without_pad_two).unwrap();

        let error = build_updated_instance(
            &current,
            &library,
            &BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]),
            &BTreeSet::from(["COL1".to_string()]),
        )
        .unwrap_err();

        assert!(error.to_string().contains("connected pad 2"), "{error:#}");
    }

    #[test]
    fn parser_rejects_unsupported_children_before_any_update_can_be_built() {
        let unsupported = LIBRARY_FOOTPRINT.replace(
            "  (model \"../models/Socket.step\"",
            "  (zone (net 0) (layers \"F.Cu\"))\n  (model \"../models/Socket.step\"",
        );

        let error = parse_library_footprint("Test:Socket", &unsupported).unwrap_err();

        assert!(error.to_string().contains("zone"), "{error:#}");
    }

    #[test]
    fn parser_names_unrepresentable_or_ambiguous_custom_properties() {
        let unsupported = KICAD_LIBRARY_FOOTPRINT.replace(
            "\t(property \"AssemblyVendor\" \"Example Assembly\"\n\t\t(at",
            "\t(property \"AssemblyVendor\" \"Example Assembly\"\n\t\t(render_cache legacy)\n\t\t(at",
        );
        assert_ne!(unsupported, KICAD_LIBRARY_FOOTPRINT);
        let error = parse_library_footprint("Test:Socket", &unsupported).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("AssemblyVendor"), "{message}");
        assert!(message.contains("render_cache"), "{message}");

        let duplicate = KICAD_LIBRARY_FOOTPRINT.replace(
            "\"KiLib_Generator\" \"konnect_test_generator\"",
            "\"AssemblyVendor\" \"konnect_test_generator\"",
        );
        assert_ne!(duplicate, KICAD_LIBRARY_FOOTPRINT);
        let error = parse_library_footprint("Test:Socket", &duplicate).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("AssemblyVendor"), "{message}");
        assert!(message.contains("more than once"), "{message}");
    }

    #[test]
    fn mandatory_properties_are_validated_and_duplicate_names_refuse() {
        let mut accepted = Vec::new();
        for (name, value) in [
            ("Reference", "REF**"),
            ("Value", "Socket"),
            ("Datasheet", "new-datasheet.pdf"),
            ("Description", "new field description"),
        ] {
            let authored = format!("\t(property \"{name}\" \"{value}\"\n\t\t(at");
            let unsupported = KICAD_LIBRARY_FOOTPRINT.replace(
                &authored,
                &format!("\t(property \"{name}\" \"{value}\"\n\t\t(render_cache legacy)\n\t\t(at"),
            );
            assert_ne!(unsupported, KICAD_LIBRARY_FOOTPRINT);
            match parse_library_footprint("Test:Socket", &unsupported) {
                Ok(_) => accepted.push(format!("{name} unsupported clause")),
                Err(error) => {
                    let message = error.to_string();
                    assert!(message.contains(name), "{message}");
                    assert!(message.contains("render_cache"), "{message}");
                }
            }

            let duplicate = KICAD_LIBRARY_FOOTPRINT.replace(
                "\"KiLib_Generator\" \"konnect_test_generator\"",
                &format!("\"{name}\" \"konnect_test_generator\""),
            );
            assert_ne!(duplicate, KICAD_LIBRARY_FOOTPRINT);
            match parse_library_footprint("Test:Socket", &duplicate) {
                Ok(_) => accepted.push(format!("{name} duplicate name")),
                Err(error) => {
                    let message = error.to_string();
                    assert!(message.contains(name), "{message}");
                    assert!(message.contains("more than once"), "{message}");
                }
            }
        }
        assert!(
            accepted.is_empty(),
            "parser accepted mandatory-property cases that must refuse: {}",
            accepted.join(", ")
        );
    }

    #[test]
    fn repeated_scalar_property_clauses_refuse() {
        let mut accepted = Vec::new();
        let vendor_prefix = "\t(property \"AssemblyVendor\" \"Example Assembly\"\n\t\t(at 0.5 0.75 15)\n\t\t(layer \"F.Fab\")\n\t\t(hide yes)";
        for (clause, replacement) in [
            ("hide", format!("{vendor_prefix}\n\t\t(hide no)")),
            (
                "knockout",
                format!("{vendor_prefix}\n\t\t(knockout yes)\n\t\t(knockout no)"),
            ),
        ] {
            let repeated = KICAD_LIBRARY_FOOTPRINT.replace(vendor_prefix, &replacement);
            assert_ne!(repeated, KICAD_LIBRARY_FOOTPRINT);
            match parse_library_footprint("Test:Socket", &repeated) {
                Ok(_) => accepted.push(clause),
                Err(error) => {
                    let message = error.to_string();
                    assert!(message.contains("AssemblyVendor"), "{message}");
                    assert!(message.contains(clause), "{message}");
                }
            }
        }

        let vendor_font =
            "\t\t\t\t(size 1 1)\n\t\t\t\t(thickness 0.15)\n\t\t\t)\n\t\t)\n\t)\n\t(attr";
        for (clause, repeated_clauses) in [
            ("bold", "\t\t\t\t(bold yes)\n\t\t\t\t(bold no)\n"),
            ("italic", "\t\t\t\t(italic yes)\n\t\t\t\t(italic no)\n"),
            (
                "line_spacing",
                "\t\t\t\t(line_spacing 1.1)\n\t\t\t\t(line_spacing 1.2)\n",
            ),
        ] {
            let replacement =
                vendor_font.replace("\t\t\t)\n", &format!("{repeated_clauses}\t\t\t)\n"));
            let repeated = KICAD_LIBRARY_FOOTPRINT.replace(vendor_font, &replacement);
            assert_ne!(repeated, KICAD_LIBRARY_FOOTPRINT);
            match parse_library_footprint("Test:Socket", &repeated) {
                Ok(_) => accepted.push(clause),
                Err(error) => {
                    let message = error.to_string();
                    assert!(message.contains("AssemblyVendor"), "{message}");
                    assert!(message.contains(clause), "{message}");
                }
            }
        }
        assert!(
            accepted.is_empty(),
            "parser accepted repeated scalar clauses that must refuse: {}",
            accepted.join(", ")
        );
    }

    #[test]
    fn refresh_refuses_an_unrepresentable_layer_before_building_the_update() {
        let unsupported = LIBRARY_FOOTPRINT.replace("(layer \"F.SilkS\"))", "(layer \"In99.Cu\"))");
        let library = parse_library_footprint("Test:Socket", &unsupported).unwrap();
        let current = current_instance(kiapi::board::types::BoardLayer::BlFCu);

        let error = build_updated_instance(
            &current,
            &library,
            &BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]),
            &BTreeSet::new(),
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("In99.Cu"), "{message}");
        assert!(message.contains("request was not sent"), "{message}");
    }

    #[test]
    fn parser_rejects_lossy_graphic_and_pad_clauses() {
        let dashed = LIBRARY_FOOTPRINT.replace(
            "(stroke (width 0.12) (type solid))",
            "(stroke (width 0.12) (type dash))",
        );
        let error = parse_library_footprint("Test:Socket", &dashed).unwrap_err();
        assert!(error.to_string().contains("stroke type"), "{error:#}");

        let solder_margin = LIBRARY_FOOTPRINT.replace(
            "(roundrect_rratio 0.2))",
            "(roundrect_rratio 0.2) (solder_mask_margin 0.05))",
        );
        let error = parse_library_footprint("Test:Socket", &solder_margin).unwrap_err();
        assert!(
            error.to_string().contains("solder_mask_margin"),
            "{error:#}"
        );
    }

    #[test]
    fn parser_rejects_lossy_user_text_variants() {
        for (from, to, expected) in [
            (
                "(at 0 1.5 0) (layer \"F.Fab\")",
                "(at 0 1.5 0) (unlocked yes) (layer \"F.Fab\")",
                "unlocked",
            ),
            (
                "(effects (font (size 0.8 0.8) (thickness 0.11)))",
                "(effects (font (size 0.8 0.7) (thickness 0.11)))",
                "non-square",
            ),
            (
                "(effects (font (size 0.8 0.8) (thickness 0.11)))",
                "(effects (font (size 0.8 0.8) (thickness 0.11)) (justify left))",
                "justify",
            ),
            (
                "(effects (font (size 0.8 0.8) (thickness 0.11)))",
                "(effects (font (size 0.8 0.8) (thickness 0.11) (italic yes)))",
                "italic",
            ),
            (
                "(effects (font (size 0.8 0.8) (thickness 0.11)))",
                "(effects (font (size 0.8 0.8)))",
                "missing its thickness",
            ),
            (
                "(uuid \"f96c2efe-5925-4f74-81d2-f89a56f57e13\")",
                "(uuid \"f96c2efe-5925-4f74-81d2-f89a56f57e13\") (uuid \"duplicate\")",
                "multiple identifier clauses",
            ),
            (
                "(uuid \"f96c2efe-5925-4f74-81d2-f89a56f57e13\")",
                "(uuid \"f96c2efe-5925-4f74-81d2-f89a56f57e13\") (tstamp \"legacy-too\")",
                "multiple identifier clauses",
            ),
        ] {
            let unsupported = LIBRARY_FOOTPRINT.replacen(from, to, 1);
            let error = parse_library_footprint("Test:Socket", &unsupported).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn rebuilding_an_applied_instance_is_a_noop_at_every_rotation_and_side() {
        let library = parse_library_footprint("Test:Socket", KICAD_LIBRARY_FOOTPRINT).unwrap();
        let net_codes = BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]);
        let routed = BTreeSet::from(["ROW1".to_string(), "COL1".to_string()]);

        for layer in [
            kiapi::board::types::BoardLayer::BlFCu,
            kiapi::board::types::BoardLayer::BlBCu,
        ] {
            for rotation in [0.0, 90.0, 180.0, 270.0, 37.0] {
                let mut current = current_instance(layer);
                current.orientation = Some(kiapi::common::types::Angle {
                    value_degrees: rotation,
                });
                let first =
                    build_updated_instance(&current, &library, &net_codes, &routed).unwrap();
                let applied =
                    kiapi::board::types::FootprintInstance::decode(first.item.value.as_slice())
                        .unwrap();

                let second =
                    build_updated_instance(&applied, &library, &net_codes, &routed).unwrap();

                assert!(
                    second.changed_domains.is_empty(),
                    "{layer:?} at {rotation} degrees changed again: {:?}",
                    second.changed_domains
                );
            }
        }
    }

    #[test]
    fn kicad_roundtrip_defaults_do_not_create_false_pad_or_graphic_changes() {
        let library = parse_library_footprint("Test:Socket", LIBRARY_FOOTPRINT).unwrap();
        let current = current_instance(kiapi::board::types::BoardLayer::BlFCu);
        let prepared = build_updated_instance(
            &current,
            &library,
            &BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]),
            &BTreeSet::new(),
        )
        .unwrap();
        let mut prepared =
            kiapi::board::types::FootprintInstance::decode(prepared.item.value.as_slice()).unwrap();
        let mut roundtripped = prepared.clone();
        roundtripped.definition.as_mut().unwrap().items.reverse();
        roundtripped.attributes.as_mut().unwrap().mounting_style =
            kiapi::board::types::FootprintMountingStyle::FmsUnspecified as i32;
        prepared.attributes.as_mut().unwrap().mounting_style =
            kiapi::board::types::FootprintMountingStyle::FmsUnknown as i32;

        for item in &mut roundtripped.definition.as_mut().unwrap().items {
            if item.type_url.ends_with("kiapi.board.types.Pad") {
                let mut pad = kiapi::board::types::Pad::decode(item.value.as_slice()).unwrap();
                pad.id = Some(kiapi::common::types::Kiid {
                    value: format!("kicad-pad-{}", pad.number),
                });
                pad.net = Some(kiapi::board::types::Net::default());
                pad.pad_to_die_length = Some(kiapi::common::types::Distance { value_nm: 0 });
                pad.symbol_pin = Some(kiapi::board::types::SymbolPinInfo::default());
                pad.pad_to_die_delay = Some(kiapi::common::types::Time { value_as: 0 });
                let stack = pad.pad_stack.as_mut().unwrap();
                stack.layers.reverse();
                if stack.drill.is_none() {
                    stack.drill = Some(kiapi::board::types::DrillProperties {
                        start_layer: kiapi::board::types::BoardLayer::BlUndefined as i32,
                        end_layer: kiapi::board::types::BoardLayer::BlUndefined as i32,
                        diameter: Some(builders::vec2(0.0, 0.0)),
                        shape: kiapi::board::types::DrillShape::DsUndefined as i32,
                        ..Default::default()
                    });
                }
                stack.zone_settings = Some(kiapi::board::types::ZoneConnectionSettings {
                    zone_connection: kiapi::board::types::ZoneConnectionStyle::ZcsInherited as i32,
                    thermal_spokes: Some(kiapi::board::types::ThermalSpokeSettings {
                        angle: Some(kiapi::common::types::Angle {
                            value_degrees: 90.0,
                        }),
                        ..Default::default()
                    }),
                });
                *item = builders::pack_any(&pad, "kiapi.board.types.Pad");
            } else if item
                .type_url
                .ends_with("kiapi.board.types.BoardGraphicShape")
            {
                let mut graphic =
                    kiapi::board::types::BoardGraphicShape::decode(item.value.as_slice()).unwrap();
                graphic.id = Some(kiapi::common::types::Kiid {
                    value: "kicad-graphic".to_string(),
                });
                graphic.net = Some(kiapi::board::types::Net::default());
                graphic
                    .shape
                    .as_mut()
                    .unwrap()
                    .attributes
                    .as_mut()
                    .unwrap()
                    .stroke
                    .as_mut()
                    .unwrap()
                    .style = kiapi::common::types::StrokeLineStyle::SlsSolid as i32;
                if let Some(kiapi::common::types::graphic_shape::Geometry::Polygon(polyset)) =
                    graphic
                        .shape
                        .as_mut()
                        .and_then(|shape| shape.geometry.as_mut())
                {
                    if let Some(outline) = polyset
                        .polygons
                        .first_mut()
                        .and_then(|polygon| polygon.outline.as_mut())
                    {
                        outline.nodes.rotate_left(1);
                        outline.nodes.reverse();
                    }
                }
                if let Some(kiapi::common::types::graphic_shape::Geometry::Rectangle(rectangle)) =
                    graphic
                        .shape
                        .as_mut()
                        .and_then(|shape| shape.geometry.as_mut())
                {
                    rectangle.corner_radius = Some(kiapi::common::types::Distance { value_nm: 0 });
                }
                *item = builders::pack_any(&graphic, "kiapi.board.types.BoardGraphicShape");
            }
        }

        let roundtripped_pads = normalized_items(roundtripped.definition.as_ref().unwrap(), "Pad")
            .unwrap()
            .into_iter()
            .map(|bytes| kiapi::board::types::Pad::decode(bytes.as_slice()).unwrap())
            .collect::<Vec<_>>();
        let prepared_pads = normalized_items(prepared.definition.as_ref().unwrap(), "Pad")
            .unwrap()
            .into_iter()
            .map(|bytes| kiapi::board::types::Pad::decode(bytes.as_slice()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(roundtripped_pads, prepared_pads);
        assert!(changed_domains(&roundtripped, &prepared)
            .unwrap()
            .is_empty());
    }

    fn plan_item(reference: &str, library_id: Option<&str>, kiid: &str) -> prost_types::Any {
        let mut instance = current_instance(kiapi::board::types::BoardLayer::BlFCu);
        instance.id = Some(kiapi::common::types::Kiid {
            value: kiid.to_string(),
        });
        instance.reference_field = Some(field("Reference", reference, 101.0, 48.0, true));
        if let Some(definition) = instance.definition.as_mut() {
            definition.reference_field = instance.reference_field.clone();
            definition.id = library_id.map(|library_id| {
                let (nickname, entry) = library_id.split_once(':').unwrap();
                kiapi::common::types::LibraryIdentifier {
                    library_nickname: nickname.to_string(),
                    entry_name: entry.to_string(),
                }
            });
        }
        builders::pack_any(&instance, "kiapi.board.types.FootprintInstance")
    }

    fn plan_fixture() -> (tempfile::TempDir, std::path::PathBuf, Vec<prost_types::Any>) {
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("board.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (version 20240108))").unwrap();
        let library_dir = temp.path().join("Test.pretty");
        std::fs::create_dir(&library_dir).unwrap();
        std::fs::write(
            library_dir.join("Socket.kicad_mod"),
            KICAD_LIBRARY_FOOTPRINT,
        )
        .unwrap();
        std::fs::write(
            temp.path().join("fp-lib-table"),
            "(fp_lib_table (lib (name \"Test\") (type \"KiCad\") (uri \"${KIPRJMOD}/Test.pretty\") (options \"\") (descr \"\")))",
        )
        .unwrap();
        let items = vec![
            plan_item("SW2", Some("Test:Socket"), "kiid-2"),
            plan_item("TP1", None, "kiid-3"),
            plan_item("SW1", Some("Test:Socket"), "kiid-1"),
        ];
        (temp, board, items)
    }

    #[test]
    fn planner_refuses_an_unrepresentable_property_before_preparing_any_update() {
        let (temp, board, items) = plan_fixture();
        let unsupported = KICAD_LIBRARY_FOOTPRINT.replace(
            "\t(property \"AssemblyVendor\" \"Example Assembly\"\n\t\t(at",
            "\t(property \"AssemblyVendor\" \"Example Assembly\"\n\t\t(render_cache legacy)\n\t\t(at",
        );
        std::fs::write(
            temp.path().join("Test.pretty/Socket.kicad_mod"),
            unsupported,
        )
        .unwrap();

        let plan = plan_updates(
            &board,
            &items,
            &BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]),
            &BTreeSet::new(),
            &UpdateFilters::default(),
        );

        assert_eq!(plan.status, PlanStatus::Conflict);
        assert!(plan.prepared_items.is_empty());
        assert!(plan.changes.is_empty());
        assert!(plan.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "unsupported_library_footprint"
                && diagnostic.message.contains("AssemblyVendor")
                && diagnostic.message.contains("render_cache")
        }));
    }

    #[test]
    fn filters_are_normalized_and_supplied_empty_arrays_select_nothing() {
        let filters = parse_filters(&serde_json::json!({
            "references": ["SW2", "SW1", "SW2"],
            "library_ids": ["Test:Socket", "Test:Socket"]
        }))
        .unwrap();
        assert_eq!(
            filters.references,
            Some(BTreeSet::from(["SW1".to_string(), "SW2".to_string()]))
        );
        assert_eq!(
            filters.library_ids,
            Some(BTreeSet::from(["Test:Socket".to_string()]))
        );

        let (_, board, items) = plan_fixture();
        let empty = plan_updates(
            &board,
            &items,
            &BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]),
            &BTreeSet::new(),
            &parse_filters(&serde_json::json!({ "references": [] })).unwrap(),
        );
        assert_eq!(empty.status, PlanStatus::Noop);
        assert_eq!(empty.coverage.selected.planned, 0);
        assert!(empty.changes.is_empty());

        let malformed =
            parse_filters(&serde_json::json!({ "library_ids": ["not-a-library-id"] })).unwrap_err();
        assert_eq!(malformed.field, "library_ids");
        assert!(malformed.reason.contains("Library:Footprint"));
    }

    #[test]
    fn planner_selects_deterministically_and_intersects_filters() {
        let (_temp, board, items) = plan_fixture();
        let net_codes = BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]);
        let all = plan_updates(
            &board,
            &items,
            &net_codes,
            &BTreeSet::new(),
            &UpdateFilters::default(),
        );
        assert_eq!(all.status, PlanStatus::Ready);
        assert_eq!(all.coverage.selected.planned, 2);
        assert_eq!(all.coverage.changed.planned, 2);
        assert_eq!(all.coverage.skipped_unlinked.planned, 1);
        assert_eq!(
            all.changes
                .iter()
                .map(|change| change.reference.as_str())
                .collect::<Vec<_>>(),
            vec!["SW1", "SW2"]
        );

        let intersection = plan_updates(
            &board,
            &items,
            &net_codes,
            &BTreeSet::new(),
            &parse_filters(&serde_json::json!({
                "references": ["SW2"],
                "library_ids": ["Other:Socket"]
            }))
            .unwrap(),
        );
        assert_eq!(intersection.status, PlanStatus::Noop);
        assert_eq!(intersection.coverage.selected.planned, 0);
    }

    #[test]
    fn planner_conflicts_on_missing_or_duplicate_selected_references() {
        let (_temp, board, mut items) = plan_fixture();
        let net_codes = BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]);
        let missing = plan_updates(
            &board,
            &items,
            &net_codes,
            &BTreeSet::new(),
            &parse_filters(&serde_json::json!({ "references": ["SW404"] })).unwrap(),
        );
        assert_eq!(missing.status, PlanStatus::Conflict);
        assert!(missing
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "reference_not_found"));

        items.push(plan_item("SW1", Some("Test:Socket"), "kiid-duplicate"));
        let duplicate = plan_updates(
            &board,
            &items,
            &net_codes,
            &BTreeSet::new(),
            &parse_filters(&serde_json::json!({ "references": ["SW1"] })).unwrap(),
        );
        assert_eq!(duplicate.status, PlanStatus::Conflict);
        assert!(duplicate
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "duplicate_reference"));
        assert!(duplicate.prepared_items.is_empty());
    }

    #[test]
    fn explicitly_selected_unlinked_footprint_is_a_conflict() {
        let (_temp, board, items) = plan_fixture();
        let plan = plan_updates(
            &board,
            &items,
            &BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]),
            &BTreeSet::new(),
            &parse_filters(&serde_json::json!({ "references": ["TP1"] })).unwrap(),
        );

        assert_eq!(plan.status, PlanStatus::Conflict);
        assert!(plan
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "unlinked_footprint"));
        assert!(plan.prepared_items.is_empty());
    }

    #[test]
    fn planner_returns_noop_after_the_prepared_update_is_applied() {
        let (_temp, board, items) = plan_fixture();
        let net_codes = BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]);
        let first = plan_updates(
            &board,
            &[items[2].clone()],
            &net_codes,
            &BTreeSet::new(),
            &UpdateFilters::default(),
        );
        assert_eq!(first.status, PlanStatus::Ready);

        let second = plan_updates(
            &board,
            &first.prepared_items,
            &net_codes,
            &BTreeSet::new(),
            &UpdateFilters::default(),
        );

        assert_eq!(second.status, PlanStatus::Noop);
        assert_eq!(second.coverage.selected.planned, 1);
        assert_eq!(second.coverage.changed.planned, 0);
        assert_eq!(second.coverage.unchanged.planned, 1);
        assert!(second.prepared_items.is_empty());
    }

    #[test]
    fn datasheet_only_library_change_is_reported_as_metadata() {
        let base = parse_library_footprint("Test:Socket", LIBRARY_FOOTPRINT).unwrap();
        let current = current_instance(kiapi::board::types::BoardLayer::BlFCu);
        let net_codes = BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]);
        let routed = BTreeSet::new();
        let applied = build_updated_instance(&current, &base, &net_codes, &routed).unwrap();
        let applied =
            kiapi::board::types::FootprintInstance::decode(applied.item.value.as_slice()).unwrap();
        let changed_source =
            LIBRARY_FOOTPRINT.replace("new-datasheet.pdf", "replacement-datasheet.pdf");
        let changed = parse_library_footprint("Test:Socket", &changed_source).unwrap();

        let prepared = build_updated_instance(&applied, &changed, &net_codes, &routed).unwrap();

        assert_eq!(
            prepared.changed_domains,
            BTreeSet::from([ChangedDomain::Metadata])
        );
    }

    #[test]
    fn plan_revision_tracks_selected_board_library_and_filter_inputs_only() {
        let (_temp, board, mut items) = plan_fixture();
        let net_codes = BTreeMap::from([("ROW1".to_string(), 11), ("COL1".to_string(), 12)]);
        let filters = parse_filters(&serde_json::json!({ "references": ["SW1"] })).unwrap();
        let first = plan_updates(&board, &items, &net_codes, &BTreeSet::new(), &filters);
        let identical = plan_updates(&board, &items, &net_codes, &BTreeSet::new(), &filters);
        assert_eq!(first.plan_revision, identical.plan_revision);

        items[0] = plan_item("SW2", Some("Test:Socket"), "unselected-changed");
        let unrelated = plan_updates(&board, &items, &net_codes, &BTreeSet::new(), &filters);
        assert_eq!(first.plan_revision, unrelated.plan_revision);

        items[2] = plan_item("SW1", Some("Test:Socket"), "selected-changed");
        let board_changed = plan_updates(&board, &items, &net_codes, &BTreeSet::new(), &filters);
        assert_ne!(first.plan_revision, board_changed.plan_revision);

        std::fs::write(
            board.parent().unwrap().join("Test.pretty/Socket.kicad_mod"),
            LIBRARY_FOOTPRINT.replace("updated description", "library changed"),
        )
        .unwrap();
        let library_changed = plan_updates(&board, &items, &net_codes, &BTreeSet::new(), &filters);
        assert_ne!(board_changed.plan_revision, library_changed.plan_revision);

        let different_filter = plan_updates(
            &board,
            &items,
            &net_codes,
            &BTreeSet::new(),
            &parse_filters(&serde_json::json!({ "references": ["SW2"] })).unwrap(),
        );
        assert_ne!(
            library_changed.plan_revision,
            different_filter.plan_revision
        );
    }

    fn test_context(ipc_address: &str) -> crate::tools::ToolContext {
        crate::tools::ToolContext::new(
            crate::tools::ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: ipc_address.to_string(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        )
    }

    fn result_json(result: &crate::mcp::protocol::CallToolResult) -> serde_json::Value {
        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
            other => panic!("expected text result, got {other:?}"),
        };
        serde_json::from_str(text).expect("tool result must be JSON")
    }

    #[test]
    fn tool_schema_defaults_to_dry_run_and_requires_only_the_board() {
        let definition = tool();

        assert_eq!(definition.name, "update_footprints_from_library");
        assert_eq!(
            definition.input_schema["required"],
            serde_json::json!(["board"])
        );
        assert_eq!(
            definition.input_schema["properties"]["dry_run"]["default"],
            true
        );
        assert_eq!(
            definition.input_schema["properties"]["allow_closed_board_file"]["default"],
            false
        );
        assert_eq!(
            definition.input_schema["properties"]["references"]["items"]["type"],
            "string"
        );
        assert_eq!(
            definition.input_schema["properties"]["library_ids"]["items"]["type"],
            "string"
        );
    }

    #[test]
    fn live_refusal_preserves_uncertain_and_recovered_outcomes_without_preflight_relabeling() {
        let uncertain = refused_live_update_result(crate::tools::ipc_uncertain_result(
            "update may have reached KiCad",
        ));
        assert_eq!(
            crate::mcp::error::extract_error_kind(&uncertain).as_deref(),
            Some("ipc_outcome_unknown")
        );
        assert!(uncertain.is_error);

        let recovered = refused_live_update_result(crate::tools::ipc_recovered_result(
            "late update was dropped",
        ));
        assert_eq!(
            crate::mcp::error::extract_error_kind(&recovered).as_deref(),
            Some("ipc_batch_recovered")
        );
        assert!(recovered.is_error);

        let ordinary = refused_live_update_result(CallToolResult::error("preflight rejected"));
        assert_eq!(result_json(&ordinary)["status"], "conflict");
        assert!(!ordinary.is_error);
    }

    #[tokio::test]
    async fn apply_without_a_reviewed_revision_is_rejected_before_ipc() {
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("board.kicad_pcb");
        std::fs::write(&board, "(kicad_pcb (version 20240108))").unwrap();

        let result = handle_update_footprints_from_library(
            &serde_json::json!({ "board": board, "dry_run": false }),
            &test_context(""),
        )
        .await
        .unwrap();

        assert!(result.is_error);
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("invalid_argument")
        );
        let text = match result.content.first().unwrap() {
            crate::mcp::protocol::ToolContent::Text { text } => text,
            _ => unreachable!(),
        };
        assert!(text.contains("expected_plan_revision"));
    }

    #[tokio::test]
    async fn unreachable_ipc_returns_a_non_mutating_conflict_response() {
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("board.kicad_pcb");
        let before = "(kicad_pcb (version 20240108))";
        std::fs::write(&board, before).unwrap();

        let result = handle_update_footprints_from_library(
            &serde_json::json!({ "board": board }),
            &test_context(""),
        )
        .await
        .unwrap();

        assert!(!result.is_error);
        let response = result_json(&result);
        assert_eq!(response["status"], "conflict");
        assert_eq!(response["coverage"]["transport"], "live_kicad_ipc");
        assert!(response["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("saved-file path was not enabled"));
        assert_eq!(std::fs::read_to_string(board).unwrap(), before);
    }

    #[tokio::test]
    async fn explicit_closed_board_mode_plans_rejects_stale_and_applies_with_readback() {
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("board.kicad_pcb");
        let board_source = format!("(kicad_pcb\n\t(version 20240108)\n\t{}\n)\n", CLOSED_PLACED);
        std::fs::write(&board, &board_source).unwrap();
        let library_dir = temp.path().join("Test.pretty");
        std::fs::create_dir(&library_dir).unwrap();
        std::fs::write(library_dir.join("LED_D5.0mm.kicad_mod"), CLOSED_LIBRARY).unwrap();
        std::fs::write(
            temp.path().join("fp-lib-table"),
            "(fp_lib_table (lib (name \"Test\") (type \"KiCad\") (uri \"${KIPRJMOD}/Test.pretty\") (options \"\") (descr \"\")))",
        )
        .unwrap();
        let context = test_context("");
        let args = serde_json::json!({
            "board": board,
            "references": ["D1"],
            "allow_closed_board_file": true
        });
        let dry_run = handle_update_footprints_from_library(&args, &context)
            .await
            .unwrap();
        let dry_run = result_json(&dry_run);
        assert_eq!(dry_run["status"], "ready");
        assert_eq!(dry_run["coverage"]["transport"], "closed_board_file");
        assert_eq!(dry_run["target"]["source"], "saved_file");
        let revision = dry_run["plan_revision"].as_str().unwrap().to_string();

        let board_b = temp.path().join("board-b.kicad_pcb");
        std::fs::write(&board_b, &board_source).unwrap();
        let wrong_target = handle_update_footprints_from_library(
            &serde_json::json!({
                "board": board_b,
                "references": ["D1"],
                "allow_closed_board_file": true,
                "dry_run": false,
                "expected_plan_revision": revision.clone()
            }),
            &context,
        )
        .await
        .unwrap();
        assert_eq!(
            result_json(&wrong_target)["diagnostics"][0]["code"],
            "stale_plan_revision"
        );
        assert_eq!(std::fs::read_to_string(&board_b).unwrap(), board_source);

        let stale = handle_update_footprints_from_library(
            &serde_json::json!({
                "board": board,
                "references": ["D1"],
                "allow_closed_board_file": true,
                "dry_run": false,
                "expected_plan_revision": "stale"
            }),
            &context,
        )
        .await
        .unwrap();
        assert_eq!(
            result_json(&stale)["diagnostics"][0]["code"],
            "stale_plan_revision"
        );
        assert_eq!(std::fs::read_to_string(&board).unwrap(), board_source);

        let applied = handle_update_footprints_from_library(
            &serde_json::json!({
                "board": board,
                "references": ["D1"],
                "allow_closed_board_file": true,
                "dry_run": false,
                "expected_plan_revision": revision
            }),
            &context,
        )
        .await
        .unwrap();
        let applied = result_json(&applied);
        assert_eq!(applied["status"], "applied");
        assert_eq!(applied["coverage"]["changed"]["applied"], 1);
        let written = std::fs::read_to_string(&board).unwrap();
        assert!(written.contains("(point"));
        assert!(written.contains("(net \"GND\")"));
        assert!(written.contains("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"));

        if std::process::Command::new("kicad-cli")
            .arg("version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            let report = temp.path().join("closed-refresh-drc.json");
            let output = std::process::Command::new("kicad-cli")
                .args(["pcb", "drc", "--format", "json", "--output"])
                .arg(&report)
                .arg(&board)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "kicad-cli rejected the engine-written board: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(report.is_file());
        }
    }

    #[tokio::test]
    async fn closed_board_preflight_failure_reports_closed_source_and_unchanged_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("invalid.kicad_pcb");
        let before = "(not_a_board)\n";
        std::fs::write(&board, before).unwrap();
        let result = handle_update_footprints_from_library(
            &serde_json::json!({
                "board": board,
                "allow_closed_board_file": true
            }),
            &test_context(""),
        )
        .await
        .unwrap();
        let response = result_json(&result);
        assert_eq!(response["status"], "conflict");
        assert_eq!(response["coverage"]["transport"], "closed_board_file");
        assert_eq!(
            response["coverage"]["atomicity"],
            "compare_and_swap_atomic_replace_with_readback_and_rollback"
        );
        assert_eq!(response["recovery"], "unchanged");
        assert_eq!(
            response["diagnostics"][0]["code"],
            "closed_board_preflight_conflict"
        );
        assert_eq!(std::fs::read_to_string(board).unwrap(), before);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn closed_board_symlink_target_is_refused_without_replacing_the_link_or_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real.kicad_pcb");
        let alias = temp.path().join("alias.kicad_pcb");
        let before = "(kicad_pcb (version 20240108))\n";
        std::fs::write(&real, before).unwrap();
        symlink(&real, &alias).unwrap();

        let result = handle_update_footprints_from_library(
            &serde_json::json!({
                "board": alias,
                "allow_closed_board_file": true
            }),
            &test_context(""),
        )
        .await
        .unwrap();
        let response = result_json(&result);
        assert_eq!(response["status"], "conflict");
        assert!(response["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("is a symlink"));
        assert!(std::fs::symlink_metadata(&alias)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(real).unwrap(), before);
    }

    #[derive(Default)]
    struct MutationCapture {
        begin_count: usize,
        update_batches: Vec<usize>,
        commit_actions: Vec<kiapi::common::commands::CommitAction>,
    }

    struct PlannerMock {
        url: String,
        capture: std::sync::Arc<std::sync::Mutex<MutationCapture>>,
        _thread: std::thread::JoinHandle<()>,
    }

    fn api_ok() -> kiapi::common::ApiResponse {
        kiapi::common::ApiResponse {
            status: Some(kiapi::common::ApiResponseStatus {
                status: kiapi::common::ApiStatusCode::AsOk as i32,
                error_message: String::new(),
            }),
            header: None,
            message: None,
        }
    }

    fn api_reply(message: prost_types::Any) -> kiapi::common::ApiResponse {
        kiapi::common::ApiResponse {
            message: Some(message),
            ..api_ok()
        }
    }

    fn planner_live_source(footprint: &prost_types::Any) -> String {
        let footprint = kiapi::board::types::FootprintInstance::decode(footprint.value.as_slice())
            .expect("planner fixture footprint");
        let kiid = footprint.id.as_ref().unwrap().value.as_str();
        let reference = field_text(&footprint.reference_field);
        let library_id = footprint
            .definition
            .as_ref()
            .and_then(|definition| definition.id.as_ref())
            .map(|id| format!("{}:{}", id.library_nickname, id.entry_name))
            .unwrap();
        let position = footprint.position.as_ref().unwrap();
        let rotation = footprint.orientation.as_ref().unwrap().value_degrees;
        format!(
            "(kicad_pcb (version 20240108) (footprint \"{library_id}\" (layer \"F.Cu\") (uuid \"{kiid}\") (at {} {} {rotation}) (property \"Reference\" \"{reference}\" (at 0 0 0) (layer \"F.SilkS\") (effects (font (size 1 1))))))",
            builders::nm_to_mm(position.x_nm),
            builders::nm_to_mm(position.y_nm),
        )
    }

    fn spawn_planner_mock(
        board: &Path,
        footprint: prost_types::Any,
        fail_update: bool,
    ) -> PlannerMock {
        use nng::options::Options;

        static NEXT_MOCK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let url = format!(
            "inproc://footprint-update-{}",
            NEXT_MOCK.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let socket = nng::Socket::new(nng::Protocol::Rep0).unwrap();
        socket
            .set_opt::<nng::options::RecvTimeout>(Some(std::time::Duration::from_secs(20)))
            .unwrap();
        socket.listen(&url).unwrap();

        let board_name = board.to_string_lossy().to_string();
        let document = kiapi::common::types::DocumentSpecifier {
            r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
            identifier: Some(
                kiapi::common::types::document_specifier::Identifier::BoardFilename(
                    board_name.clone(),
                ),
            ),
            project: None,
        };
        let live_source = planner_live_source(&footprint);
        let capture = std::sync::Arc::new(std::sync::Mutex::new(MutationCapture::default()));
        let captured = capture.clone();
        let thread = std::thread::spawn(move || {
            while let Ok(message) = socket.recv() {
                let request = kiapi::common::ApiRequest::decode(message.as_slice()).unwrap();
                let message = request.message.expect("request message");
                let response = if message.type_url.ends_with("GetOpenDocuments") {
                    api_reply(builders::pack_any(
                        &kiapi::common::commands::GetOpenDocumentsResponse {
                            documents: vec![document.clone()],
                        },
                        "kiapi.common.commands.GetOpenDocumentsResponse",
                    ))
                } else if message.type_url.ends_with("GetItems") {
                    let command =
                        kiapi::common::commands::GetItems::decode(message.value.as_slice())
                            .unwrap();
                    let items = if command.types
                        == vec![kiapi::common::types::KiCadObjectType::KotPcbFootprint as i32]
                    {
                        vec![footprint.clone()]
                    } else {
                        Vec::new()
                    };
                    api_reply(builders::pack_any(
                        &kiapi::common::commands::GetItemsResponse {
                            header: None,
                            status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                            items,
                        },
                        "kiapi.common.commands.GetItemsResponse",
                    ))
                } else if message.type_url.ends_with("GetNets") {
                    api_reply(builders::pack_any(
                        &kiapi::board::commands::NetsResponse {
                            nets: vec![builders::net("ROW1", 11), builders::net("COL1", 12)],
                        },
                        "kiapi.board.commands.NetsResponse",
                    ))
                } else if message.type_url.ends_with("SaveDocumentToString") {
                    let command = kiapi::common::commands::SaveDocumentToString::decode(
                        message.value.as_slice(),
                    )
                    .unwrap();
                    assert_eq!(command.document, Some(document.clone()));
                    api_reply(builders::pack_any(
                        &kiapi::common::commands::SavedDocumentResponse {
                            document: Some(document.clone()),
                            contents: live_source.clone(),
                        },
                        "kiapi.common.commands.SavedDocumentResponse",
                    ))
                } else if message.type_url.ends_with("BeginCommit") {
                    captured.lock().unwrap().begin_count += 1;
                    api_reply(builders::pack_any(
                        &kiapi::common::commands::BeginCommitResponse {
                            id: Some(kiapi::common::types::Kiid {
                                value: "commit-1".to_string(),
                            }),
                        },
                        "kiapi.common.commands.BeginCommitResponse",
                    ))
                } else if message.type_url.ends_with("UpdateItems") {
                    let update =
                        kiapi::common::commands::UpdateItems::decode(message.value.as_slice())
                            .unwrap();
                    captured
                        .lock()
                        .unwrap()
                        .update_batches
                        .push(update.items.len());
                    let updated_items = update
                        .items
                        .into_iter()
                        .map(|item| kiapi::common::commands::ItemUpdateResult {
                            status: Some(kiapi::common::commands::ItemStatus {
                                code: if fail_update {
                                    kiapi::common::commands::ItemStatusCode::IscInvalidData as i32
                                } else {
                                    kiapi::common::commands::ItemStatusCode::IscOk as i32
                                },
                                error_message: if fail_update {
                                    "mock update failure".to_string()
                                } else {
                                    String::new()
                                },
                            }),
                            item: Some(item),
                        })
                        .collect();
                    api_reply(builders::pack_any(
                        &kiapi::common::commands::UpdateItemsResponse {
                            header: None,
                            status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                            updated_items,
                        },
                        "kiapi.common.commands.UpdateItemsResponse",
                    ))
                } else if message.type_url.ends_with("EndCommit") {
                    let end = kiapi::common::commands::EndCommit::decode(message.value.as_slice())
                        .unwrap();
                    captured.lock().unwrap().commit_actions.push(end.action());
                    api_reply(builders::pack_any(
                        &kiapi::common::commands::EndCommitResponse {},
                        "kiapi.common.commands.EndCommitResponse",
                    ))
                } else {
                    panic!("unexpected mock request {}", message.type_url);
                };
                let response = nng::Message::from(response.encode_to_vec().as_slice());
                if socket.send(response).is_err() {
                    break;
                }
            }
        });
        PlannerMock {
            url,
            capture,
            _thread: thread,
        }
    }

    #[tokio::test]
    async fn dry_run_and_stale_revision_never_open_a_commit_or_send_updates() {
        let (_temp, board, items) = plan_fixture();
        let mock = spawn_planner_mock(&board, items[2].clone(), false);
        let context = test_context(&mock.url);

        let dry_run =
            handle_update_footprints_from_library(&serde_json::json!({ "board": board }), &context)
                .await
                .unwrap();
        assert_eq!(result_json(&dry_run)["status"], "ready");

        let stale = handle_update_footprints_from_library(
            &serde_json::json!({
                "board": board,
                "dry_run": false,
                "expected_plan_revision": "stale"
            }),
            &context,
        )
        .await
        .unwrap();
        let stale = result_json(&stale);
        assert_eq!(stale["status"], "conflict");
        assert_eq!(stale["diagnostics"][0]["code"], "stale_plan_revision");

        let capture = mock.capture.lock().unwrap();
        assert_eq!(capture.begin_count, 0);
        assert!(capture.update_batches.is_empty());
        assert!(capture.commit_actions.is_empty());
    }

    #[tokio::test]
    async fn apply_sends_one_batch_in_one_commit() {
        let (_temp, board, items) = plan_fixture();
        let mock = spawn_planner_mock(&board, items[2].clone(), false);
        let context = test_context(&mock.url);
        let dry_run =
            handle_update_footprints_from_library(&serde_json::json!({ "board": board }), &context)
                .await
                .unwrap();
        let revision = result_json(&dry_run)["plan_revision"]
            .as_str()
            .unwrap()
            .to_string();

        let applied = handle_update_footprints_from_library(
            &serde_json::json!({
                "board": board,
                "dry_run": false,
                "expected_plan_revision": revision
            }),
            &context,
        )
        .await
        .unwrap();

        let applied = result_json(&applied);
        assert_eq!(applied["status"], "applied");
        assert_eq!(applied["coverage"]["changed"]["applied"], 1);
        let capture = mock.capture.lock().unwrap();
        assert_eq!(capture.begin_count, 1);
        assert_eq!(capture.update_batches, vec![1]);
        assert_eq!(
            capture.commit_actions,
            vec![kiapi::common::commands::CommitAction::CmaCommit]
        );
    }

    #[tokio::test]
    async fn failed_update_drops_the_single_commit() {
        let (_temp, board, items) = plan_fixture();
        let mock = spawn_planner_mock(&board, items[2].clone(), true);
        let context = test_context(&mock.url);
        let dry_run =
            handle_update_footprints_from_library(&serde_json::json!({ "board": board }), &context)
                .await
                .unwrap();
        let revision = result_json(&dry_run)["plan_revision"]
            .as_str()
            .unwrap()
            .to_string();

        let failed = handle_update_footprints_from_library(
            &serde_json::json!({
                "board": board,
                "dry_run": false,
                "expected_plan_revision": revision
            }),
            &context,
        )
        .await
        .unwrap();

        assert_eq!(result_json(&failed)["status"], "conflict");
        let capture = mock.capture.lock().unwrap();
        assert_eq!(capture.begin_count, 1);
        assert_eq!(capture.update_batches, vec![1]);
        assert_eq!(
            capture.commit_actions,
            vec![kiapi::common::commands::CommitAction::CmaDrop]
        );
    }

    const CLOSED_LIBRARY: &str = r#"(footprint "LED_D5.0mm"
	(version 20260206)
	(generator "kicad-footprint-generator")
	(layer "F.Cu")
	(property "Reference" "REF**" (at 1.27 -3.96 0) (layer "F.SilkS") (effects (font (size 1 1) (thickness 0.15))))
	(property "Value" "LED_D5.0mm" (at 1.27 3.96 0) (layer "F.Fab") (effects (font (size 1 1) (thickness 0.15))))
	(property "KiLib_Generator" "LED/THT" (at 0 0 0) (layer "F.SilkS") (hide yes) (effects (font (size 1 1) (thickness 0.15))))
	(attr through_hole)
	(fp_circle (center 1.27 0) (end 3.77 0) (stroke (width 0.1) (type solid)) (fill no) (layer "F.Fab"))
	(fp_text user "${REFERENCE}" (at 1.27 0 0) (layer "F.Fab") (effects (font (size 0.8 0.8) (thickness 0.12))))
	(pad "1" thru_hole rect (at 0 0) (size 1.8 1.8) (drill 0.9) (layers "*.Cu" "*.Mask") (remove_unused_layers no))
	(pad "2" thru_hole circle (at 2.54 0) (size 1.8 1.8) (drill 0.9) (layers "*.Cu" "*.Mask") (remove_unused_layers no))
	(point
		(at 1.27 0)
		(size 2)
		(layer "F.Fab")
	)
	(embedded_fonts no)
)"#;

    const CLOSED_PLACED: &str = r#"(footprint "Test:LED_D5.0mm"
	(layer "F.Cu")
	(uuid "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
	(at 132 108 180)
	(locked yes)
	(property "Reference" "D1" (at 1.27 -3.96 180) (layer "F.SilkS") (uuid "11111111-1111-4111-8111-111111111111") (effects (font (size 1 1))))
	(property "Value" "LED" (at 1.27 3.96 180) (layer "F.Fab") (uuid "22222222-2222-4222-8222-222222222222") (effects (font (size 1 1))))
	(property "InstanceNote" "keep" (at 0 0 180) (layer "F.Fab") (uuid "33333333-3333-4333-8333-333333333333") (effects (font (size 1 1))))
	(path "/90e64816-9539-4b62-9d79-d001045cf4b3")
	(units (unit (name "A") (pins "1" "2")))
	(attr through_hole dnp)
	(fp_circle (center 1.27 0) (end 3.77 0) (stroke (width 0.1) (type solid)) (fill no) (layer "F.Fab") (uuid "44444444-4444-4444-8444-444444444444"))
	(fp_text user "${REFERENCE}" (at 1.27 0 180) (unlocked yes) (layer "F.Fab") (uuid "77777777-7777-4777-8777-777777777777") (effects (font (size 0.8 0.8) (thickness 0.12))))
	(pad "1" thru_hole rect (at 0 0 180) (size 1.8 1.8) (drill 0.9) (layers "*.Cu" "*.Mask") (remove_unused_layers no) (locked yes) (zone_layer_connections) (net "GND") (pinfunction "K") (pintype "passive") (uuid "55555555-5555-4555-8555-555555555555"))
	(pad "2" thru_hole circle (at 2.54 0 180) (size 1.8 1.8) (drill 0.9) (layers "*.Cu" "*.Mask") (remove_unused_layers no) (net "Net-(D1-A)") (uuid "66666666-6666-4666-8666-666666666666"))
	(embedded_fonts no)
)"#;

    #[test]
    fn closed_board_refresh_preserves_instance_state_and_complete_point_bytes() {
        let path = std::path::Path::new("/tmp/Test.pretty/LED_D5.0mm.kicad_mod");
        let prepared =
            prepare_closed_board_footprint(CLOSED_PLACED, "Test:LED_D5.0mm", CLOSED_LIBRARY, path)
                .unwrap();

        let replacement = &prepared.replacement;
        assert!(replacement.contains("(uuid \"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa\")"));
        assert!(replacement.contains("(at 132 108 180)"));
        assert!(replacement.contains("(locked yes)"));
        assert!(replacement.contains("(path \"/90e64816-9539-4b62-9d79-d001045cf4b3\")"));
        assert!(replacement.contains("(units (unit (name \"A\") (pins \"1\" \"2\")))"));
        assert!(replacement.contains("(property \"InstanceNote\" \"keep\""));
        assert!(replacement.contains("(net \"GND\")"));
        assert!(replacement.contains("(uuid \"55555555-5555-4555-8555-555555555555\")"));
        assert!(replacement.contains("(attr through_hole dnp)"));

        let library_points = children_named(
            &raw_direct_children(CLOSED_LIBRARY, "footprint").unwrap(),
            "point",
        );
        let written_points = children_named(
            &raw_direct_children(replacement, "footprint").unwrap(),
            "point",
        );
        assert_eq!(written_points, library_points, "point bytes must be exact");
        assert_eq!(written_points.len(), 1);
        closed_footprint_invariant(
            replacement,
            &raw_direct_children(CLOSED_LIBRARY, "footprint").unwrap(),
        )
        .unwrap();

        let old_children = raw_direct_children(CLOSED_PLACED, "footprint").unwrap();
        let new_children = raw_direct_children(replacement, "footprint").unwrap();
        let old_text = children_named(&old_children, "fp_text");
        let new_text = children_named(&new_children, "fp_text");
        assert_eq!(new_text, old_text, "fp_text user must be byte-identical");
        let named_property = |children: &[RawChild], name: &str| {
            children_named(children, "property")
                .into_iter()
                .find(|block| property_name(block).ok().as_deref() == Some(name))
                .unwrap()
        };
        assert_eq!(
            named_property(&new_children, "InstanceNote"),
            named_property(&old_children, "InstanceNote"),
            "instance-only property must be byte-identical"
        );
        let old_pad = unique_pads(&children_named(&old_children, "pad"), "old")
            .unwrap()
            .remove("1")
            .unwrap();
        let new_pad = unique_pads(&children_named(&new_children, "pad"), "new")
            .unwrap()
            .remove("1")
            .unwrap();
        for tag in [
            "locked",
            "zone_layer_connections",
            "net",
            "pinfunction",
            "pintype",
            "uuid",
        ] {
            assert_eq!(
                direct_child_block(&new_pad, "pad", tag).unwrap(),
                direct_child_block(&old_pad, "pad", tag).unwrap(),
                "pad child '{tag}' must be byte-identical"
            );
        }
    }

    #[test]
    fn installed_kicad_led_point_survives_the_closed_board_engine_byte_exact() {
        let path = std::path::Path::new(
            "/opt/kicad/10.0.6/share/kicad/footprints/LED_THT.pretty/LED_D5.0mm.kicad_mod",
        );
        let Ok(source) = std::fs::read_to_string(path) else {
            eprintln!("SKIP: {} is not installed", path.display());
            return;
        };
        let mut placed = super::super::pcb_components::board_footprint_sexp_from_source(
            &source,
            "LED_THT:LED_D5.0mm",
            path,
            132.0,
            108.0,
            180.0,
            "F.Cu",
            Some("D1"),
        )
        .unwrap();
        let point = children_named(&raw_direct_children(&placed, "footprint").unwrap(), "point")
            .into_iter()
            .next()
            .expect("installed KiCad 10.0.6 LED footprint has point");
        let point_index = raw_direct_children(&placed, "footprint")
            .unwrap()
            .iter()
            .filter(|child| child.tag == "point")
            .count();
        assert_eq!(point_index, 1);
        let (start, end) = find_direct_child_blocks(&placed, "footprint")
            .into_iter()
            .find(|(start, end)| {
                konnect_sexp::parse_sexp(&placed[*start..*end])
                    .ok()
                    .is_some_and(|node| node.head() == Some("point"))
            })
            .unwrap();
        placed = apply_edits(placed, vec![SexpEdit::delete(start, end)]);

        let prepared =
            prepare_closed_board_footprint(&placed, "LED_THT:LED_D5.0mm", &source, path).unwrap();
        let restored = children_named(
            &raw_direct_children(&prepared.replacement, "footprint").unwrap(),
            "point",
        );
        assert_eq!(restored, vec![point]);
    }

    #[test]
    fn kicad_reserialized_point_keeps_its_board_uuid_across_refresh() {
        let current = CLOSED_PLACED.replace(
            "\t(embedded_fonts no)",
            "\t(point (at 1.270001 0) (size 2) (layer \"F.Fab\") (uuid \"dddddddd-dddd-4ddd-8ddd-dddddddddddd\"))\n\t(embedded_fonts no)",
        );
        let prepared = prepare_closed_board_footprint(
            &current,
            "Test:LED_D5.0mm",
            CLOSED_LIBRARY,
            std::path::Path::new("/tmp/Test.pretty/LED_D5.0mm.kicad_mod"),
        )
        .unwrap();
        let points = children_named(
            &raw_direct_children(&prepared.replacement, "footprint").unwrap(),
            "point",
        );
        assert_eq!(points.len(), 1);
        assert_eq!(
            direct_child_block(&points[0], "point", "uuid")
                .unwrap()
                .unwrap(),
            "(uuid \"dddddddd-dddd-4ddd-8ddd-dddddddddddd\")"
        );
    }

    #[test]
    fn closed_board_refresh_refuses_back_side_duplicate_pads_and_user_graphics() {
        let path = std::path::Path::new("/tmp/Test.pretty/LED_D5.0mm.kicad_mod");
        let back = CLOSED_PLACED.replacen("(layer \"F.Cu\")", "(layer \"B.Cu\")", 1);
        let error = prepare_closed_board_footprint(&back, "Test:LED_D5.0mm", CLOSED_LIBRARY, path)
            .unwrap_err();
        assert!(format!("{error:#}").contains("supports F.Cu only"));

        let duplicate = CLOSED_LIBRARY.replace(
            "\t(pad \"2\" thru_hole circle",
            "\t(pad \"1\" thru_hole circle",
        );
        let error =
            prepare_closed_board_footprint(CLOSED_PLACED, "Test:LED_D5.0mm", &duplicate, path)
                .unwrap_err();
        assert!(format!("{error:#}").contains("duplicate pad number '1'"));

        let user_graphic = CLOSED_PLACED.replace(
            "\t(pad \"1\"",
            "\t(fp_line (start 0 0) (end 1 1) (stroke (width 0.1) (type solid)) (layer \"F.SilkS\") (uuid \"77777777-7777-4777-8777-777777777777\"))\n\t(pad \"1\"",
        );
        let error =
            prepare_closed_board_footprint(&user_graphic, "Test:LED_D5.0mm", CLOSED_LIBRARY, path)
                .unwrap_err();
        assert!(format!("{error:#}").contains("ownership is ambiguous"));

        let same_count_library = CLOSED_LIBRARY.replace(
            "\t(fp_circle",
            "\t(fp_line (start 10 10) (end 11 11) (stroke (width 0.1) (type solid)) (layer \"F.SilkS\"))\n\t(fp_circle",
        );
        let same_count_user = CLOSED_PLACED.replace(
            "\t(fp_circle",
            "\t(fp_line (start 20 20) (end 21 21) (stroke (width 0.1) (type solid)) (layer \"F.SilkS\") (uuid \"99999999-9999-4999-8999-999999999999\"))\n\t(fp_circle",
        );
        let error = prepare_closed_board_footprint(
            &same_count_user,
            "Test:LED_D5.0mm",
            &same_count_library,
            path,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("ownership is ambiguous"));

        let changed_user_text = CLOSED_LIBRARY.replace("${REFERENCE}", "LIBRARY_REFERENCE");
        let error = prepare_closed_board_footprint(
            CLOSED_PLACED,
            "Test:LED_D5.0mm",
            &changed_user_text,
            path,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("different fp_text user identities"));

        let added_user_text = CLOSED_LIBRARY.replace(
            "\t(pad \"1\"",
            "\t(fp_text user \"NEW_LIBRARY_NOTE\" (at 0 0 0) (layer \"F.Fab\") (effects (font (size 1 1) (thickness 0.15))))\n\t(pad \"1\"",
        );
        let error = prepare_closed_board_footprint(
            CLOSED_PLACED,
            "Test:LED_D5.0mm",
            &added_user_text,
            path,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("different fp_text user identities"));

        let grouped = CLOSED_LIBRARY.replace(
            "\t(embedded_fonts no)",
            "\t(group \"Fab\" (uuid \"88888888-8888-4888-8888-888888888888\") (members \"44444444-4444-4444-8444-444444444444\"))\n\t(embedded_fonts no)",
        );
        let error =
            prepare_closed_board_footprint(CLOSED_PLACED, "Test:LED_D5.0mm", &grouped, path)
                .unwrap_err();
        assert!(format!("{error:#}").contains("cannot yet remap group member UUIDs"));

        let grouped_current = CLOSED_PLACED.replace(
            "\t(embedded_fonts no)",
            "\t(group \"BoardOnly\" (uuid \"abababab-abab-4bab-8bab-abababababab\") (members \"44444444-4444-4444-8444-444444444444\"))\n\t(embedded_fonts no)",
        );
        let error = prepare_closed_board_footprint(
            &grouped_current,
            "Test:LED_D5.0mm",
            CLOSED_LIBRARY,
            path,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("cannot yet remap group member UUIDs"));

        let point = children_named(
            &raw_direct_children(CLOSED_LIBRARY, "footprint").unwrap(),
            "point",
        )[0]
        .clone();
        let duplicate_point = CLOSED_PLACED.replace(
            "\t(embedded_fonts no)",
            &format!("\t{point}\n\t{point}\n\t(embedded_fonts no)"),
        );
        let error = prepare_closed_board_footprint(
            &duplicate_point,
            "Test:LED_D5.0mm",
            CLOSED_LIBRARY,
            path,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("point ownership is ambiguous"));
    }

    #[test]
    fn closed_board_refresh_refuses_removing_a_connected_pad() {
        let path = std::path::Path::new("/tmp/Test.pretty/LED_D5.0mm.kicad_mod");
        let without_pad_two = CLOSED_LIBRARY.replace(
            "\t(pad \"2\" thru_hole circle (at 2.54 0) (size 1.8 1.8) (drill 0.9) (layers \"*.Cu\" \"*.Mask\") (remove_unused_layers no))\n",
            "",
        );
        let error = prepare_closed_board_footprint(
            CLOSED_PLACED,
            "Test:LED_D5.0mm",
            &without_pad_two,
            path,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("removes connected pad 2"));
    }

    #[test]
    fn new_library_child_uuids_are_deterministic_and_instance_unique() {
        let path = std::path::Path::new("/tmp/Test.pretty/LED_D5.0mm.kicad_mod");
        let library = CLOSED_LIBRARY.replace(
            "\t(pad \"1\"",
            "\t(fp_line (start 10 10) (end 11 11) (stroke (width 0.1) (type solid)) (layer \"F.SilkS\") (uuid \"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\"))\n\t(pad \"1\"",
        );
        let first =
            prepare_closed_board_footprint(CLOSED_PLACED, "Test:LED_D5.0mm", &library, path)
                .unwrap();
        let second_current = CLOSED_PLACED
            .replace(
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
            )
            .replace("\"D1\"", "\"D2\"");
        let second =
            prepare_closed_board_footprint(&second_current, "Test:LED_D5.0mm", &library, path)
                .unwrap();
        let new_uuid = |replacement: &str| {
            let line = children_named(
                &raw_direct_children(replacement, "footprint").unwrap(),
                "fp_line",
            )
            .into_iter()
            .next()
            .unwrap();
            direct_child_block(&line, "fp_line", "uuid")
                .unwrap()
                .unwrap()
        };
        let first_uuid = new_uuid(&first.replacement);
        let second_uuid = new_uuid(&second.replacement);
        assert_ne!(first_uuid, second_uuid);
        assert!(!first_uuid.contains("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"));
        assert!(!second_uuid.contains("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"));
    }

    #[test]
    fn post_write_verification_failure_restores_and_reads_back_original_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("rollback.kicad_pcb");
        let original = "(kicad_pcb (version 20240108))\n".to_string();
        let replacement = "(kicad_pcb (version 20240108) (generator pcbnew))\n".to_string();
        std::fs::write(&board, &original).unwrap();
        let plan = ClosedBoardPlan {
            target_path: board.display().to_string(),
            status: PlanStatus::Ready,
            plan_revision: "test".to_string(),
            coverage: UpdateCoverage::default(),
            changes: Vec::new(),
            diagnostics: Vec::new(),
            original: original.clone(),
            replacement,
            invariants: Vec::new(),
        };

        let result = persist_closed_board_plan_with(&board, &plan, |_| {
            anyhow::bail!("injected post-write verifier failure")
        });
        assert!(matches!(result, Err(ClosedPersistFailure::RolledBack(_))));
        assert_eq!(std::fs::read_to_string(board).unwrap(), original);
    }

    #[test]
    fn post_write_external_change_is_preserved_instead_of_rolled_back() {
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("post-write-race.kicad_pcb");
        let original = "(kicad_pcb (version 20240108))\n".to_string();
        let replacement = "(kicad_pcb (version 20240108) (generator konnect))\n".to_string();
        let external = "(kicad_pcb (version 20240108) (generator external))\n".to_string();
        std::fs::write(&board, &original).unwrap();
        let plan = ClosedBoardPlan {
            target_path: board.display().to_string(),
            status: PlanStatus::Ready,
            plan_revision: "test".to_string(),
            coverage: UpdateCoverage::default(),
            changes: Vec::new(),
            diagnostics: Vec::new(),
            original,
            replacement,
            invariants: Vec::new(),
        };

        let result = persist_closed_board_plan_with_hooks(
            &board,
            &plan,
            || std::fs::write(&board, &external).unwrap(),
            |_| Ok(()),
        );
        assert!(matches!(result, Err(ClosedPersistFailure::Uncertain(_))));
        assert_eq!(std::fs::read_to_string(board).unwrap(), external);
    }

    #[test]
    fn pre_rollback_external_change_is_preserved_by_compare_and_swap() {
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("pre-rollback-race.kicad_pcb");
        let original = "(kicad_pcb (version 20240108))\n".to_string();
        let replacement = "(kicad_pcb (version 20240108) (generator konnect))\n".to_string();
        let external = "(kicad_pcb (version 20240108) (generator external))\n".to_string();
        std::fs::write(&board, &original).unwrap();
        let plan = ClosedBoardPlan {
            target_path: board.display().to_string(),
            status: PlanStatus::Ready,
            plan_revision: "test".to_string(),
            coverage: UpdateCoverage::default(),
            changes: Vec::new(),
            diagnostics: Vec::new(),
            original,
            replacement,
            invariants: Vec::new(),
        };

        let result = persist_closed_board_plan_with(&board, &plan, |_| {
            std::fs::write(&board, &external).unwrap();
            anyhow::bail!("injected verifier failure after an external write")
        });
        assert!(matches!(result, Err(ClosedPersistFailure::Uncertain(_))));
        assert_eq!(std::fs::read_to_string(board).unwrap(), external);
    }

    #[test]
    fn compare_and_swap_race_never_overwrites_external_bytes_and_reports_uncertain_phase() {
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("stale.kicad_pcb");
        let original = "(kicad_pcb (version 20240108))\n".to_string();
        let external = "(kicad_pcb (version 20240108) (generator external))\n".to_string();
        std::fs::write(&board, &external).unwrap();
        let plan = ClosedBoardPlan {
            target_path: board.display().to_string(),
            status: PlanStatus::Ready,
            plan_revision: "test".to_string(),
            coverage: UpdateCoverage::default(),
            changes: Vec::new(),
            diagnostics: Vec::new(),
            original,
            replacement: "(kicad_pcb (version 20240108) (generator konnect))\n".to_string(),
            invariants: Vec::new(),
        };

        let result = persist_closed_board_plan(&board, &plan);
        assert!(matches!(result, Err(ClosedPersistFailure::Uncertain(_))));
        assert_eq!(std::fs::read_to_string(board).unwrap(), external);
    }
}
