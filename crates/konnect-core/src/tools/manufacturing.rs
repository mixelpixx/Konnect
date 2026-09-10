//! `manufacturing` toolset — Design-to-fab pipeline: export packages, cost estimation, validation.
//!
//! Orchestrates gerber export, BOM generation, and pick-and-place file creation
//! into a single manufacturing-ready package for a specific fab house.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, ToolContext, ToolDef};
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tracing::{debug, error, info};

use super::{cli, pcb_export};

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "export_manufacturing_package",
            "Generate ALL files needed for PCB fabrication and assembly in one call: \
             Gerbers, drill files, BOM (fab-house format), and pick-and-place positions. \
             Targets a specific fab house (JLCPCB, PCBWay, etc.).",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file (for BOM generation)" },
                    "output_dir": { "type": "string", "description": "Directory to write all output files" },
                    "fab_house": {
                        "type": "string",
                        "description": "Target manufacturer: 'jlcpcb' (default), 'pcbway', 'oshpark', 'generic'",
                        "default": "jlcpcb"
                    },
                    "include_assembly": {
                        "type": "boolean",
                        "description": "Include BOM + pick-and-place files for SMT assembly",
                        "default": true
                    },
                    "bom_fields": {
                        "type": "string",
                        "description": "Ordered, comma-separated BOM columns, e.g. 'Reference,Value,Footprint,MPN,${QUANTITY}'. Any schematic field name works — this is how MPN/LCSC columns reach the fab. Omit for KiCAD's default Reference,Value,Footprint,QUANTITY,DNP."
                    },
                    "bom_labels": {
                        "type": "string",
                        "description": "Ordered, comma-separated BOM column headings matching 'bom_fields'. Omit to label each column with its field name."
                    },
                    "bom_group_by": {
                        "type": "string",
                        "description": "Comma-separated fields whose matching references collapse into one BOM row, e.g. 'Value,Footprint'."
                    },
                    "gerber_layers": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Exact Gerber layers. Omit or pass [] to auto-select enabled copper, F/B.Mask, F/B.SilkS, and Edge.Cuts while excluding documentation layers."
                    },
                    "position_side": {
                        "type": "string",
                        "enum": ["front", "back", "both"],
                        "description": "Board side(s) in the assembly position file.",
                        "default": "both"
                    },
                    "position_units": {
                        "type": "string",
                        "enum": ["mm", "in"],
                        "description": "Coordinate units in the assembly position file. JLCPCB assembly requires 'mm'.",
                        "default": "mm"
                    },
                    "jlcpcb_cpl_corrections_path": {
                        "type": "string",
                        "description": "Optional path to a versioned JLCPCB CPL correction-policy JSON file. Project footprint rules override built-in rules, and exact designator overrides take highest precedence. Used only when fab_house is 'jlcpcb'."
                    }
                },
                "required": ["board", "output_dir"]
            }),
            |args, ctx| async move { handle_export_manufacturing_package(args, ctx).await }
        ),
        tool!(
            "validate_for_manufacturing",
            "Pre-flight check before ordering: verifies the design is ready for the target \
             fab house. Runs KiCad's DRC and checks board outline, design rules, footprints, \
             and routing evidence. Returns NOT READY — never READY — if \
             DRC reports errors or could not be run, so a READY verdict always rests on \
             evidence rather than on an absence of findings.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "fab_house": {
                        "type": "string",
                        "description": "Target manufacturer: 'jlcpcb', 'pcbway', 'oshpark'",
                        "default": "jlcpcb"
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_validate_for_manufacturing(args, ctx).await }
        ),
        tool!(
            "estimate_cost",
            "Estimate the total manufacturing cost for PCB fabrication and assembly at a given fab house. \
             Counts components from board footprints and returns an itemized rough estimate: \
             PCB, components, assembly, and total.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "fab_house": {
                        "type": "string",
                        "description": "'jlcpcb' (default), 'pcbway'",
                        "default": "jlcpcb"
                    },
                    "quantity": {
                        "type": "integer",
                        "description": "Number of boards to manufacture",
                        "default": 5
                    },
                    "layers": {
                        "type": "integer",
                        "description": "Copper layer count to quote at (2, 4, 6). Defaults to the count the board file declares; when given and different, the response reports both under board.copper_layers / board.board_copper_layers and warns."
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_estimate_cost(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_export_manufacturing_package(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let output_dir = get_path(args, "output_dir")?;
    let fab_house = args["fab_house"].as_str().unwrap_or("jlcpcb");
    let include_assembly = args["include_assembly"].as_bool().unwrap_or(true);
    let is_jlcpcb = fab_house == "jlcpcb";
    let schematic = args["schematic"].as_str().map(PathBuf::from);
    let requested_gerber_layers = match pcb_export::optional_string_array(args, "gerber_layers") {
        Ok(layers) => layers,
        Err(error) => return Ok(error),
    };
    let gerber_layers = if requested_gerber_layers.is_empty() {
        let board_source = tokio::fs::read_to_string(&board).await?;
        pcb_export::standard_gerber_layers(&board_source)?
    } else {
        requested_gerber_layers
    };
    let position_side = args["position_side"].as_str().unwrap_or("both");
    let position_units = args["position_units"].as_str().unwrap_or("mm");
    let jlcpcb_cpl_corrections_path = if args.get("jlcpcb_cpl_corrections_path").is_some() {
        Some(get_path(args, "jlcpcb_cpl_corrections_path")?)
    } else {
        None
    };
    if let Err((field, reason)) =
        pcb_export::validate_position_values("csv", position_side, position_units)
    {
        let public_field = match field {
            "side" => "position_side",
            "units" => "position_units",
            other => other,
        };
        return Ok(invalid_manufacturing_argument(public_field, reason));
    }
    if is_jlcpcb && include_assembly && position_units != "mm" {
        return Ok(invalid_manufacturing_argument(
            "position_units",
            "JLCPCB CPL coordinates must use millimetres",
        ));
    }
    if jlcpcb_cpl_corrections_path.is_some() && (!is_jlcpcb || !include_assembly) {
        return Ok(invalid_manufacturing_argument(
            "jlcpcb_cpl_corrections_path",
            "requires fab_house='jlcpcb' and include_assembly=true",
        ));
    }

    info!(
        board = %board.display(),
        output_dir = %output_dir.display(),
        fab_house = %fab_house,
        include_assembly = include_assembly,
        "[BETA] Generating manufacturing package"
    );

    tokio::fs::create_dir_all(&output_dir).await?;

    let cli_path = &ctx.config.kicad_cli;
    let mut files_generated = Vec::new();
    let mut verified_paths = Vec::new();
    let mut warnings = Vec::new();
    let mut cpl_designators = None;
    let mut bom_designators = None;
    let mut cpl_orientation_evidence = None;

    // 1. Export Gerbers
    let gerber_dir = output_dir.join("gerbers");
    tokio::fs::create_dir_all(&gerber_dir).await?;
    let gerber_layer_refs = gerber_layers.iter().map(String::as_str).collect::<Vec<_>>();
    match cli::export_gerber(cli_path, &board, &gerber_dir, &gerber_layer_refs).await {
        Ok(gerber_files) => {
            info!(files = gerber_files.len(), "[BETA] Gerber export succeeded");
            verified_paths.extend(gerber_files.iter().cloned());
            files_generated.push(json!({
                "type": "gerber",
                "path": gerber_dir.to_str().unwrap_or(""),
                "layers": gerber_layers.clone(),
                "files": gerber_files.iter().map(|path| path.to_str().unwrap_or("")).collect::<Vec<_>>()
            }));
        }
        Err(e) => {
            error!(error = %e, "[BETA] Gerber export failed");
            warnings.push(format!("Gerber export failed: {}", e));
        }
    }

    // 2. Export drill files, into the gerber directory so a fab receives the
    //    plated and non-plated Excellon files alongside the layers they belong
    //    to. `--output` is a directory; the old `output_dir.join("drill.drl")`
    //    made KiCad create a directory named drill.drl, so the package
    //    advertised a "drill" file that was really an empty-looking folder and
    //    the real Excellon output never appeared in the file list at all.
    match cli::export_drill(cli_path, &board, &gerber_dir).await {
        Ok(drill_files) => {
            info!(files = drill_files.len(), "[BETA] Drill export succeeded");
            verified_paths.extend(drill_files.iter().cloned());
            for file in &drill_files {
                files_generated.push(json!({
                    "type": "drill",
                    "path": file.to_str().unwrap_or("")
                }));
            }
        }
        Err(e) => {
            error!(error = %e, "[BETA] Drill export failed");
            warnings.push(format!("Drill export failed: {e}"));
        }
    }

    // 3. Assembly files (BOM + pick-and-place)
    if include_assembly {
        // Pick-and-place (position file). KiCad's native CSV is retained for
        // generic callers. JLCPCB receives a structurally parsed CPL with the
        // exact vendor column contract instead of a header text replacement.
        let project_name = board
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("board");
        let pos_path = if is_jlcpcb {
            output_dir.join(format!("CPL-{project_name}.csv"))
        } else {
            output_dir.join("positions.csv")
        };
        let position_result = if is_jlcpcb {
            export_jlcpcb_cpl(
                cli_path,
                &board,
                &pos_path,
                position_side,
                jlcpcb_cpl_corrections_path.as_deref(),
            )
            .await
            .map(Some)
        } else {
            cli::export_position_file(
                cli_path,
                &board,
                &pos_path,
                "csv",
                position_units,
                position_side,
            )
            .await
            .map(|()| None)
        };
        match position_result {
            Ok(export) => {
                info!("[BETA] Position file export succeeded");
                if let Some(export) = export {
                    cpl_designators = Some(export.designators);
                    cpl_orientation_evidence = Some(export.orientation_evidence.clone());
                }
                verified_paths.push(pos_path.clone());
                let mut generated = json!({
                    "type": "pick_and_place",
                    "path": pos_path.to_str().unwrap_or(""),
                    "format": "csv",
                    "units": position_units,
                    "side": position_side
                });
                if let Some(evidence) = &cpl_orientation_evidence {
                    generated["placement_orientation"] = json!(evidence);
                }
                files_generated.push(generated);
            }
            Err(e) => {
                error!(error = %e, "[BETA] Position file export failed");
                warnings.push(format!("Position file export failed: {e:#}"));
            }
        }

        // BOM
        if let Some(ref sch) = schematic {
            let bom_path = if is_jlcpcb {
                output_dir.join(format!("BOM-{project_name}.csv"))
            } else {
                output_dir.join("bom.csv")
            };
            // Without bom_fields the package gets kicad-cli's fixed
            // Reference,Value,Footprint,QUANTITY,DNP set — no MPN, no supplier
            // part number, nothing a fab can source a part from.
            let bom_options = cli::BomOptions {
                fields: args["bom_fields"].as_str(),
                labels: args["bom_labels"].as_str(),
                group_by: args["bom_group_by"].as_str(),
                exclude_dnp: is_jlcpcb,
            };
            let bom_result = if is_jlcpcb {
                cli::export_bom_with_ref_range_delimiter(cli_path, sch, &bom_path, &bom_options, "")
                    .await
            } else {
                cli::export_bom(cli_path, sch, &bom_path, &bom_options).await
            };
            match bom_result {
                Ok(()) => {
                    let parsed = if is_jlcpcb {
                        let source = tokio::fs::read_to_string(&bom_path).await?;
                        jlcpcb_bom_designators(&source).map(Some)
                    } else {
                        Ok(None)
                    };
                    match parsed {
                        Ok(designators) => {
                            info!("[BETA] BOM export succeeded");
                            bom_designators = designators;
                            verified_paths.push(bom_path.clone());
                            files_generated.push(json!({
                                "type": "bom",
                                "path": bom_path.to_str().unwrap_or(""),
                                "format": "csv",
                                "fields": bom_options.fields
                            }));
                        }
                        Err(e) => {
                            error!(error = %e, "[BETA] JLCPCB BOM validation failed");
                            warnings.push(format!("JLCPCB BOM validation failed: {e:#}"));
                        }
                    }
                }
                Err(e) => {
                    error!(error = %e, "[BETA] BOM export failed");
                    warnings.push(format!("BOM export failed: {}", e));
                }
            }
        } else {
            warnings.push("No schematic provided — BOM not generated. Pass 'schematic' for full assembly package.".to_string());
        }

        if is_jlcpcb {
            if let (Some(cpl), Some(bom)) = (&cpl_designators, &bom_designators) {
                if let Some(mismatch) = jlcpcb_designator_mismatch(cpl, bom) {
                    warnings.push(mismatch);
                }
            }
        }
    }

    // Derive the public file list only from artifacts the CLI boundary already
    // verified as regular and non-empty. A stale or empty directory entry can
    // no longer make an incomplete package look successful (#252).
    let mut all_files = verified_paths
        .iter()
        .map(|path| {
            path.strip_prefix(&output_dir)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect::<Vec<_>>();
    all_files.sort();
    all_files.dedup();

    let complete = warnings.is_empty();

    let summary = format!(
        "{} for {}. {} verified non-empty files. {}",
        if complete {
            "Complete package"
        } else {
            "INCOMPLETE package"
        },
        fab_house.to_uppercase(),
        all_files.len(),
        if warnings.is_empty() {
            "No warnings.".to_string()
        } else {
            format!("{} warnings.", warnings.len())
        }
    );

    info!(
        complete = complete,
        files = all_files.len(),
        warnings = warnings.len(),
        "[BETA] Manufacturing package finished"
    );

    let next_steps = if complete {
        let upload = format!(
                "Upload only the verified paths listed in `files` from {} to {}'s order page. Gerbers go in the PCB order; BOM and CPL/positions go in the assembly order.",
                output_dir.display(),
                fab_house.to_uppercase()
            );
        if is_jlcpcb && include_assembly {
            format!(
                "{upload} Then inspect every component in JLCPCB Component Placements. Correction rules reduce known orientation mismatches; they do not replace the mandatory visual placement preview."
            )
        } else {
            upload
        }
    } else {
        "Do not upload this package. Resolve every warning and export again.".to_string()
    };
    let body = serde_json::to_string(&json!({
        "complete": complete,
        "fab_house": fab_house,
        "output_dir": output_dir.to_str().unwrap_or(""),
        "files": all_files,
        "files_generated": files_generated,
        "gerber_layers": gerber_layers,
        "position_units": if include_assembly { Some(position_units) } else { None },
        "position_side": if include_assembly { Some(position_side) } else { None },
        "placement_orientation": cpl_orientation_evidence,
        "warnings": warnings,
        "summary": summary,
        "next_steps": next_steps
    }))
    .unwrap();
    Ok(if complete {
        CallToolResult::text(body)
    } else {
        CallToolResult::error(body)
    })
}

async fn export_jlcpcb_cpl(
    cli_path: &str,
    board: &Path,
    output: &Path,
    side: &str,
    project_policy_path: Option<&Path>,
) -> anyhow::Result<JlcpcbCplExport> {
    let staging = tempfile::tempdir_in(
        output
            .parent()
            .context("JLCPCB CPL output has no parent directory")?,
    )?;
    let native = staging.path().join("kicad-positions.csv");
    cli::export_position_file_excluding_dnp(cli_path, board, &native, "csv", "mm", side).await?;
    let source = tokio::fs::read_to_string(&native).await?;
    let policies = JlcpcbCorrectionPolicies::load(project_policy_path).await?;
    let export = jlcpcb_cpl_from_kicad_csv(&source, &policies)?;
    cli::publish_verified_bytes(output, &export.bytes, "JLCPCB CPL").await?;
    Ok(export)
}

const BUILT_IN_JLCPCB_CORRECTIONS: &str = include_str!("jlcpcb_cpl_corrections_v1.json");

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JlcpcbCorrectionPolicy {
    schema_version: u64,
    policy_id: String,
    provenance: String,
    #[serde(default)]
    footprint_rules: Vec<JlcpcbCorrectionRule>,
    #[serde(default)]
    component_overrides: Vec<JlcpcbComponentOverride>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JlcpcbCorrectionRule {
    id: String,
    footprint_prefix: String,
    #[serde(default)]
    rotation_degrees: f64,
    #[serde(default)]
    offset_x_mm: f64,
    #[serde(default)]
    offset_y_mm: f64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JlcpcbComponentOverride {
    id: String,
    designator: String,
    #[serde(default)]
    rotation_degrees: f64,
    #[serde(default)]
    offset_x_mm: f64,
    #[serde(default)]
    offset_y_mm: f64,
}

#[derive(Debug)]
struct JlcpcbCorrectionPolicies {
    built_in: JlcpcbCorrectionPolicy,
    project: Option<JlcpcbCorrectionPolicy>,
}

impl JlcpcbCorrectionPolicies {
    async fn load(project_path: Option<&Path>) -> anyhow::Result<Self> {
        let built_in = parse_jlcpcb_correction_policy(BUILT_IN_JLCPCB_CORRECTIONS, "built-in")?;
        let project = if let Some(path) = project_path {
            let source = tokio::fs::read_to_string(path)
                .await
                .with_context(|| format!("read JLCPCB correction policy {}", path.display()))?;
            Some(parse_jlcpcb_correction_policy(
                &source,
                &path.display().to_string(),
            )?)
        } else {
            None
        };
        Ok(Self { built_in, project })
    }

    fn matching_rule<'a>(&'a self, designator: &str, footprint: &str) -> Option<MatchedRule<'a>> {
        if let Some(project) = &self.project {
            if let Some(rule) = project
                .component_overrides
                .iter()
                .find(|rule| rule.designator == designator)
            {
                return Some(MatchedRule::Component {
                    policy: project,
                    rule,
                });
            }
            if let Some(rule) = project
                .footprint_rules
                .iter()
                .find(|rule| footprint.starts_with(&rule.footprint_prefix))
            {
                return Some(MatchedRule::Footprint {
                    policy: project,
                    rule,
                });
            }
        }
        self.built_in
            .footprint_rules
            .iter()
            .find(|rule| footprint.starts_with(&rule.footprint_prefix))
            .map(|rule| MatchedRule::Footprint {
                policy: &self.built_in,
                rule,
            })
    }
}

enum MatchedRule<'a> {
    Component {
        policy: &'a JlcpcbCorrectionPolicy,
        rule: &'a JlcpcbComponentOverride,
    },
    Footprint {
        policy: &'a JlcpcbCorrectionPolicy,
        rule: &'a JlcpcbCorrectionRule,
    },
}

impl MatchedRule<'_> {
    fn values(&self) -> (&str, &str, &str, f64, f64, f64) {
        match self {
            Self::Component { policy, rule } => (
                &policy.policy_id,
                &rule.id,
                "component_override",
                rule.rotation_degrees,
                rule.offset_x_mm,
                rule.offset_y_mm,
            ),
            Self::Footprint { policy, rule } => (
                &policy.policy_id,
                &rule.id,
                "footprint_rule",
                rule.rotation_degrees,
                rule.offset_x_mm,
                rule.offset_y_mm,
            ),
        }
    }
}

fn parse_jlcpcb_correction_policy(
    source: &str,
    description: &str,
) -> anyhow::Result<JlcpcbCorrectionPolicy> {
    let policy: JlcpcbCorrectionPolicy = serde_json::from_str(source)
        .with_context(|| format!("parse JLCPCB correction policy {description}"))?;
    if policy.schema_version != 1 {
        bail!(
            "JLCPCB correction policy {description} uses unsupported schema_version {}",
            policy.schema_version
        );
    }
    if policy.policy_id.trim().is_empty() || policy.provenance.trim().is_empty() {
        bail!("JLCPCB correction policy {description} requires policy_id and provenance");
    }
    let mut ids = BTreeSet::new();
    for rule in &policy.footprint_rules {
        if rule.id.trim().is_empty() || rule.footprint_prefix.trim().is_empty() {
            bail!(
                "JLCPCB correction policy {description} has an empty rule id or footprint_prefix"
            );
        }
        if !ids.insert(rule.id.as_str()) {
            bail!(
                "JLCPCB correction policy {description} has duplicate rule id '{}'",
                rule.id
            );
        }
        validate_finite_correction(
            description,
            &rule.id,
            rule.rotation_degrees,
            rule.offset_x_mm,
            rule.offset_y_mm,
        )?;
    }
    let mut designators = BTreeSet::new();
    for rule in &policy.component_overrides {
        if rule.id.trim().is_empty() || rule.designator.trim().is_empty() {
            bail!("JLCPCB correction policy {description} has an empty override id or designator");
        }
        if !ids.insert(rule.id.as_str()) {
            bail!(
                "JLCPCB correction policy {description} has duplicate rule id '{}'",
                rule.id
            );
        }
        if !designators.insert(rule.designator.as_str()) {
            bail!(
                "JLCPCB correction policy {description} has duplicate override for '{}'",
                rule.designator
            );
        }
        validate_finite_correction(
            description,
            &rule.id,
            rule.rotation_degrees,
            rule.offset_x_mm,
            rule.offset_y_mm,
        )?;
    }
    Ok(policy)
}

fn validate_finite_correction(
    description: &str,
    id: &str,
    rotation: f64,
    offset_x: f64,
    offset_y: f64,
) -> anyhow::Result<()> {
    if [rotation, offset_x, offset_y]
        .iter()
        .any(|value| !value.is_finite())
    {
        bail!("JLCPCB correction rule '{id}' in {description} contains a non-finite value");
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
struct JlcpcbOrientationEvidence {
    status: &'static str,
    physical_validation: bool,
    policies: Vec<JlcpcbPolicyEvidence>,
    applied_corrections: Vec<JlcpcbAppliedCorrection>,
    unmatched_footprints: Vec<JlcpcbUnmatchedFootprint>,
    note: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct JlcpcbPolicyEvidence {
    policy_id: String,
    schema_version: u64,
    provenance: String,
    precedence: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct JlcpcbAppliedCorrection {
    designator: String,
    footprint: String,
    side: String,
    policy_id: String,
    rule_id: String,
    match_kind: String,
    rotation_before_degrees: f64,
    rotation_after_degrees: f64,
    x_before_mm: f64,
    x_after_mm: f64,
    y_before_mm: f64,
    y_after_mm: f64,
}

#[derive(Clone, Debug, Serialize)]
struct JlcpcbUnmatchedFootprint {
    designator: String,
    footprint: String,
    side: String,
}

#[derive(Debug)]
struct JlcpcbCplExport {
    bytes: Vec<u8>,
    designators: BTreeSet<String>,
    orientation_evidence: JlcpcbOrientationEvidence,
}

/// Translate KiCad 10's native position CSV into JLCPCB's documented CPL
/// contract: Designator, Mid X, Mid Y, Layer, Rotation. Parsing and writing as
/// CSV preserves quoted commas and non-ASCII values even though Val/Package do
/// not belong in the vendor file.
fn jlcpcb_cpl_from_kicad_csv(
    source: &str,
    policies: &JlcpcbCorrectionPolicies,
) -> anyhow::Result<JlcpcbCplExport> {
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(source.as_bytes());
    let headers = reader.headers()?.clone();
    let column = |name: &str| {
        headers
            .iter()
            .position(|header| header == name)
            .with_context(|| format!("KiCad position CSV is missing '{name}'"))
    };
    let reference = column("Ref")?;
    let pos_x = column("PosX")?;
    let pos_y = column("PosY")?;
    let rotation = column("Rot")?;
    let side = column("Side")?;
    let package = column("Package")?;

    let mut writer = csv::WriterBuilder::new().from_writer(Vec::new());
    writer.write_record(["Designator", "Mid X", "Mid Y", "Layer", "Rotation"])?;
    let mut designators = BTreeSet::new();
    let mut applied_corrections = Vec::new();
    let mut unmatched_footprints = Vec::new();
    for row in reader.records() {
        let row = row?;
        let field = |index: usize, name: &str| {
            row.get(index)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .with_context(|| format!("KiCad position row is missing '{name}'"))
        };
        let designator = field(reference, "Ref")?;
        let footprint = field(package, "Package")?
            .rsplit(':')
            .next()
            .context("footprint basename is missing")?;
        let x_source = field(pos_x, "PosX")?;
        let y_source = field(pos_y, "PosY")?;
        let angle_source = field(rotation, "Rot")?;
        let mut x = x_source
            .parse::<f64>()
            .with_context(|| format!("invalid PosX for {designator}: {x_source}"))?;
        let mut y = y_source
            .parse::<f64>()
            .with_context(|| format!("invalid PosY for {designator}: {y_source}"))?;
        let angle = angle_source
            .parse::<f64>()
            .with_context(|| format!("invalid Rot for {designator}: {angle_source}"))?;
        let layer = match field(side, "Side")?.to_ascii_lowercase().as_str() {
            "top" | "front" => "top",
            "bottom" | "back" => "bottom",
            other => bail!("invalid Side for {designator}: {other}"),
        };
        if !designators.insert(designator.to_string()) {
            bail!("duplicate CPL designator '{designator}'");
        }
        let before_x = x;
        let before_y = y;
        let matched = policies.matching_rule(designator, footprint);
        let corrected_angle = if let Some(rule) = matched {
            let (policy_id, rule_id, match_kind, rotation_delta, offset_x, offset_y) =
                rule.values();
            x += offset_x;
            y += offset_y;
            let corrected = if layer == "bottom" {
                (180.0 - (angle - rotation_delta)).rem_euclid(360.0)
            } else {
                (angle + rotation_delta).rem_euclid(360.0)
            };
            applied_corrections.push(JlcpcbAppliedCorrection {
                designator: designator.to_string(),
                footprint: footprint.to_string(),
                side: layer.to_string(),
                policy_id: policy_id.to_string(),
                rule_id: rule_id.to_string(),
                match_kind: match_kind.to_string(),
                rotation_before_degrees: angle,
                rotation_after_degrees: corrected,
                x_before_mm: before_x,
                x_after_mm: x,
                y_before_mm: before_y,
                y_after_mm: y,
            });
            corrected
        } else {
            unmatched_footprints.push(JlcpcbUnmatchedFootprint {
                designator: designator.to_string(),
                footprint: footprint.to_string(),
                side: layer.to_string(),
            });
            if layer == "bottom" {
                (180.0 - angle).rem_euclid(360.0)
            } else {
                angle.rem_euclid(360.0)
            }
        };
        writer.write_record([
            designator.to_string(),
            format!("{x:.6}"),
            format!("{y:.6}"),
            layer.to_string(),
            format!("{corrected_angle:.6}"),
        ])?;
    }
    if designators.is_empty() {
        bail!("KiCad position CSV contains no components");
    }
    writer.flush()?;
    let mut policy_evidence = Vec::new();
    if let Some(project) = &policies.project {
        policy_evidence.push(JlcpcbPolicyEvidence {
            policy_id: project.policy_id.clone(),
            schema_version: project.schema_version,
            provenance: project.provenance.clone(),
            precedence: "project component override, then project footprint first-match",
        });
    }
    policy_evidence.push(JlcpcbPolicyEvidence {
        policy_id: policies.built_in.policy_id.clone(),
        schema_version: policies.built_in.schema_version,
        provenance: policies.built_in.provenance.clone(),
        precedence: "built-in footprint first-match after project policy",
    });
    Ok(JlcpcbCplExport {
        bytes: writer.into_inner()?,
        designators,
        orientation_evidence: JlcpcbOrientationEvidence {
            status: "PREVIEW_REQUIRED",
            physical_validation: false,
            policies: policy_evidence,
            applied_corrections,
            unmatched_footprints,
            note: "Structural export does not prove physical placement orientation. Inspect every component in JLCPCB Component Placements before ordering.",
        },
    })
}

fn jlcpcb_bom_designators(source: &str) -> anyhow::Result<BTreeSet<String>> {
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(source.as_bytes());
    let headers = reader.headers()?.clone();
    let reference = headers
        .iter()
        .position(|header| matches!(header, "Designator" | "Reference" | "Refs"))
        .context("JLCPCB BOM has no Designator/Reference/Refs column")?;
    let mut designators = BTreeSet::new();
    for row in reader.records() {
        let row = row?;
        let group = row
            .get(reference)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("JLCPCB BOM contains an empty designator group")?;
        for designator in group.split(',').map(str::trim) {
            if designator.contains('-') {
                bail!(
                    "JLCPCB BOM contains compressed designator range '{designator}'; pass an empty KiCad reference-range delimiter"
                );
            }
            if designator.is_empty() || !designators.insert(designator.to_string()) {
                bail!("JLCPCB BOM contains an empty or duplicate designator '{designator}'");
            }
        }
    }
    if designators.is_empty() {
        bail!("JLCPCB BOM contains no designators");
    }
    Ok(designators)
}

fn jlcpcb_designator_mismatch(cpl: &BTreeSet<String>, bom: &BTreeSet<String>) -> Option<String> {
    let missing_from_bom = cpl.difference(bom).cloned().collect::<Vec<_>>();
    let missing_from_cpl = bom.difference(cpl).cloned().collect::<Vec<_>>();
    if missing_from_bom.is_empty() && missing_from_cpl.is_empty() {
        None
    } else {
        Some(format!(
            "JLCPCB BOM/CPL designators do not match: missing from BOM [{}]; missing from CPL [{}]",
            missing_from_bom.join(", "),
            missing_from_cpl.join(", ")
        ))
    }
}

fn invalid_manufacturing_argument(field: &str, reason: impl Into<String>) -> CallToolResult {
    let reason = reason.into();
    CallToolResult::error_kind(
        crate::mcp::error::ToolErrorKind::InvalidArgument {
            field: field.to_string(),
            reason: reason.clone(),
        },
        format!("Argument '{field}' is invalid: {reason}"),
    )
}

async fn handle_validate_for_manufacturing(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let fab_house = args["fab_house"].as_str().unwrap_or("jlcpcb");

    info!(
        board = %board.display(),
        fab_house = %fab_house,
        "[BETA] Running manufacturing validation"
    );

    let content = tokio::fs::read_to_string(&board).await?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;

    let mut issues = Vec::new();

    // Check board outline
    let has_outline = content.contains("Edge.Cuts");
    if !has_outline {
        issues.push(json!({
            "severity": "error",
            "issue": "No board outline found on Edge.Cuts layer",
            "fix": "Add a board outline using add_board_outline before ordering"
        }));
    }

    // Check that footprints exist
    let fp_count = tree.find_all("footprint").len();
    if fp_count == 0 {
        issues.push(json!({
            "severity": "error",
            "issue": "No footprints found on the board",
            "fix": "Open the PCB in KiCAD and run Tools > Update PCB from Schematic (kicad-cli has no 'pcb sync' command)"
        }));
    }

    // Check layer count — structurally, from the `(layers …)` table, through
    // the same function `get_board_info` uses (#461). Counting the substring
    // `signal)` missed every `power`/`mixed`/`jumper` copper layer.
    let copper_layers = konnect_sexp::layers::copper_layer_count(&tree);
    debug!(
        copper_layers = copper_layers,
        "[BETA] Detected copper layers"
    );

    // Fab-specific checks
    let (min_trace, _min_drill, _max_layers) = match fab_house {
        "jlcpcb" => (0.127, 0.3, 32),
        "oshpark" => (0.152, 0.254, 4),
        "pcbway" => (0.1, 0.2, 32),
        _ => (0.15, 0.3, 32),
    };

    // Check design rules
    if let Some(min_tw) = find_setup_value(&content, "min_trace_width") {
        if min_tw < min_trace {
            issues.push(json!({
                "severity": "error",
                "issue": format!("Trace width {:.3}mm is below {}'s minimum ({:.3}mm)", min_tw, fab_house, min_trace),
                "fix": format!("Increase minimum trace width to {:.3}mm in design rules", min_trace)
            }));
        }
    }

    // Check for unrouted nets (ratsnest)
    let (net_count, track_count) = count_nets_and_tracks(&tree);
    if net_count > 3 && track_count == 0 {
        issues.push(json!({
            "severity": "error",
            "issue": format!("{} nets defined but no traces routed", net_count),
            "fix": "Route traces using route_trace before manufacturing"
        }));
    }

    // That heuristic only fires on a board with *zero* tracks, so a board
    // routed except for one net sailed past it — and nothing here had ever
    // consulted DRC, which is the only thing that actually knows. This tool
    // returned READY on a board with 25 DRC errors and an unrouted item
    // (#247). A readiness verdict now requires the evidence.
    let drc = cli::run_drc(&ctx.config.kicad_cli, &board, false).await;
    let drc_summary = match &drc {
        Ok(report) => {
            for violation in report.all().filter(|v| v.severity == "error") {
                issues.push(json!({
                    "severity": "error",
                    "issue": format!("DRC [{}]: {}", violation.rule, violation.description),
                    "fix": "Fix in the PCB editor, or waive the rule deliberately; \
                            run_drc lists every violation with its location"
                }));
            }
            for missing in report.missing_categories() {
                issues.push(json!({
                    "severity": "error",
                    "issue": format!("kicad-cli did not report DRC '{missing}'"),
                    "fix": "Readiness cannot be established without it; check the \
                            kicad-cli version"
                }));
            }
            json!({
                "errors": report.error_count(),
                "design_rule_violations": report.violations.len(),
                "unconnected_items": report.unconnected_items.as_ref().map(Vec::len),
                "schematic_parity": report.schematic_parity.as_ref().map(Vec::len),
            })
        }
        Err(error) => {
            issues.push(json!({
                "severity": "error",
                "issue": format!("DRC could not run: {error:#}"),
                "fix": "Without DRC this tool cannot tell a clean board from a \
                        broken one; fix the kicad-cli path and re-run"
            }));
            serde_json::Value::Null
        }
    };

    let verdict = if issues.iter().any(|i| i["severity"] == "error") {
        "NOT READY"
    } else if !issues.is_empty() {
        "NEEDS REVIEW"
    } else {
        "READY"
    };

    info!(
        verdict = verdict,
        issues = issues.len(),
        "[BETA] Manufacturing validation complete"
    );

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "verdict": verdict,
            "fab_house": fab_house,
            "board_info": {
                "footprint_count": fp_count,
                "copper_layers": copper_layers,
                "net_count": net_count,
                "track_count": track_count
            },
            // Null means DRC did not run, and an issue above says so. Never a
            // zeroed-out object: this tool must not be able to imply a clean
            // board it never checked.
            "drc": drc_summary,
            "issues": issues,
            "summary": format!(
                "{}: {} issues found. {} footprints, {} copper layers.",
                verdict, issues.len(), fp_count, copper_layers
            )
        }))
        .unwrap(),
    ))
}

async fn handle_estimate_cost(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let fab_house = args["fab_house"].as_str().unwrap_or("jlcpcb");
    let quantity = args["quantity"].as_u64().unwrap_or(5) as usize;

    info!(
        board = %board.display(),
        fab_house = %fab_house,
        quantity = quantity,
        "[BETA] Estimating manufacturing cost"
    );

    let content = tokio::fs::read_to_string(&board).await?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;

    // Count components
    let fps = tree.find_all("footprint");
    let component_count = fps.len();

    // The board's own copper count comes from its `(layers …)` table, through
    // the same function `get_board_info` uses (#461). A caller may still quote
    // at a different count, but the quote then says so instead of presenting
    // the requested number as the board's.
    let board_copper_layers = konnect_sexp::layers::copper_layer_count(&tree);
    let requested_layers = args["layers"].as_u64().map(|n| n as usize);
    let copper_layers = requested_layers.unwrap_or(board_copper_layers);
    let mut warnings: Vec<String> = Vec::new();
    match requested_layers {
        Some(requested) if requested != board_copper_layers => warnings.push(format!(
            "quoted at {requested} copper layers, but the board file declares \
             {board_copper_layers}; this estimate does not describe the board as saved"
        )),
        _ => {}
    }
    if board_copper_layers == 0 {
        warnings.push(
            "the board file declares no copper layers (no `(layers …)` table was read); \
             the layer count could not be taken from the board"
                .to_string(),
        );
    }

    // Estimate board dimensions from Edge.Cuts
    let (width_mm, height_mm) = estimate_board_dimensions(&content);

    // Rough cost estimation based on fab house pricing models
    let (pcb_cost, assembly_cost, component_est) = match fab_house {
        "jlcpcb" => {
            let pcb = match copper_layers {
                // JLCPCB prices single-sided boards on the two-layer scale.
                1 | 2 => 2.0 + (quantity as f64 - 5.0).max(0.0) * 0.40,
                4 => 7.0 + (quantity as f64 - 5.0).max(0.0) * 1.40,
                6 => 15.0 + (quantity as f64 - 5.0).max(0.0) * 3.00,
                _ => 30.0 + (quantity as f64 - 5.0).max(0.0) * 5.00,
            };
            let smt_setup = if component_count > 0 { 8.0 } else { 0.0 };
            let smt_per_board = component_count as f64 * 0.003 * quantity as f64;
            let comp_est = component_count as f64 * 0.05; // rough avg per component
            (pcb, smt_setup + smt_per_board, comp_est * quantity as f64)
        }
        "pcbway" => {
            let pcb = match copper_layers {
                1 | 2 => 5.0 + (quantity as f64 - 5.0).max(0.0) * 0.50,
                4 => 12.0 + (quantity as f64 - 5.0).max(0.0) * 2.00,
                _ => 25.0 + (quantity as f64 - 5.0).max(0.0) * 4.00,
            };
            let smt = component_count as f64 * 0.005 * quantity as f64;
            let comp_est = component_count as f64 * 0.08 * quantity as f64;
            (pcb, smt, comp_est)
        }
        _ => {
            let pcb = 10.0 + quantity as f64 * 2.0;
            (pcb, 0.0, 0.0)
        }
    };

    let total = pcb_cost + assembly_cost + component_est;

    debug!(
        pcb_cost = pcb_cost,
        assembly_cost = assembly_cost,
        component_est = component_est,
        total = total,
        "[BETA] Cost estimate calculated"
    );

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "fab_house": fab_house,
            "quantity": quantity,
            "board": {
                "width_mm": width_mm,
                "height_mm": height_mm,
                // What this estimate was priced at, and what the board itself
                // declares. They differ only when the caller passed `layers`.
                "copper_layers": copper_layers,
                "board_copper_layers": board_copper_layers,
                "component_count": component_count
            },
            "warnings": warnings,
            "cost_estimate": {
                "pcb_fabrication": format!("${:.2}", pcb_cost),
                "smt_assembly": format!("${:.2}", assembly_cost),
                "components_estimate": format!("${:.2}", component_est),
                "total_estimate": format!("${:.2}", total),
                "per_board": format!("${:.2}", total / quantity as f64)
            },
            "notes": [
                "Estimates are approximate — actual cost depends on board size, finish, and specific components",
                "Component costs are rough averages — use generate_bom with supply chain data for accurate pricing",
                format!("Based on {} quantity from {}", quantity, fab_house.to_uppercase())
            ],
            "disclaimer": "BETA: Cost estimates are indicative only. Always confirm with the fab house's online quoting tool."
        }))
        .unwrap(),
    ))
}

#[cfg(test)]
mod package_export_option_tests {
    use super::*;

    fn result_json(result: &CallToolResult) -> serde_json::Value {
        match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => {
                serde_json::from_str(text).unwrap()
            }
            other => panic!("expected text result, got {other:?}"),
        }
    }

    #[test]
    fn package_schema_exposes_applied_gerber_and_position_options() {
        let package = tools()
            .into_iter()
            .find(|tool| tool.name == "export_manufacturing_package")
            .unwrap();
        let properties = &package.input_schema["properties"];
        assert_eq!(properties["gerber_layers"]["items"]["type"], "string");
        assert_eq!(properties["position_units"]["enum"], json!(["mm", "in"]));
        assert_eq!(
            properties["position_side"]["enum"],
            json!(["front", "back", "both"])
        );
        assert_eq!(properties["jlcpcb_cpl_corrections_path"]["type"], "string");
    }

    #[tokio::test]
    async fn package_is_an_error_when_cli_success_produces_no_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("live_ipc.kicad_pcb");
        // Real KiCad 9 output, as required for tests that parse board layers.
        std::fs::write(
            &board,
            include_str!("../../../konnect-ipc/tests/fixtures/live_ipc.kicad_pcb"),
        )
        .unwrap();
        let cli = crate::tools::cli::test_support::noop_cli(dir.path());
        let ctx = ToolContext::new(
            crate::tools::ServerConfig {
                kicad_cli: cli.display().to_string(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        );

        let result = handle_export_manufacturing_package(
            &json!({
                "board": board.display().to_string(),
                "output_dir": dir.path().join("package").display().to_string(),
                "include_assembly": false
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error, "incomplete package must fail closed");
        let body = result_json(&result);
        assert_eq!(body["complete"], false);
        assert_eq!(body["files"], json!([]));
        assert!(body["warnings"].as_array().unwrap().len() >= 2, "{body}");
        assert!(body["next_steps"]
            .as_str()
            .unwrap()
            .starts_with("Do not upload"));
    }

    #[tokio::test]
    async fn jlcpcb_assembly_rejects_non_metric_positions_before_writing_output() {
        let dir = tempfile::tempdir().unwrap();
        let board = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/specctra_two_resistors_locked.kicad_pcb");
        let output = dir.path().join("package");
        let ctx = ToolContext::new(
            crate::tools::ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        );

        let result = handle_export_manufacturing_package(
            &json!({
                "board": board,
                "output_dir": output,
                "fab_house": "jlcpcb",
                "include_assembly": true,
                "position_units": "in"
            }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(result.is_error);
        assert!(!output.exists(), "invalid request must not create output");
        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
            other => panic!("expected text result, got {other:?}"),
        };
        assert!(text.contains("must use millimetres"));
    }

    #[tokio::test]
    async fn jlcpcb_policy_is_rejected_when_the_export_cannot_apply_it() {
        let dir = tempfile::tempdir().unwrap();
        let board = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/specctra_two_resistors_locked.kicad_pcb");
        let output = dir.path().join("package");
        let policy = dir.path().join("policy.json");
        std::fs::write(&policy, "{}").unwrap();
        let ctx = ToolContext::new(
            crate::tools::ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        );

        let result = handle_export_manufacturing_package(
            &json!({
                "board": board,
                "output_dir": output,
                "fab_house": "generic",
                "include_assembly": true,
                "jlcpcb_cpl_corrections_path": policy
            }),
            &ctx,
        )
        .await
        .unwrap();

        assert!(result.is_error);
        assert!(!output.exists(), "invalid request must not create output");
        let text = match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
            other => panic!("expected text result, got {other:?}"),
        };
        assert!(text.contains("requires fab_house='jlcpcb'"));
    }
}

#[cfg(test)]
mod jlcpcb_assembly_tests {
    use super::*;

    /// Captured verbatim from KiCad 10.0.6 using:
    /// `kicad-cli pcb export pos --format csv --units mm --side both`
    /// against the repository's real `pic_programmer.kicad_pcb` fixture.
    const KICAD_POSITIONS: &str =
        include_str!("../../tests/fixtures/positions_pic_programmer_kicad10.csv");
    const ISSUE_518_POSITIONS: &str =
        include_str!("../../tests/fixtures/positions_jlcpcb_issue518_kicad10.csv");

    fn built_in_policies() -> JlcpcbCorrectionPolicies {
        JlcpcbCorrectionPolicies {
            built_in: parse_jlcpcb_correction_policy(BUILT_IN_JLCPCB_CORRECTIONS, "test").unwrap(),
            project: None,
        }
    }

    fn csv_rows(bytes: &[u8]) -> Vec<Vec<String>> {
        csv::ReaderBuilder::new()
            .has_headers(false)
            .from_reader(bytes)
            .records()
            .map(|record| record.unwrap().iter().map(str::to_string).collect())
            .collect()
    }

    #[test]
    fn real_kicad_positions_become_the_documented_jlcpcb_cpl_schema() {
        let export = jlcpcb_cpl_from_kicad_csv(KICAD_POSITIONS, &built_in_policies()).unwrap();
        let rows = csv_rows(&export.bytes);
        assert_eq!(
            rows[0],
            ["Designator", "Mid X", "Mid Y", "Layer", "Rotation"]
        );
        assert_eq!(
            rows[1],
            ["JP1", "148.082000", "-97.790000", "bottom", "180.000000"]
        );
        assert!(rows.iter().any(|row| {
            row.as_slice() == ["R10", "114.300000", "-48.260000", "top", "0.000000"]
        }));
        assert_eq!(export.designators.len(), rows.len() - 1);
        assert!(export.designators.contains("JP1"));
        assert!(export.designators.contains("R10"));
        assert_eq!(export.orientation_evidence.status, "PREVIEW_REQUIRED");
        assert!(!export.orientation_evidence.physical_validation);
    }

    #[test]
    fn malformed_or_ambiguous_position_rows_fail_instead_of_becoming_cpl() {
        let missing_column = "Ref,Package,PosX,PosY,Rot\nR1,R_0402,1,2,0\n";
        let policies = built_in_policies();
        assert!(jlcpcb_cpl_from_kicad_csv(missing_column, &policies)
            .unwrap_err()
            .to_string()
            .contains("Side"));

        let duplicate =
            "Ref,Package,PosX,PosY,Rot,Side\nR1,R_0402,1,2,0,top\nR1,R_0402,3,4,0,bottom\n";
        assert!(jlcpcb_cpl_from_kicad_csv(duplicate, &policies)
            .unwrap_err()
            .to_string()
            .contains("duplicate CPL designator"));

        let invalid_number = "Ref,Package,PosX,PosY,Rot,Side\nR1,R_0402,left,2,0,top\n";
        assert!(jlcpcb_cpl_from_kicad_csv(invalid_number, &policies)
            .unwrap_err()
            .to_string()
            .contains("invalid PosX"));
    }

    #[test]
    fn built_in_rules_correct_soic_usb_c_bottom_and_normalize_angles() {
        let source = concat!(
            "Ref,Val,Package,PosX,PosY,Rot,Side\n",
            "U1,timer,Package_SO:SOIC-8_3.9x4.9mm_P1.27mm,47,-35,90,top\n",
            "J1,usb,Connector_USB:USB_C_Receptacle_HRO_TYPE-C-31-M-12,30,-26.5,0,top\n",
            "R1,10k,Resistor_SMD:R_0402_1005Metric,1,2,450,top\n",
            "U2,timer,Package_SO:SOIC-8_3.9x4.9mm_P1.27mm,5,6,90,bottom\n",
        );
        let export = jlcpcb_cpl_from_kicad_csv(source, &built_in_policies()).unwrap();
        let rows = csv_rows(&export.bytes);
        assert_eq!(rows[1][4], "0.000000");
        assert_eq!(rows[2][4], "180.000000");
        assert_eq!(rows[3][4], "90.000000");
        assert_eq!(rows[4][4], "0.000000");
        assert_eq!(export.orientation_evidence.applied_corrections.len(), 3);
        assert_eq!(export.orientation_evidence.unmatched_footprints.len(), 1);
        assert_eq!(
            export.orientation_evidence.unmatched_footprints[0].designator,
            "R1"
        );
    }

    #[test]
    fn project_component_override_precedes_project_and_built_in_footprint_rules() {
        let project = parse_jlcpcb_correction_policy(
            r#"{
              "schema_version": 1,
              "policy_id": "project-clock-v1",
              "provenance": "Project owner verified in JLCPCB placement preview",
              "footprint_rules": [{
                "id": "project-soic",
                "footprint_prefix": "SOIC-8_",
                "rotation_degrees": 90,
                "offset_x_mm": 1,
                "offset_y_mm": -2
              }],
              "component_overrides": [{
                "id": "u1-exact",
                "designator": "U1",
                "rotation_degrees": 45,
                "offset_x_mm": 3,
                "offset_y_mm": 4
              }]
            }"#,
            "project test",
        )
        .unwrap();
        let policies = JlcpcbCorrectionPolicies {
            built_in: built_in_policies().built_in,
            project: Some(project),
        };
        let source = concat!(
            "Ref,Val,Package,PosX,PosY,Rot,Side\n",
            "U1,timer,Package_SO:SOIC-8_3.9x4.9mm_P1.27mm,1,2,10,top\n",
            "U2,timer,Package_SO:SOIC-8_3.9x4.9mm_P1.27mm,1,2,10,top\n",
        );
        let export = jlcpcb_cpl_from_kicad_csv(source, &policies).unwrap();
        let rows = csv_rows(&export.bytes);
        assert_eq!(&rows[1][1..], ["4.000000", "6.000000", "top", "55.000000"]);
        assert_eq!(&rows[2][1..], ["2.000000", "0.000000", "top", "100.000000"]);
        assert_eq!(
            export.orientation_evidence.applied_corrections[0].match_kind,
            "component_override"
        );
        assert_eq!(
            export.orientation_evidence.applied_corrections[1].rule_id,
            "project-soic"
        );
    }

    #[test]
    fn malformed_or_ambiguous_correction_policies_fail_closed() {
        let unsupported = r#"{
          "schema_version": 2,
          "policy_id": "future",
          "provenance": "test"
        }"#;
        assert!(parse_jlcpcb_correction_policy(unsupported, "test")
            .unwrap_err()
            .to_string()
            .contains("unsupported schema_version"));

        let duplicate = r#"{
          "schema_version": 1,
          "policy_id": "duplicate",
          "provenance": "test",
          "component_overrides": [
            {"id":"first", "designator":"U1", "rotation_degrees":90},
            {"id":"second", "designator":"U1", "rotation_degrees":180}
          ]
        }"#;
        assert!(parse_jlcpcb_correction_policy(duplicate, "test")
            .unwrap_err()
            .to_string()
            .contains("duplicate override"));
    }

    #[test]
    fn real_issue_518_kicad_fixture_changes_only_known_package_values() {
        let export = jlcpcb_cpl_from_kicad_csv(ISSUE_518_POSITIONS, &built_in_policies()).unwrap();
        let rows = csv_rows(&export.bytes);
        let u1 = rows.iter().find(|row| row[0] == "U1").unwrap();
        let j1 = rows.iter().find(|row| row[0] == "J1").unwrap();
        let r1 = rows.iter().find(|row| row[0] == "R1").unwrap();
        assert_eq!(u1, &["U1", "47.000000", "-35.000000", "top", "0.000000"]);
        assert_eq!(j1, &["J1", "30.000000", "-26.500000", "top", "180.000000"]);
        assert_eq!(r1, &["R1", "38.000000", "-24.000000", "top", "90.000000"]);
        assert_eq!(export.designators.len(), 12);
    }

    #[test]
    fn fully_enumerated_bom_groups_match_individual_cpl_rows() {
        let bom = "\"Designator\",\"Comment\",\"Footprint\",\"LCSC Part #\"\n\"C1,C2,C3\",\"100nF\",\"C_0402\",\"C1525\"\n\"R1\",\"10k\",\"R_0402\",\"C25744\"\n";
        let bom_refs = jlcpcb_bom_designators(bom).unwrap();
        let cpl_refs = ["C1", "C2", "C3", "R1"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(jlcpcb_designator_mismatch(&cpl_refs, &bom_refs), None);
    }

    #[test]
    fn compressed_ranges_and_cross_file_mismatches_fail_closed() {
        let ranged = "Designator,Comment\nC1-C3,100nF\n";
        assert!(jlcpcb_bom_designators(ranged)
            .unwrap_err()
            .to_string()
            .contains("compressed designator range"));

        let cpl = ["C1", "C2", "U1"].into_iter().map(str::to_string).collect();
        let bom = ["C1", "C2", "R1"].into_iter().map(str::to_string).collect();
        let mismatch = jlcpcb_designator_mismatch(&cpl, &bom).unwrap();
        assert!(mismatch.contains("missing from BOM [U1]"));
        assert!(mismatch.contains("missing from CPL [R1]"));
    }

    /// This is an output-level test of KiCad's range switch, not merely an
    /// assertion about the argument vector. The checked-in schematic is a
    /// KiCad demo saved by KiCad and contains groups of one, two, and more than
    /// three identical parts across a hierarchy.
    #[tokio::test]
    #[ignore = "requires an installed KiCad 10 kicad-cli"]
    async fn real_kicad_grouped_bom_enumerates_every_reference() {
        let cli_path = std::env::var("KICAD_CLI_PATH").unwrap_or_else(|_| "kicad-cli".into());
        let schematic = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/project_ownership/complex_hierarchy.kicad_sch");
        let dir = tempfile::tempdir().unwrap();
        let enumerated = dir.path().join("enumerated.csv");
        let ranged = dir.path().join("ranged.csv");

        let options = cli::BomOptions {
            fields: Some("Reference,Value,Footprint"),
            labels: Some("Designator,Comment,Footprint"),
            group_by: Some("Value,Footprint"),
            exclude_dnp: true,
        };
        cli::export_bom_with_ref_range_delimiter(&cli_path, &schematic, &enumerated, &options, "")
            .await
            .unwrap();
        let source = tokio::fs::read_to_string(&enumerated).await.unwrap();
        let mut reader = csv::Reader::from_reader(source.as_bytes());
        let mut group_sizes = Vec::new();
        for row in reader.records() {
            let row = row.unwrap();
            let group = row.get(0).unwrap();
            assert!(!group.contains('-'), "compressed group escaped: {group}");
            group_sizes.push(group.split(',').count());
        }
        assert!(group_sizes.contains(&1));
        assert!(group_sizes.contains(&2));
        assert!(group_sizes.iter().any(|size| *size >= 3));

        cli::export_bom(&cli_path, &schematic, &ranged, &options)
            .await
            .unwrap();
        let ranged_source = tokio::fs::read_to_string(&ranged).await.unwrap();
        let ranged_refs = jlcpcb_bom_designators(&ranged_source).unwrap_err();
        assert!(ranged_refs
            .to_string()
            .contains("compressed designator range"));
    }

    /// The KiCad ECC83 demo contains board footprints excluded from both the
    /// BOM and position files. Running the real exporters proves their output
    /// populations remain identical after the JLCPCB transformations.
    #[tokio::test]
    #[ignore = "requires an installed KiCad 10 kicad-cli"]
    async fn real_kicad_exclusions_leave_a_matched_bom_and_cpl() {
        let cli_path = std::env::var("KICAD_CLI_PATH").unwrap_or_else(|_| "kicad-cli".into());
        let fixture_root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../konnect-sexp/tests/fixtures");
        let schematic = fixture_root.join("variants/ecc83-pp.kicad_sch");
        let board = fixture_root.join("ecc83-pp.kicad_pcb");
        let dir = tempfile::tempdir().unwrap();
        let bom = dir.path().join("BOM-ecc83.csv");
        let cpl = dir.path().join("CPL-ecc83.csv");

        let cpl_export = export_jlcpcb_cpl(&cli_path, &board, &cpl, "both", None)
            .await
            .unwrap();
        let options = cli::BomOptions {
            fields: Some("Reference,Value,Footprint"),
            labels: Some("Designator,Comment,Footprint"),
            group_by: Some("Value,Footprint"),
            exclude_dnp: true,
        };
        cli::export_bom_with_ref_range_delimiter(&cli_path, &schematic, &bom, &options, "")
            .await
            .unwrap();
        let bom_source = tokio::fs::read_to_string(&bom).await.unwrap();
        let bom_refs = jlcpcb_bom_designators(&bom_source).unwrap();
        assert_eq!(
            jlcpcb_designator_mismatch(&cpl_export.designators, &bom_refs),
            None
        );
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Distinct nets and routed items on the board, read from the parsed tree.
///
/// Both net shapes have to be handled. KiCad ≤ 9 declares each net once at top
/// level — `(net 1 "GND")` — and items refer to it by number, `(net 1)`. KiCad
/// 10 dropped the top-level table entirely and writes the name on every item,
/// `(net "GND")`. Counting names when any are present covers both without
/// double-counting a KiCad 9 net through its declaration *and* its references.
///
/// Tracks are always direct children of `(kicad_pcb …)`, so they are counted
/// there rather than by walking: `(arc …)` also appears inside `(pts …)` of a
/// zone outline, which is not routed copper.
fn count_nets_and_tracks(tree: &konnect_sexp::SexpNode) -> (usize, usize) {
    use std::collections::HashSet;

    fn walk(node: &konnect_sexp::SexpNode, names: &mut HashSet<String>, ids: &mut HashSet<String>) {
        let Some(children) = node.children() else {
            return;
        };
        if node.head() == Some("net") {
            match children.last() {
                // (net 1 "GND") and (net "GND") both end in the quoted name.
                Some(konnect_sexp::SexpNode::Str(name)) if !name.is_empty() => {
                    names.insert(name.clone());
                }
                // (net 1) — a bare reference in a file whose table we have not
                // seen. Net 0 is the unconnected pseudo-net.
                Some(konnect_sexp::SexpNode::Atom(id)) if id != "0" => {
                    ids.insert(id.clone());
                }
                _ => {}
            }
        }
        for child in children {
            walk(child, names, ids);
        }
    }

    let mut names = HashSet::new();
    let mut ids = HashSet::new();
    walk(tree, &mut names, &mut ids);
    let net_count = if names.is_empty() {
        ids.len()
    } else {
        names.len()
    };

    let track_count =
        tree.find_all("segment").len() + tree.find_all("via").len() + tree.find_all("arc").len();

    (net_count, track_count)
}

fn find_setup_value(content: &str, key: &str) -> Option<f64> {
    let pat = format!("({} ", key);
    let pos = content.find(&pat)?;
    let after = &content[pos + pat.len()..];
    let end = after.find(')')?;
    after[..end].trim().parse().ok()
}

fn estimate_board_dimensions(content: &str) -> (f64, f64) {
    let mut min_x = f64::MAX;
    let mut max_x = f64::MIN;
    let mut min_y = f64::MAX;
    let mut max_y = f64::MIN;
    let mut found = false;

    // Scan gr_line on Edge.Cuts for board outline coordinates
    let mut pos = 0;
    while let Some(line_pos) = content[pos..].find("(gr_line") {
        let abs = pos + line_pos;
        let block_end = content[abs..].find(")\n").unwrap_or(300) + abs;
        let block = &content[abs..block_end.min(content.len())];

        if block.contains("Edge.Cuts") {
            // Extract start and end coordinates
            if let (Some(sx), Some(sy)) = (
                extract_coord(block, "start", 0),
                extract_coord(block, "start", 1),
            ) {
                if sx < min_x {
                    min_x = sx;
                }
                if sx > max_x {
                    max_x = sx;
                }
                if sy < min_y {
                    min_y = sy;
                }
                if sy > max_y {
                    max_y = sy;
                }
                found = true;
            }
            if let (Some(ex), Some(ey)) = (
                extract_coord(block, "end", 0),
                extract_coord(block, "end", 1),
            ) {
                if ex < min_x {
                    min_x = ex;
                }
                if ex > max_x {
                    max_x = ex;
                }
                if ey < min_y {
                    min_y = ey;
                }
                if ey > max_y {
                    max_y = ey;
                }
            }
        }
        pos = abs + 1;
    }

    if found {
        ((max_x - min_x).abs(), (max_y - min_y).abs())
    } else {
        (0.0, 0.0) // Unknown
    }
}

fn extract_coord(block: &str, keyword: &str, index: usize) -> Option<f64> {
    let pat = format!("({} ", keyword);
    let pos = block.find(&pat)? + pat.len();
    let rest = &block[pos..];
    let parts: Vec<&str> = rest.split([' ', ')']).collect();
    parts.get(index)?.parse().ok()
}

#[cfg(test)]
mod net_track_count_tests {
    use super::*;
    use konnect_sexp::parser::parse_sexp;

    /// KiCad 10 (file format 20260206) writes tab indentation, puts each
    /// `(segment …)` / `(via …)` on its own multi-line form, and has **no**
    /// top-level net table — every item names its net instead. The old
    /// substring probes (`"\n  (net "`, `"(segment "`, `"(via "`) match none of
    /// that, so a fully routed board reported net_count 0 / track_count 0 and
    /// still came back READY.
    const KICAD_10_BOARD: &str = "(kicad_pcb\n\
        \t(version 20260206)\n\
        \t(generator \"pcbnew\")\n\
        \t(segment\n\t\t(start 110 110)\n\t\t(end 120 110)\n\t\t(width 0.2)\n\t\t(layer \"F.Cu\")\n\t\t(net \"GND\")\n\t)\n\
        \t(segment\n\t\t(start 120 110)\n\t\t(end 130 120)\n\t\t(width 0.2)\n\t\t(layer \"F.Cu\")\n\t\t(net \"GND\")\n\t)\n\
        \t(via\n\t\t(at 130 120)\n\t\t(size 0.6)\n\t\t(drill 0.3)\n\t\t(net \"GND\")\n\t)\n\
        \t(segment\n\t\t(start 110 130)\n\t\t(end 120 130)\n\t\t(width 0.2)\n\t\t(layer \"B.Cu\")\n\t\t(net \"VCC\")\n\t)\n\
        )\n";

    /// KiCad ≤ 9: a top-level net table plus numeric references on the items.
    /// The same net must not be counted once for its declaration and again for
    /// every segment that mentions it.
    const KICAD_9_BOARD: &str = "(kicad_pcb\n\
        \t(version 20241229)\n\
        \t(net 0 \"\")\n\
        \t(net 1 \"GND\")\n\
        \t(net 2 \"VCC\")\n\
        \t(segment (start 110 110) (end 120 110) (width 0.2) (layer \"F.Cu\") (net 1))\n\
        \t(segment (start 120 110) (end 130 120) (width 0.2) (layer \"F.Cu\") (net 1))\n\
        \t(via (at 130 120) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net 1))\n\
        \t(segment (start 110 130) (end 120 130) (width 0.2) (layer \"B.Cu\") (net 2))\n\
        )\n";

    #[test]
    fn counts_a_kicad_10_board_with_no_top_level_net_table() {
        let tree = parse_sexp(KICAD_10_BOARD).unwrap();
        assert_eq!(count_nets_and_tracks(&tree), (2, 4));
    }

    #[test]
    fn counts_a_kicad_9_board_without_double_counting_declarations() {
        let tree = parse_sexp(KICAD_9_BOARD).unwrap();
        assert_eq!(count_nets_and_tracks(&tree), (2, 4));
    }

    #[test]
    fn the_unconnected_pseudo_net_does_not_count() {
        let tree = parse_sexp("(kicad_pcb\n\t(net 0 \"\")\n)\n").unwrap();
        assert_eq!(count_nets_and_tracks(&tree), (0, 0));
    }

    /// A zone outline may carry `(arc …)` inside its `(pts …)`; that is a
    /// polygon corner, not routed copper.
    #[test]
    fn zone_outline_arcs_are_not_routed_copper() {
        let board = "(kicad_pcb\n\
            \t(zone\n\t\t(net \"GND\")\n\t\t(polygon\n\t\t\t(pts\n\t\t\t\t(xy 0 0)\n\t\t\t\t(arc (start 1 0) (mid 2 1) (end 1 2))\n\t\t\t)\n\t\t)\n\t)\n\
            )\n";
        let tree = parse_sexp(board).unwrap();
        assert_eq!(count_nets_and_tracks(&tree), (1, 0));
    }

    // ─── End to end through the tool ─────────────────────────────────────────

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            crate::tools::ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        )
    }

    async fn validate(board_text: &str) -> serde_json::Value {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, board_text).unwrap();
        let result = handle_validate_for_manufacturing(
            &json!({ "board": board.to_str().unwrap() }),
            &test_ctx(),
        )
        .await
        .unwrap();
        match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => {
                serde_json::from_str(text).unwrap()
            }
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_routed_kicad_10_board_reports_its_nets_and_tracks() {
        let report = validate(KICAD_10_BOARD).await;
        assert_eq!(report["board_info"]["net_count"], json!(2));
        assert_eq!(report["board_info"]["track_count"], json!(4));
    }

    /// The symptom that surfaced this: an unrouted board came back READY on the
    /// routing check because both counts read zero, so the `net_count > 3 &&
    /// track_count == 0` guard could never fire.
    #[tokio::test]
    async fn an_unrouted_kicad_10_board_is_flagged_not_ready() {
        let board = "(kicad_pcb\n\
            \t(version 20260206)\n\
            \t(gr_line (start 0 0) (end 10 0) (layer \"Edge.Cuts\"))\n\
            \t(footprint \"R:R_0402\"\n\
            \t\t(pad \"1\" smd rect (at 0 0) (size 1 1) (net \"GND\"))\n\
            \t\t(pad \"2\" smd rect (at 1 0) (size 1 1) (net \"VCC\"))\n\
            \t)\n\
            \t(footprint \"R:R_0402\"\n\
            \t\t(pad \"1\" smd rect (at 5 0) (size 1 1) (net \"SDA\"))\n\
            \t\t(pad \"2\" smd rect (at 6 0) (size 1 1) (net \"SCL\"))\n\
            \t)\n\
            )\n";
        let report = validate(board).await;
        assert_eq!(report["board_info"]["net_count"], json!(4));
        assert_eq!(report["board_info"]["track_count"], json!(0));
        assert_eq!(report["verdict"], json!("NOT READY"));
        let issues = report["issues"].as_array().unwrap();
        assert!(
            issues.iter().any(|i| i["issue"]
                .as_str()
                .unwrap_or("")
                .contains("no traces routed")),
            "{issues:?}"
        );
    }
}

#[cfg(test)]
mod readiness_evidence_tests {
    use super::*;
    use serde_json::json;

    /// No kicad-cli, so DRC cannot run — the point of these tests.
    fn ctx_without_kicad_cli() -> ToolContext {
        ToolContext::new(
            crate::tools::ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        )
    }

    async fn validate(board_text: &str) -> serde_json::Value {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, board_text).unwrap();
        let result = handle_validate_for_manufacturing(
            &json!({ "board": board.to_str().unwrap() }),
            &ctx_without_kicad_cli(),
        )
        .await
        .unwrap();
        match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => {
                serde_json::from_str(text).unwrap()
            }
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// A board that passes every check this tool performs itself.
    const CLEAN_LOOKING_BOARD: &str = "(kicad_pcb\n\
        \t(version 20260206)\n\
        \t(generator \"pcbnew\")\n\
        \t(layers\n\t\t(0 \"F.Cu\" signal)\n\t\t(31 \"B.Cu\" signal)\n\t)\n\
        \t(gr_line (start 0 0) (end 50 0) (layer \"Edge.Cuts\") (width 0.1))\n\
        \t(footprint \"R:R_0402\"\n\
        \t\t(pad \"1\" smd rect (at 5 0) (size 1 1) (net \"SDA\"))\n\
        \t)\n\
        \t(segment (start 0 0) (end 5 0) (width 0.25) (layer \"F.Cu\") (net \"SDA\"))\n\
        )\n";

    /// #247. This tool returned `READY` with zero issues on a board carrying
    /// 25 DRC errors and an unrouted item, because it never asked DRC —
    /// its only routing check fires when a board has *no* tracks at all.
    ///
    /// The test context has no kicad-cli, so DRC cannot run. Missing evidence
    /// must block the verdict: "I found nothing wrong" is not the same claim
    /// as "nothing is wrong", and only one of them justifies ordering boards.
    #[tokio::test]
    async fn readiness_needs_drc_evidence_not_just_an_absence_of_findings() {
        let report = validate(CLEAN_LOOKING_BOARD).await;

        assert_ne!(
            report["verdict"], "READY",
            "a board whose DRC was never run cannot be declared ready: {report}"
        );
        assert!(report["drc"].is_null(), "no DRC ran, so no DRC summary");
        let issues = report["issues"].as_array().unwrap();
        assert!(
            issues.iter().any(|i| i["issue"]
                .as_str()
                .unwrap_or("")
                .contains("DRC could not run")),
            "the missing evidence must be named: {issues:?}"
        );
    }
}

#[cfg(test)]
mod copper_layer_count_tests {
    //! Issue #461: `estimate_cost` and `validate_for_manufacturing` counted
    //! copper layers by finding the substring `signal)` in the file text, so a
    //! board whose inner layers are `power`/`mixed`/`jumper` planes was quoted
    //! as a two-layer board while `get_board_info` on the same file said six.
    //!
    //! The fixture is pcbnew's own serialization of a six-layer board with two
    //! `power` planes and one `mixed` layer — provenance in
    //! `tests/fixtures/six_layer_power_planes_kicad10.README.md`. The old
    //! substring count reads it as 3.

    use super::*;
    use serde_json::json;

    const SIX_LAYER: &str =
        include_str!("../../tests/fixtures/six_layer_power_planes_kicad10.kicad_pcb");

    /// Two copper layers, both `signal` — the control the old scan got right.
    const TWO_LAYER: &str = "(kicad_pcb\n\
        \t(version 20260206)\n\
        \t(generator \"pcbnew\")\n\
        \t(layers\n\t\t(0 \"F.Cu\" signal)\n\t\t(31 \"B.Cu\" signal)\n\t)\n\
        \t(gr_line (start 0 0) (end 50 0) (layer \"Edge.Cuts\") (width 0.1))\n\
        )\n";

    fn ctx() -> ToolContext {
        ToolContext::new(
            crate::tools::ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        )
    }

    fn text_of(result: CallToolResult) -> serde_json::Value {
        match result.content.first() {
            Some(crate::mcp::protocol::ToolContent::Text { text }) => {
                serde_json::from_str(text).unwrap()
            }
            other => panic!("expected text content, got {other:?}"),
        }
    }

    async fn estimate(board_text: &str, extra: serde_json::Value) -> serde_json::Value {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, board_text).unwrap();
        let mut args =
            json!({ "board": board.to_str().unwrap(), "fab_house": "jlcpcb", "quantity": 5 });
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        text_of(handle_estimate_cost(&args, &ctx()).await.unwrap())
    }

    async fn validate(board_text: &str) -> serde_json::Value {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("board.kicad_pcb");
        std::fs::write(&board, board_text).unwrap();
        text_of(
            handle_validate_for_manufacturing(&json!({ "board": board.to_str().unwrap() }), &ctx())
                .await
                .unwrap(),
        )
    }

    /// The fixture really does defeat the substring scan; if a future KiCad
    /// resave changed that, this test would be proving nothing.
    #[test]
    fn the_fixture_defeats_a_substring_count() {
        let substring =
            SIX_LAYER.matches("signal)").count() + SIX_LAYER.matches("signal \"").count();
        assert_eq!(substring, 3, "fixture must carry non-signal copper");
        let tree = konnect_sexp::parser::parse_sexp(SIX_LAYER).unwrap();
        assert_eq!(konnect_sexp::layers::copper_layer_count(&tree), 6);
    }

    /// The reported case: a six-layer board priced as a six-layer board, with
    /// the quote's count and the board's count agreeing and no warning.
    #[tokio::test]
    async fn estimate_cost_prices_the_boards_declared_copper_count() {
        let report = estimate(SIX_LAYER, json!({})).await;
        assert_eq!(report["board"]["copper_layers"], 6, "{report}");
        assert_eq!(report["board"]["board_copper_layers"], 6);
        assert_eq!(
            report["cost_estimate"]["pcb_fabrication"], "$15.00",
            "JLCPCB six-layer price at quantity 5, not the two-layer $2.00: {report}"
        );
        assert_eq!(report["warnings"], json!([]));

        let control = estimate(TWO_LAYER, json!({})).await;
        assert_eq!(control["board"]["copper_layers"], 2);
        assert_eq!(control["cost_estimate"]["pcb_fabrication"], "$2.00");
    }

    /// A caller may quote at a different count, but the response must say the
    /// board disagrees rather than present the request as the board's fact.
    #[tokio::test]
    async fn estimate_cost_reports_a_layer_override_against_the_board() {
        let report = estimate(SIX_LAYER, json!({ "layers": 2 })).await;
        assert_eq!(report["board"]["copper_layers"], 2, "priced as asked");
        assert_eq!(
            report["board"]["board_copper_layers"], 6,
            "but the board says six"
        );
        assert_eq!(report["cost_estimate"]["pcb_fabrication"], "$2.00");
        let warnings = report["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 1, "{report}");
        let text = warnings[0].as_str().unwrap();
        assert!(
            text.contains("quoted at 2") && text.contains("declares 6"),
            "{text}"
        );

        // Asking for the count the board already has is not a discrepancy.
        let agree = estimate(SIX_LAYER, json!({ "layers": 6 })).await;
        assert_eq!(agree["warnings"], json!([]));
    }

    /// No `(layers …)` table: zero and a warning, never an invented two.
    #[tokio::test]
    async fn estimate_cost_does_not_invent_two_layers_for_a_board_without_a_table() {
        let report = estimate(
            "(kicad_pcb (version 20260206) (generator \"pcbnew\"))",
            json!({}),
        )
        .await;
        assert_eq!(report["board"]["copper_layers"], 0);
        assert_eq!(report["board"]["board_copper_layers"], 0);
        assert!(
            report["warnings"][0]
                .as_str()
                .unwrap()
                .contains("no copper layers"),
            "{report}"
        );
    }

    #[tokio::test]
    async fn validate_for_manufacturing_counts_copper_structurally() {
        let report = validate(SIX_LAYER).await;
        assert_eq!(report["board_info"]["copper_layers"], 6, "{report}");
        assert!(report["summary"]
            .as_str()
            .unwrap()
            .contains("6 copper layers"));

        let control = validate(TWO_LAYER).await;
        assert_eq!(control["board_info"]["copper_layers"], 2);
    }
}
