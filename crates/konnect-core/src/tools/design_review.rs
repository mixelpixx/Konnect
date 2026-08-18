//! `design_review` toolset — AI-powered design audits.
//!
//! Analyzes schematic and PCB files for common design issues. Returns structured
//! findings that Claude can explain, prioritize, and suggest fixes for.
//!
//! These tools work on the S-expression files directly — no KiCAD running required.

use super::cli;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, project_name_for, sch_hierarchy, ToolContext, ToolDef};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    parser::parse_sexp,
    schematic::{
        extract_labels, extract_lib_pins, extract_lib_pins_for_unit, extract_symbol_instances,
        find_lib_symbol, pin_endpoint, read_schematic,
    },
};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tracing::info;

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "audit_decoupling",
            "Audit schematic connectivity between IC power nets and decoupling capacitors. \
             This does not inspect PCB placement distance; use PCB clearance/placement tools \
             for a physical review.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_audit_decoupling(args, ctx).await }
        ),
        tool!(
            "audit_connections",
            "Check for common connection mistakes: missing pull-ups on I2C/reset, \
             missing series resistors on LEDs, floating inputs, outputs shorted together.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_audit_connections(args, ctx).await }
        ),
        tool!(
            "audit_power_rails",
            "Check power rail integrity: missing bulk capacitance, no test points on power rails, \
             voltage regulator output caps missing.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_audit_power_rails(args, ctx).await }
        ),
        tool!(
            "audit_manufacturing",
            "DFM checks for the configured fab house: component spacing, silkscreen overlap, \
             via-in-pad, acid traps, board outline issues.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "fab_house": {
                        "type": "string",
                        "description": "Target manufacturer: 'jlcpcb' (default), 'pcbway', 'oshpark'",
                        "default": "jlcpcb"
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_audit_manufacturing(args, ctx).await }
        ),
        tool!(
            "run_design_review",
            "Run all available audit checks and produce a consolidated design review report. \
             Audits every reachable schematic sheet, and when a board is supplied also runs \
             KiCAD's DRC, folding its errors, unconnected items and schematic-parity findings \
             into the verdict. Reports status, coverage, and diagnostics. Returns an INCOMPLETE \
             verdict instead of approval when coverage is partial, when an audit failed, or \
             when DRC could not run. This is the tool to call when the user asks 'is my board \
             ready?' or 'review my design'.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "board": { "type": "string", "description": "Path to .kicad_pcb file (optional)" },
                    "severity_filter": {
                        "type": "string",
                        "description": "Minimum severity to include: 'error', 'warning' (default), 'info'",
                        "default": "warning"
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_run_design_review(args, ctx).await }
        ),
        tool!(
            "check_bom_health",
            "Analyze the BOM for supply chain risks: parts with no MPN, lifecycle warnings, \
             low stock, parts not available from preferred distributors.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_check_bom_health(args, ctx).await }
        ),
    ]
}

// ─── Audit types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
struct AuditFinding {
    severity: &'static str, // "error", "warning", "info"
    category: &'static str, // "decoupling", "connection", "power", "dfm", "bom"
    component: Option<String>,
    issue: String,
    recommendation: String,
}

// ─── Decoupling audit ────────────────────────────────────────────────────────

async fn handle_audit_decoupling(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    info!(schematic = %sch_path.display(), "[BETA] Running decoupling audit");
    let (content, tree) = read_schematic(&sch_path)?;

    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let mut findings = Vec::new();
    let mut pass_count = 0;
    let mut total_power_pins = 0;

    // Collect all capacitor references and their net connections
    let cap_nets = collect_capacitor_nets(&content, &instances, &lib_syms);

    // For each IC (non-passive, non-connector component), check power pins
    for inst in &instances {
        let lib_sym = find_lib_symbol(&lib_syms, inst);
        let lib_sym = match lib_sym {
            Some(s) => s,
            None => continue,
        };

        let pins = extract_lib_pins(lib_sym);
        let is_passive = inst.lib_id.contains("R_")
            || inst.lib_id.contains("C_")
            || inst.lib_id.contains("L_")
            || inst.lib_id.contains("D_");
        let is_connector = inst.lib_id.contains("Conn_")
            || inst.lib_id.contains("Jack")
            || inst.lib_id.contains("Header");

        if is_passive || is_connector {
            continue;
        }

        // Find power pins (power_in type, or named VCC/VDD/VBUS/3V3/etc.)
        for pin in &pins {
            let is_power_pin = is_power_pin_name(&pin.name);
            if !is_power_pin {
                continue;
            }
            total_power_pins += 1;

            // Get the endpoint position of this power pin
            let (px, py) = pin_endpoint(pin, inst.pin_transform());

            // Check if there's a capacitor connected to a net that this pin is on
            let pin_net = find_net_at_point(&content, px, py);

            let has_decoupling = if let Some(ref net) = pin_net {
                cap_nets.contains(net)
            } else {
                false
            };

            if has_decoupling {
                pass_count += 1;
            } else {
                findings.push(AuditFinding {
                    severity: "error",
                    category: "decoupling",
                    component: Some(inst.reference.clone()),
                    issue: format!(
                        "Power pin '{}' on {} has no decoupling capacitor{}",
                        pin.name,
                        inst.reference,
                        pin_net
                            .as_ref()
                            .map(|n| format!(" (net: {})", n))
                            .unwrap_or_default()
                    ),
                    recommendation: format!(
                        "Add a 100nF ceramic capacitor close to {} pin '{}'",
                        inst.reference, pin.name
                    ),
                });
            }
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "audit": "decoupling",
            "scope": "schematic_connectivity",
            "pcb_distance_checked": false,
            "findings": findings,
            "pass_count": pass_count,
            "total_power_pins": total_power_pins,
            "summary": format!(
                "{}/{} power pins have decoupling. {} issues found.",
                pass_count, total_power_pins, findings.len()
            )
        }))
        .unwrap(),
    ))
}

// ─── Connection audit ────────────────────────────────────────────────────────

async fn handle_audit_connections(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    info!(schematic = %sch_path.display(), "[BETA] Running connection audit");
    let (content, tree) = read_schematic(&sch_path)?;

    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let mut findings = Vec::new();

    for inst in &instances {
        let lib_sym = match find_lib_symbol(&lib_syms, inst) {
            Some(s) => s,
            None => continue,
        };

        let pins = extract_lib_pins(lib_sym);

        // Check for I2C pull-ups
        if has_i2c_pins(&pins) {
            let sda_pin = pins.iter().find(|p| p.name.to_uppercase().contains("SDA"));
            let scl_pin = pins.iter().find(|p| p.name.to_uppercase().contains("SCL"));

            for (pin_name, pin) in [("SDA", sda_pin), ("SCL", scl_pin)] {
                if let Some(pin) = pin {
                    let (px, py) = pin_endpoint(pin, inst.pin_transform());
                    let net = find_net_at_point(&content, px, py);
                    if let Some(ref net_name) = net {
                        if !has_pull_up_on_net(&content, &instances, &lib_syms, net_name) {
                            findings.push(AuditFinding {
                                severity: "warning",
                                category: "connection",
                                component: Some(inst.reference.clone()),
                                issue: format!(
                                    "I2C {} pin on {} (net: {}) has no pull-up resistor",
                                    pin_name, inst.reference, net_name
                                ),
                                recommendation: format!(
                                    "Add a 4.7k pull-up resistor from {} to VCC",
                                    net_name
                                ),
                            });
                        }
                    }
                }
            }
        }

        // Check for reset pins without pull-up
        for pin in &pins {
            let name_upper = pin.name.to_uppercase();
            if (name_upper.contains("RESET") || name_upper.contains("NRST") || name_upper == "RST")
                && !name_upper.contains("OUT")
            {
                let (px, py) = pin_endpoint(pin, inst.pin_transform());
                let net = find_net_at_point(&content, px, py);
                if let Some(ref net_name) = net {
                    if !has_pull_up_on_net(&content, &instances, &lib_syms, net_name) {
                        findings.push(AuditFinding {
                            severity: "warning",
                            category: "connection",
                            component: Some(inst.reference.clone()),
                            issue: format!(
                                "Reset pin '{}' on {} (net: {}) has no pull-up resistor",
                                pin.name, inst.reference, net_name
                            ),
                            recommendation: format!(
                                "Add a 10k pull-up resistor from {} to VCC with 100nF cap to GND",
                                net_name
                            ),
                        });
                    }
                }
            }
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "audit": "connections",
            "findings": findings,
            "summary": format!("{} connection issues found.", findings.len())
        }))
        .unwrap(),
    ))
}

// ─── Power rail audit ────────────────────────────────────────────────────────

async fn handle_audit_power_rails(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    info!(schematic = %sch_path.display(), "[BETA] Running power rail audit");
    let (content, tree) = read_schematic(&sch_path)?;

    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let mut findings = Vec::new();

    // Find all power nets (from power symbols and labels)
    let power_nets = collect_power_nets(&content);

    // Check each power net for bulk capacitance
    let cap_nets = collect_capacitor_nets(&content, &instances, &lib_syms);
    let bulk_cap_nets = collect_bulk_cap_nets(&content, &instances, &lib_syms);

    for net in &power_nets {
        if net.to_uppercase().contains("GND") || net.to_uppercase().contains("VSS") {
            continue; // Ground nets don't need caps
        }

        if !cap_nets.contains(net.as_str()) {
            findings.push(AuditFinding {
                severity: "error",
                category: "power",
                component: None,
                issue: format!("Power rail '{}' has no decoupling capacitors", net),
                recommendation: format!("Add at least one 100nF ceramic cap on the '{}' rail", net),
            });
        } else if !bulk_cap_nets.contains(net.as_str()) {
            findings.push(AuditFinding {
                severity: "warning",
                category: "power",
                component: None,
                issue: format!("Power rail '{}' has no bulk capacitance (>= 10uF)", net),
                recommendation: format!(
                    "Add a 10uF or larger electrolytic/ceramic cap on the '{}' rail near the power source",
                    net
                ),
            });
        }
    }

    // Check for test points on power rails
    let test_point_nets = collect_test_point_nets(&content, &instances);
    for net in &power_nets {
        if net.to_uppercase().contains("GND") {
            continue;
        }
        if !test_point_nets.contains(net.as_str()) {
            findings.push(AuditFinding {
                severity: "info",
                category: "power",
                component: None,
                issue: format!("Power rail '{}' has no test point", net),
                recommendation: format!("Add a test point on '{}' for debugging", net),
            });
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "audit": "power_rails",
            "power_nets": power_nets,
            "findings": findings,
            "summary": format!("{} power rail issues found across {} rails.", findings.len(), power_nets.len())
        }))
        .unwrap(),
    ))
}

// ─── Manufacturing audit ─────────────────────────────────────────────────────

async fn handle_audit_manufacturing(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let fab_house = args["fab_house"].as_str().unwrap_or("jlcpcb");
    info!(board = %board_path.display(), fab_house = %fab_house, "[BETA] Running DFM audit");

    let content = tokio::fs::read_to_string(&board_path).await?;
    let tree = parse_sexp(&content)?;

    let mut findings = Vec::new();

    // Get fab constraints for the target fab house
    let (min_trace, _min_space, _min_drill, _min_annular) = match fab_house {
        "jlcpcb" => (0.127, 0.127, 0.3, 0.13), // JLCPCB standard capability
        "pcbway" => (0.1, 0.1, 0.2, 0.1),
        "oshpark" => (0.152, 0.152, 0.254, 0.127), // 6mil/6mil
        _ => (0.15, 0.15, 0.3, 0.13),
    };

    // Check board outline exists
    let has_edge_cuts = content.contains("Edge.Cuts");
    if !has_edge_cuts {
        findings.push(AuditFinding {
            severity: "error",
            category: "dfm",
            component: None,
            issue: "No board outline found (Edge.Cuts layer is empty)".to_string(),
            recommendation: "Add a board outline on the Edge.Cuts layer using add_board_outline"
                .to_string(),
        });
    }

    // Check for footprints on both sides (assembly complexity)
    let fps = tree.find_all("footprint");
    let mut front_count = 0;
    let mut back_count = 0;
    for fp in &fps {
        if let Some(layer) = fp
            .find("layer")
            .and_then(|l| l.get(1))
            .and_then(|l| l.as_str())
        {
            if layer == "F.Cu" {
                front_count += 1;
            }
            if layer == "B.Cu" {
                back_count += 1;
            }
        }
    }
    if back_count > 0 && front_count > 0 {
        findings.push(AuditFinding {
            severity: "info",
            category: "dfm",
            component: None,
            issue: format!(
                "Components on both sides: {} front, {} back. This requires dual-side assembly.",
                front_count, back_count
            ),
            recommendation: "Verify your fab house supports dual-side assembly. JLCPCB charges extra for back-side SMT.".to_string(),
        });
    }

    // Check for silkscreen overlapping pads
    let silkscreen_issues = check_silkscreen_overlap(&content, &tree);
    findings.extend(silkscreen_issues);

    // Check design rules in setup section
    let _setup = tree.find("setup");
    if let Some(_setup) = _setup {
        // Check trace width
        if let Some(trace_min) = find_design_rule_value(&content, "min_trace_width") {
            if trace_min < min_trace {
                findings.push(AuditFinding {
                    severity: "error",
                    category: "dfm",
                    component: None,
                    issue: format!(
                        "Minimum trace width ({:.3}mm) is below {} capability ({:.3}mm)",
                        trace_min, fab_house, min_trace
                    ),
                    recommendation: format!(
                        "Increase minimum trace width to at least {:.3}mm",
                        min_trace
                    ),
                });
            }
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "audit": "manufacturing",
            "fab_house": fab_house,
            "components": { "front": front_count, "back": back_count },
            "findings": findings,
            "summary": format!("{} DFM issues found for {}.", findings.len(), fab_house)
        }))
        .unwrap(),
    ))
}

// ─── Unified design review ───────────────────────────────────────────────────

#[derive(Default)]
struct SchematicReviewCoverage {
    sheet_instances: usize,
    schematic_files: usize,
    symbol_instances: usize,
    resolved_symbols: usize,
    unresolved_symbols: usize,
    named_nets: usize,
    multi_unit_symbols: usize,
}

#[derive(Default)]
struct BoardReviewCoverage {
    footprints: usize,
    pads: usize,
    named_nets: usize,
}

struct AuditAggregate {
    name: &'static str,
    requested: usize,
    completed: usize,
    failed: usize,
    findings: Vec<Value>,
}

impl AuditAggregate {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            requested: 0,
            completed: 0,
            failed: 0,
            findings: Vec::new(),
        }
    }

    fn record(
        &mut self,
        source: &Path,
        result: anyhow::Result<CallToolResult>,
        diagnostics: &mut Vec<Value>,
    ) {
        self.requested += 1;
        match result {
            Ok(result) => match extract_findings(&result) {
                Ok(findings) => {
                    self.completed += 1;
                    self.findings
                        .extend(findings.into_iter().map(|mut finding| {
                            finding["audit"] = json!(self.name);
                            finding["source"] = json!(source.display().to_string());
                            finding
                        }));
                }
                Err(reason) => {
                    self.failed += 1;
                    diagnostics.push(json!({
                        "code": "invalid_audit_result",
                        "audit": self.name,
                        "source": source.display().to_string(),
                        "message": reason
                    }));
                }
            },
            Err(error) => {
                self.failed += 1;
                diagnostics.push(json!({
                    "code": "audit_failed",
                    "audit": self.name,
                    "source": source.display().to_string(),
                    "message": error.to_string()
                }));
            }
        }
    }

    fn status(&self) -> &'static str {
        if self.failed == 0 {
            "complete"
        } else if self.completed > 0 {
            "partial"
        } else {
            "failed"
        }
    }

    fn summary(&self) -> Value {
        json!({
            "status": self.status(),
            "requested": self.requested,
            "completed": self.completed,
            "failed": self.failed,
            "findings": self.findings.len()
        })
    }
}

fn collect_hierarchy_paths(
    path: &Path,
    node: &Value,
    sheet_instances: &mut usize,
    seen_files: &mut HashSet<PathBuf>,
    files: &mut Vec<PathBuf>,
    diagnostics: &mut Vec<Value>,
) {
    *sheet_instances += 1;
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if seen_files.insert(canonical) {
        files.push(path.to_path_buf());
    }

    if let Some(error) = node.get("error").and_then(Value::as_str) {
        diagnostics.push(json!({
            "code": "hierarchy_error",
            "source": path.display().to_string(),
            "message": error
        }));
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if let Some(children) = node.get("children").and_then(Value::as_array) {
        for child in children {
            let Some(file) = child.get("file").and_then(Value::as_str) else {
                diagnostics.push(json!({
                    "code": "hierarchy_error",
                    "source": path.display().to_string(),
                    "message": "hierarchy entry has no child file"
                }));
                continue;
            };
            collect_hierarchy_paths(
                &parent.join(file),
                child,
                sheet_instances,
                seen_files,
                files,
                diagnostics,
            );
        }
    }
}

fn inspect_schematic_coverage(
    path: &Path,
    coverage: &mut SchematicReviewCoverage,
    diagnostics: &mut Vec<Value>,
) {
    let (_, tree) = match read_schematic(path) {
        Ok(loaded) => loaded,
        Err(error) => {
            diagnostics.push(json!({
                "code": "schematic_parse_failed",
                "source": path.display().to_string(),
                "message": error.to_string()
            }));
            return;
        }
    };

    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|node| node.find_all("symbol"))
        .unwrap_or_default();
    coverage.symbol_instances += instances.len();
    coverage.named_nets += extract_labels(&tree)
        .into_iter()
        .map(|label| label.net)
        .collect::<HashSet<_>>()
        .len();

    let mut multi_unit_references = HashSet::new();
    for instance in &instances {
        let Some(lib_symbol) = find_lib_symbol(&lib_syms, instance) else {
            coverage.unresolved_symbols += 1;
            diagnostics.push(json!({
                "code": "unresolved_library_symbol",
                "source": path.display().to_string(),
                "reference": instance.reference,
                "library_symbol": instance.lib_symbol_name(),
                "message": "symbol was skipped by one or more design-review audits"
            }));
            continue;
        };
        coverage.resolved_symbols += 1;

        // The design-review call sites tracked by #182 still use the
        // unit-agnostic extractor. Until that issue lands, surface every such
        // component as partial coverage rather than pretending it was audited.
        if extract_lib_pins(lib_symbol).len()
            > extract_lib_pins_for_unit(lib_symbol, instance.unit).len()
            && multi_unit_references.insert(instance.reference.clone())
        {
            coverage.multi_unit_symbols += 1;
            diagnostics.push(json!({
                "code": "multi_unit_review_incomplete",
                "source": path.display().to_string(),
                "reference": instance.reference,
                "message": "design-review pin analysis is not unit-aware yet; tracked by issue #182"
            }));
        }
    }
}

fn inspect_board_coverage(path: &Path) -> anyhow::Result<BoardReviewCoverage> {
    let content = std::fs::read_to_string(path)?;
    let tree = parse_sexp(&content)?;
    Ok(BoardReviewCoverage {
        footprints: konnect_sexp::board::footprints(&tree).len(),
        // Pads are nested inside each footprint, so asking the root for them
        // returned 0 for every board this review has ever run on (#246).
        pads: konnect_sexp::board::count_pads(&tree),
        named_nets: konnect_sexp::net::count_distinct_nets(&tree),
    })
}

fn args_for_schematic(args: &Value, path: &Path) -> Value {
    let mut sheet_args = args.clone();
    sheet_args["schematic"] = json!(path.display().to_string());
    sheet_args
}

async fn handle_run_design_review(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    info!("[BETA] Running full design review");
    let severity_filter = args["severity_filter"].as_str().unwrap_or("warning");
    let min_rank = match severity_filter {
        "error" => 2,
        "warning" => 1,
        _ => 0,
    };

    let root_path = get_path(args, "schematic")?;
    let project_name = project_name_for(&root_path);
    let mut hierarchy_visited = HashSet::new();
    let hierarchy =
        sch_hierarchy::build_hierarchy_node(&root_path, &project_name, 0, &mut hierarchy_visited)?;

    let mut diagnostics = Vec::new();
    let mut schematic_coverage = SchematicReviewCoverage::default();
    let mut schematic_files = Vec::new();
    let mut seen_files = HashSet::new();
    collect_hierarchy_paths(
        &root_path,
        &hierarchy,
        &mut schematic_coverage.sheet_instances,
        &mut seen_files,
        &mut schematic_files,
        &mut diagnostics,
    );
    schematic_coverage.schematic_files = schematic_files.len();

    let mut audits = vec![
        AuditAggregate::new("decoupling"),
        AuditAggregate::new("connections"),
        AuditAggregate::new("power_rails"),
        AuditAggregate::new("bom_health"),
    ];

    for schematic_path in &schematic_files {
        inspect_schematic_coverage(schematic_path, &mut schematic_coverage, &mut diagnostics);
        let sheet_args = args_for_schematic(args, schematic_path);

        let result = handle_audit_decoupling(&sheet_args, ctx).await;
        audits[0].record(schematic_path, result, &mut diagnostics);
        let result = handle_audit_connections(&sheet_args, ctx).await;
        audits[1].record(schematic_path, result, &mut diagnostics);
        let result = handle_audit_power_rails(&sheet_args, ctx).await;
        audits[2].record(schematic_path, result, &mut diagnostics);
        let result = handle_check_bom_health(&sheet_args, ctx).await;
        audits[3].record(schematic_path, result, &mut diagnostics);
    }

    if schematic_coverage.symbol_instances == 0 {
        diagnostics.push(json!({
            "code": "zero_symbol_instances",
            "source": root_path.display().to_string(),
            "message": "no symbol instances were found in the schematic hierarchy"
        }));
    }
    if schematic_coverage.named_nets == 0 {
        diagnostics.push(json!({
            "code": "zero_named_nets",
            "source": root_path.display().to_string(),
            "message": "no named nets were found in the schematic hierarchy"
        }));
    }

    let mut board_coverage = None;
    let mut drc_summary = None;
    if let Some(board) = args["board"].as_str() {
        let board_path = PathBuf::from(board);
        match inspect_board_coverage(&board_path) {
            Ok(coverage) => {
                if coverage.footprints == 0 {
                    diagnostics.push(json!({
                        "code": "zero_footprints",
                        "source": board_path.display().to_string(),
                        "message": "the supplied board contains no footprints"
                    }));
                } else if coverage.pads == 0 {
                    // A board with footprints and no pads is not a design, it
                    // is a failed read. #185 added the zero-footprint and
                    // zero-net diagnostics and missed this one, so an
                    // impossible pad count was absorbed into a "complete"
                    // review that then said LOOKS GOOD (#246).
                    diagnostics.push(json!({
                        "code": "zero_pads",
                        "source": board_path.display().to_string(),
                        "message": format!(
                            "the supplied board has {} footprints but no pads; \
                             the board was not read correctly, so this review \
                             cannot speak for it",
                            coverage.footprints
                        )
                    }));
                }
                board_coverage = Some(coverage);
            }
            Err(error) => diagnostics.push(json!({
                "code": "board_parse_failed",
                "source": board_path.display().to_string(),
                "message": error.to_string()
            })),
        }

        let mut manufacturing = AuditAggregate::new("manufacturing");
        let result = handle_audit_manufacturing(args, ctx).await;
        manufacturing.record(&board_path, result, &mut diagnostics);
        audits.push(manufacturing);

        // KiCad's own DRC is the authority on whether a board is clean, and
        // this review never asked it. It ran four schematic audits plus a DFM
        // check, found nothing, and said LOOKS GOOD about a board carrying 25
        // DRC errors and an unrouted net (#247). A review that has not
        // consulted DRC has not reviewed the board.
        match cli::run_drc(&ctx.config.kicad_cli, &board_path, false).await {
            Ok(report) => {
                for missing in report.missing_categories() {
                    diagnostics.push(json!({
                        "code": "drc_category_not_reported",
                        "source": board_path.display().to_string(),
                        "message": format!(
                            "kicad-cli did not report '{missing}', so this review \
                             cannot speak to that class of problem"
                        )
                    }));
                }
                drc_summary = Some(json!({
                    "errors": report.error_count(),
                    "design_rule_violations": report.violations.len(),
                    "unconnected_items": report.unconnected_items.as_ref().map(Vec::len),
                    "schematic_parity": report.schematic_parity.as_ref().map(Vec::len),
                }));
                let mut drc = AuditAggregate::new("drc");
                drc.completed += 1;
                drc.findings.extend(report.all().map(|violation| {
                    json!({
                        "severity": violation.severity,
                        "category": "drc",
                        "rule": violation.rule,
                        "message": violation.description,
                        "location": violation.pos.as_ref().map(|p| json!({ "x": p.x, "y": p.y })),
                    })
                }));
                audits.push(drc);
            }
            Err(error) => diagnostics.push(json!({
                "code": "drc_unavailable",
                "source": board_path.display().to_string(),
                "message": format!(
                    "DRC could not run, so no verdict here accounts for design \
                     rule violations or unrouted nets: {error:#}"
                )
            })),
        }
    }

    let audit_summaries = audits
        .iter()
        .map(|audit| (audit.name.to_string(), audit.summary()))
        .collect::<serde_json::Map<_, _>>();
    let completed_audits = audits.iter().map(|audit| audit.completed).sum::<usize>();
    let failed_audits = audits.iter().map(|audit| audit.failed).sum::<usize>();
    let mut all_findings = audits
        .iter()
        .flat_map(|audit| audit.findings.iter().cloned())
        .collect::<Vec<_>>();

    // Collect and filter findings
    let mut error_count = 0;
    let mut warning_count = 0;
    let mut info_count = 0;

    for finding in &all_findings {
        match finding["severity"].as_str().unwrap_or("info") {
            "error" => error_count += 1,
            "warning" => warning_count += 1,
            _ => info_count += 1,
        }
    }
    all_findings.retain(|finding| {
        let rank = match finding["severity"].as_str().unwrap_or("info") {
            "error" => 2,
            "warning" => 1,
            _ => 0,
        };
        rank >= min_rank
    });

    // Sort by severity (errors first)
    all_findings.sort_by(|a, b| {
        let rank_a = match a["severity"].as_str().unwrap_or("") {
            "error" => 0,
            "warning" => 1,
            _ => 2,
        };
        let rank_b = match b["severity"].as_str().unwrap_or("") {
            "error" => 0,
            "warning" => 1,
            _ => 2,
        };
        rank_a.cmp(&rank_b)
    });

    let status = if completed_audits == 0 {
        "failed"
    } else if failed_audits > 0 || !diagnostics.is_empty() {
        "partial"
    } else {
        "complete"
    };
    let verdict = if status != "complete" {
        "INCOMPLETE — review could not evaluate the full design"
    } else if error_count > 0 {
        "NOT READY — critical issues must be fixed before manufacturing"
    } else if warning_count > 0 {
        "NEEDS ATTENTION — review warnings before manufacturing"
    } else {
        "LOOKS GOOD — no critical issues found"
    };

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "design_review": {
                "status": status,
                "verdict": verdict,
                "errors": error_count,
                "warnings": warning_count,
                "info": info_count,
                "severity_filter": severity_filter,
                "findings": all_findings,
                "audits": audit_summaries,
                "coverage": {
                    "schematic": {
                        "sheet_instances": schematic_coverage.sheet_instances,
                        "schematic_files": schematic_coverage.schematic_files,
                        "symbol_instances": schematic_coverage.symbol_instances,
                        "resolved_symbols": schematic_coverage.resolved_symbols,
                        "unresolved_symbols": schematic_coverage.unresolved_symbols,
                        "named_nets": schematic_coverage.named_nets,
                        "multi_unit_symbols": schematic_coverage.multi_unit_symbols
                    },
                    "board": board_coverage.map(|coverage| json!({
                        "footprints": coverage.footprints,
                        "pads": coverage.pads,
                        "named_nets": coverage.named_nets
                    }))
                },
                // Null when no board was supplied, or when DRC could not run
                // — in the latter case `diagnostics` says so and the verdict
                // is INCOMPLETE. Never zero: this review must not be able to
                // imply a clean board it did not check.
                "drc": drc_summary,
                "diagnostics": diagnostics
            }
        }))
        .unwrap(),
    ))
}

// ─── BOM health check ───────────────────────────────────────────────────────

async fn handle_check_bom_health(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    info!(schematic = %sch_path.display(), "[BETA] Running BOM health check");

    let sch = cse::Schematic::load(&sch_path)
        .map_err(|e| anyhow::anyhow!("Failed to load schematic: {e}"))?;

    let mut findings = Vec::new();
    let mut total_components = 0;
    let mut missing_mpn = 0;
    let mut missing_footprint = 0;
    let mut missing_value = 0;

    for sym in sch.symbols.iter() {
        let reference = match sym.reference() {
            Some(r) => r,
            None => continue,
        };

        // Skip power symbols and sub-units
        if reference.starts_with('#') {
            continue;
        }
        total_components += 1;

        let value = sym.value_str().unwrap_or("");

        // Check for missing value
        if value.is_empty() || value == "~" {
            missing_value += 1;
            findings.push(AuditFinding {
                severity: "warning",
                category: "bom",
                component: Some(reference.to_owned()),
                issue: format!("{} has no value assigned", reference),
                recommendation: "Set the component value (e.g., '100nF', '10k', 'STM32F411')"
                    .to_string(),
            });
        }

        // Check for missing footprint
        let footprint = sym.footprint().unwrap_or("");
        if footprint.is_empty() || footprint == "~" {
            missing_footprint += 1;
            findings.push(AuditFinding {
                severity: "error",
                category: "bom",
                component: Some(reference.to_owned()),
                issue: format!("{} ({}) has no footprint assigned", reference, value),
                recommendation: "Assign a footprint before PCB layout".to_string(),
            });
        }

        // Check for missing MPN (per-component check via properties)
        let has_mpn = sym.property("MPN").is_some() || sym.property("LCSC").is_some();
        if reference.starts_with('U') && !has_mpn {
            missing_mpn += 1;
            findings.push(AuditFinding {
                severity: "warning",
                category: "bom",
                component: Some(reference.to_owned()),
                issue: format!(
                    "{} ({}) has no MPN (Manufacturer Part Number)",
                    reference, value
                ),
                recommendation:
                    "Add an MPN property for accurate BOM generation and supply chain lookup"
                        .to_string(),
            });
        }
    }

    Ok(CallToolResult::text(
        serde_json::to_string(&json!({
            "audit": "bom_health",
            "total_components": total_components,
            "missing_mpn": missing_mpn,
            "missing_footprint": missing_footprint,
            "missing_value": missing_value,
            "findings": findings,
            "summary": format!(
                "{} components, {} issues. {} missing footprints, {} missing values.",
                total_components, findings.len(), missing_footprint, missing_value
            )
        }))
        .unwrap(),
    ))
}

// ─── Helper functions ────────────────────────────────────────────────────────

fn is_power_pin_name(name: &str) -> bool {
    let upper = name.to_uppercase();
    upper.starts_with("VCC")
        || upper.starts_with("VDD")
        || upper.starts_with("VBUS")
        || upper.starts_with("V+")
        || upper.starts_with("VIN")
        || upper.starts_with("3V3")
        || upper.starts_with("5V")
        || upper.starts_with("1V")
        || upper.starts_with("2V")
        || upper == "AVCC"
        || upper == "AVDD"
        || upper == "DVCC"
        || upper == "DVDD"
        || upper.starts_with("VCAP")
        || upper.starts_with("VREF")
        || upper.contains("POWER")
        || upper.contains("PWR")
}

fn has_i2c_pins(pins: &[konnect_sexp::schematic::LibPin]) -> bool {
    let names: Vec<String> = pins.iter().map(|p| p.name.to_uppercase()).collect();
    names.iter().any(|n| n.contains("SDA")) && names.iter().any(|n| n.contains("SCL"))
}

/// Collect nets that have at least one capacitor connected.
fn collect_capacitor_nets(
    content: &str,
    instances: &[konnect_sexp::schematic::SymbolInstance],
    lib_syms: &[&konnect_sexp::parser::SexpNode],
) -> HashSet<String> {
    let mut nets = HashSet::new();
    for inst in instances {
        if !inst.reference.starts_with('C') || inst.reference.starts_with("CN") {
            continue; // Only capacitors
        }
        let lib_sym = find_lib_symbol(lib_syms, inst);
        if let Some(sym) = lib_sym {
            let pins = extract_lib_pins(sym);
            for pin in &pins {
                let (px, py) = pin_endpoint(pin, inst.pin_transform());
                if let Some(net) = find_net_at_point(content, px, py) {
                    nets.insert(net);
                }
            }
        }
    }
    nets
}

/// Collect nets that have bulk capacitors (>= 10uF).
fn collect_bulk_cap_nets(
    content: &str,
    instances: &[konnect_sexp::schematic::SymbolInstance],
    lib_syms: &[&konnect_sexp::parser::SexpNode],
) -> HashSet<String> {
    let mut nets = HashSet::new();
    for inst in instances {
        if !inst.reference.starts_with('C') || inst.reference.starts_with("CN") {
            continue;
        }
        // Check if value suggests bulk cap (>= 10uF)
        let val_upper = inst.value.to_uppercase();
        let is_bulk = val_upper.contains("10U")
            || val_upper.contains("22U")
            || val_upper.contains("47U")
            || val_upper.contains("100U")
            || val_upper.contains("220U")
            || val_upper.contains("470U")
            || val_upper.contains("1000U")
            || val_upper.contains("10µ")
            || val_upper.contains("22µ")
            || val_upper.contains("47µ");

        if !is_bulk {
            continue;
        }

        let lib_sym = find_lib_symbol(lib_syms, inst);
        if let Some(sym) = lib_sym {
            let pins = extract_lib_pins(sym);
            for pin in &pins {
                let (px, py) = pin_endpoint(pin, inst.pin_transform());
                if let Some(net) = find_net_at_point(content, px, py) {
                    nets.insert(net);
                }
            }
        }
    }
    nets
}

/// Collect power net names from power symbols and labels.
fn collect_power_nets(content: &str) -> Vec<String> {
    let mut nets = HashSet::new();

    // Find power symbols (reference starts with #PWR)
    // and net labels with power-like names
    let _search = content.as_bytes();
    let mut pos = 0;
    while pos < content.len() {
        // Look for plain labels with power-ish names. KiCAD's tag is `label`;
        // `net_label` is not in the schematic format and matched nothing.
        if let Some(label_pos) = content[pos..].find("(label \"") {
            let abs = pos + label_pos + 8;
            if let Some(end) = content[abs..].find('"') {
                let name = &content[abs..abs + end];
                if is_power_net_name(name) {
                    nets.insert(name.to_string());
                }
            }
            pos = abs + 1;
        } else {
            break;
        }
    }

    // Also check global labels
    pos = 0;
    while pos < content.len() {
        if let Some(label_pos) = content[pos..].find("(global_label \"") {
            let abs = pos + label_pos + 15;
            if let Some(end) = content[abs..].find('"') {
                let name = &content[abs..abs + end];
                if is_power_net_name(name) {
                    nets.insert(name.to_string());
                }
            }
            pos = abs + 1;
        } else {
            break;
        }
    }

    // Also check power_port symbols
    pos = 0;
    while pos < content.len() {
        if let Some(pp_pos) = content[pos..].find("(power_port \"") {
            let abs = pos + pp_pos + 13;
            if let Some(end) = content[abs..].find('"') {
                nets.insert(content[abs..abs + end].to_string());
            }
            pos = abs + 1;
        } else {
            break;
        }
    }

    nets.into_iter().collect()
}

fn is_power_net_name(name: &str) -> bool {
    let upper = name.to_uppercase();
    upper.starts_with("VCC")
        || upper.starts_with("VDD")
        || upper.starts_with("3V3")
        || upper.starts_with("5V")
        || upper.starts_with("12V")
        || upper.starts_with("VBUS")
        || upper == "GND"
        || upper == "DGND"
        || upper == "AGND"
        || upper.starts_with("+")
        || upper.starts_with("V+")
}

/// Collect nets that have test points connected.
fn collect_test_point_nets(
    _content: &str,
    instances: &[konnect_sexp::schematic::SymbolInstance],
) -> HashSet<String> {
    let nets = HashSet::new();
    for inst in instances {
        if inst.reference.starts_with("TP") || inst.value.to_uppercase().contains("TESTPOINT") {
            // Find the net this test point is on (simplified: look for nearby label)
            // A proper implementation would trace the wire
        }
    }
    nets
}

fn has_pull_up_on_net(
    content: &str,
    instances: &[konnect_sexp::schematic::SymbolInstance],
    lib_syms: &[&konnect_sexp::parser::SexpNode],
    net_name: &str,
) -> bool {
    // Check if any resistor has one pin on this net and the other on a power net
    for inst in instances {
        if !inst.reference.starts_with('R') {
            continue;
        }
        let lib_sym = find_lib_symbol(lib_syms, inst);
        if let Some(sym) = lib_sym {
            let pins = extract_lib_pins(sym);
            let pin_nets: Vec<Option<String>> = pins
                .iter()
                .map(|p| {
                    let (px, py) = pin_endpoint(p, inst.pin_transform());
                    find_net_at_point(content, px, py)
                })
                .collect();

            let has_target_net = pin_nets.iter().any(|n| n.as_deref() == Some(net_name));
            let has_power_net = pin_nets.iter().any(|n| {
                n.as_ref()
                    .map(|name| is_power_net_name(name))
                    .unwrap_or(false)
            });
            if has_target_net && has_power_net {
                return true;
            }
        }
    }
    false
}

/// Find the net name at a given schematic point by checking nearby labels and wires.
fn find_net_at_point(content: &str, x: f64, y: f64) -> Option<String> {
    // Check plain labels near this point (KiCAD's tag is `label`, not
    // `net_label` — the latter matched nothing in any real schematic).
    let tolerance = 0.5; // mm
    let mut search = 0;
    while let Some(pos) = content[search..].find("(label \"") {
        let abs = search + pos;
        let after = &content[abs + 8..];
        let name_end = after.find('"')?;
        let name = &after[..name_end];

        // Find the (at X Y) in this label
        let block_end = content[abs..].find(")\n").unwrap_or(200) + abs;
        let block = &content[abs..block_end.min(content.len())];
        if let Some(at_pos) = block.find("(at ") {
            let at_str = &block[at_pos + 4..];
            let parts: Vec<&str> = at_str.split([' ', ')']).take(2).collect();
            if parts.len() >= 2 {
                if let (Ok(lx), Ok(ly)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                    if (lx - x).abs() < tolerance && (ly - y).abs() < tolerance {
                        return Some(name.to_string());
                    }
                }
            }
        }
        search = abs + 1;
    }

    // Also check global_labels
    search = 0;
    while let Some(pos) = content[search..].find("(global_label \"") {
        let abs = search + pos;
        let after = &content[abs + 15..];
        let name_end = after.find('"')?;
        let name = &after[..name_end];

        let block_end = content[abs..].find(")\n").unwrap_or(200) + abs;
        let block = &content[abs..block_end.min(content.len())];
        if let Some(at_pos) = block.find("(at ") {
            let at_str = &block[at_pos + 4..];
            let parts: Vec<&str> = at_str.split([' ', ')']).take(2).collect();
            if parts.len() >= 2 {
                if let (Ok(lx), Ok(ly)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                    if (lx - x).abs() < tolerance && (ly - y).abs() < tolerance {
                        return Some(name.to_string());
                    }
                }
            }
        }
        search = abs + 1;
    }

    None
}

fn check_silkscreen_overlap(
    _content: &str,
    tree: &konnect_sexp::parser::SexpNode,
) -> Vec<AuditFinding> {
    // Simplified check: look for footprints that are very close together
    // A full implementation would check bounding boxes of silkscreen elements
    let fps = tree.find_all("footprint");
    let mut findings = Vec::new();

    let positions: Vec<(String, f64, f64)> = fps
        .iter()
        .filter_map(|fp| {
            let reference = fp
                .find_all("property")
                .iter()
                .find(|p| p.get(1).and_then(|n| n.as_str()) == Some("Reference"))
                .and_then(|p| p.get(2))
                .and_then(|n| n.as_str())?
                .to_string();
            let at = fp.find("at")?;
            let x = at.get_f64(1)?;
            let y = at.get_f64(2)?;
            Some((reference, x, y))
        })
        .collect();

    for i in 0..positions.len() {
        for j in (i + 1)..positions.len() {
            let (ref ref_a, xa, ya) = positions[i];
            let (ref ref_b, xb, yb) = positions[j];
            let dist = ((xa - xb).powi(2) + (ya - yb).powi(2)).sqrt();
            if dist < 1.0 {
                // Less than 1mm apart
                findings.push(AuditFinding {
                    severity: "warning",
                    category: "dfm",
                    component: Some(format!("{}, {}", ref_a, ref_b)),
                    issue: format!(
                        "{} and {} are only {:.2}mm apart — possible silkscreen overlap or assembly issue",
                        ref_a, ref_b, dist
                    ),
                    recommendation: "Increase spacing between components to at least 1mm for reliable assembly".to_string(),
                });
            }
        }
    }
    findings
}

fn find_design_rule_value(content: &str, rule_name: &str) -> Option<f64> {
    let pat = format!("({} ", rule_name);
    let pos = content.find(&pat)?;
    let after = &content[pos + pat.len()..];
    let end = after.find(')')?;
    after[..end].trim().parse().ok()
}

fn extract_findings(result: &CallToolResult) -> Result<Vec<Value>, String> {
    let text = match result.content.first() {
        Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
        Some(_) => return Err("audit returned non-text content".to_string()),
        None => return Err("audit returned no content".to_string()),
    };
    if result.is_error {
        return Err(format!("audit returned an error result: {text}"));
    }

    let parsed = serde_json::from_str::<Value>(text)
        .map_err(|error| format!("audit result was not valid JSON: {error}"))?;
    let findings = parsed
        .get("findings")
        .and_then(Value::as_array)
        .ok_or_else(|| "audit result did not contain a findings array".to_string())?;
    if findings.iter().any(|finding| !finding.is_object()) {
        return Err("audit findings must be JSON objects".to_string());
    }
    Ok(findings.clone())
}

#[cfg(test)]
mod review_completion_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use konnect_sexp::schematic::HierarchicalSheetSpec;
    use konnect_sexp::schematic::{format_blank_schematic, format_hierarchical_sheet};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    fn single_unit_schematic(footprint: &str) -> String {
        format!(
            r#"(kicad_sch
  (version 20250610)
  (generator "konnect")
  (generator_version "10.0")
  (uuid "11111111-1111-4111-8111-111111111111")
  (paper "A4")
  (lib_symbols
    (symbol "Device:R"
      (property "Reference" "R" (at 0 0 0))
      (property "Value" "R" (at 0 0 0))
      (symbol "R_1_1"
        (pin passive line (at 0 0 0) (length 2.54) (name "~") (number "1"))
      )
    )
  )
  (label "SIG" (at 20 20 0) (uuid "22222222-2222-4222-8222-222222222222"))
  (symbol
    (lib_id "Device:R")
    (at 20 20 0)
    (unit 1)
    (in_bom yes)
    (on_board yes)
    (dnp no)
    (uuid "33333333-3333-4333-8333-333333333333")
    (property "Reference" "R1" (at 20 20 0))
    (property "Value" "10k" (at 20 20 0))
    (property "Footprint" "{footprint}" (at 20 20 0))
    (pin "1" (uuid "44444444-4444-4444-8444-444444444444"))
  )
)
"#
        )
    }

    fn multi_unit_schematic() -> String {
        r#"(kicad_sch
  (version 20250610)
  (generator "konnect")
  (generator_version "10.0")
  (uuid "11111111-1111-4111-8111-111111111111")
  (paper "A4")
  (lib_symbols
    (symbol "Logic:DUAL"
      (property "Reference" "U" (at 0 0 0))
      (property "Value" "DUAL" (at 0 0 0))
      (symbol "DUAL_1_1"
        (pin input line (at 0 0 0) (length 2.54) (name "A") (number "1"))
      )
      (symbol "DUAL_2_1"
        (pin output line (at 0 0 0) (length 2.54) (name "Y") (number "2"))
      )
    )
  )
  (label "SIG" (at 20 20 0) (uuid "22222222-2222-4222-8222-222222222222"))
  (symbol
    (lib_id "Logic:DUAL")
    (at 20 20 0)
    (unit 1)
    (in_bom yes)
    (on_board yes)
    (dnp no)
    (uuid "33333333-3333-4333-8333-333333333333")
    (property "Reference" "U1" (at 20 20 0))
    (property "Value" "DUAL" (at 20 20 0))
    (property "Footprint" "Package_DIP:DIP-8_W7.62mm" (at 20 20 0))
    (property "MPN" "TEST-DUAL" (at 20 20 0))
    (pin "1" (uuid "44444444-4444-4444-8444-444444444444"))
  )
)
"#
        .to_string()
    }

    fn unresolved_symbol_schematic() -> String {
        r#"(kicad_sch
  (version 20250610)
  (generator "konnect")
  (generator_version "10.0")
  (uuid "11111111-1111-4111-8111-111111111111")
  (paper "A4")
  (lib_symbols)
  (label "SIG" (at 20 20 0) (uuid "22222222-2222-4222-8222-222222222222"))
  (symbol
    (lib_id "Missing:Part")
    (at 20 20 0)
    (unit 1)
    (in_bom yes)
    (on_board yes)
    (dnp no)
    (uuid "33333333-3333-4333-8333-333333333333")
    (property "Reference" "R1" (at 20 20 0))
    (property "Value" "10k" (at 20 20 0))
    (property "Footprint" "Resistor_SMD:R_0603_1608Metric" (at 20 20 0))
  )
)
"#
        .to_string()
    }

    fn root_with_child(file: &str) -> String {
        let mut root = format_blank_schematic();
        let insert_at = root.rfind(')').expect("blank schematic has a root close");
        let block = format_hierarchical_sheet(HierarchicalSheetSpec {
            name: "Power",
            file,
            x: 20.0,
            y: 20.0,
            width: 80.0,
            height: 50.0,
            project_name: "root",
            parent_instance_path: "/11111111-1111-4111-8111-111111111111",
            page: "2",
        });
        root.insert_str(insert_at, &block);
        root
    }

    fn review_json(result: CallToolResult) -> Value {
        assert!(
            !result.is_error,
            "incomplete review is a verdict, not a tool error"
        );
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("review result must be text JSON")
        };
        serde_json::from_str(text).expect("review result must be valid JSON")
    }

    async fn review(schematic: &Path, board: Option<&Path>) -> Value {
        let mut args = json!({"schematic": schematic.display().to_string()});
        if let Some(board) = board {
            args["board"] = json!(board.display().to_string());
        }
        review_json(handle_run_design_review(&args, &test_ctx()).await.unwrap())
    }

    #[test]
    fn audit_errors_and_shape_mismatches_are_not_empty_successes() {
        assert!(extract_findings(&CallToolResult::error("boom")).is_err());
        assert!(extract_findings(&CallToolResult::text("{}"))
            .unwrap_err()
            .contains("findings array"));
        assert!(
            extract_findings(&CallToolResult::text(r#"{"findings":["not an object"]}"#))
                .unwrap_err()
                .contains("JSON objects")
        );
    }

    #[tokio::test]
    async fn blank_schematic_is_incomplete_instead_of_looking_good() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("blank.kicad_sch");
        std::fs::write(&root, format_blank_schematic()).unwrap();

        let result = review(&root, None).await;
        let report = &result["design_review"];
        assert_eq!(report["status"], "partial");
        assert_eq!(
            report["verdict"],
            "INCOMPLETE — review could not evaluate the full design"
        );
        assert_eq!(report["coverage"]["schematic"]["symbol_instances"], 0);
        assert!(report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "zero_symbol_instances"));
        assert!(report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "zero_named_nets"));
    }

    #[tokio::test]
    async fn hierarchical_child_is_included_in_coverage_and_audits() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root.kicad_sch");
        let child = tmp.path().join("power.kicad_sch");
        std::fs::write(&root, root_with_child("power.kicad_sch")).unwrap();
        std::fs::write(&child, single_unit_schematic("")).unwrap();

        let result = review(&root, None).await;
        let report = &result["design_review"];
        assert_eq!(report["coverage"]["schematic"]["sheet_instances"], 2);
        assert_eq!(report["coverage"]["schematic"]["schematic_files"], 2);
        assert_eq!(report["coverage"]["schematic"]["symbol_instances"], 1);
        assert_ne!(report["verdict"], "LOOKS GOOD — no critical issues found");
        assert!(report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["source"] == child.display().to_string()));
    }

    #[tokio::test]
    async fn clean_single_sheet_can_still_look_good() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("clean.kicad_sch");
        std::fs::write(
            &root,
            single_unit_schematic("Resistor_SMD:R_0603_1608Metric"),
        )
        .unwrap();

        let result = review(&root, None).await;
        let report = &result["design_review"];
        assert_eq!(report["status"], "complete");
        assert_eq!(report["verdict"], "LOOKS GOOD — no critical issues found");
        assert_eq!(report["coverage"]["schematic"]["symbol_instances"], 1);
        assert_eq!(report["coverage"]["schematic"]["named_nets"], 1);
    }

    #[tokio::test]
    async fn multi_unit_symbol_is_partial_until_issue_182_lands() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("multi.kicad_sch");
        std::fs::write(&root, multi_unit_schematic()).unwrap();

        let result = review(&root, None).await;
        let report = &result["design_review"];
        assert_eq!(report["status"], "partial");
        assert_eq!(report["coverage"]["schematic"]["multi_unit_symbols"], 1);
        assert!(report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "multi_unit_review_incomplete"));
    }

    #[tokio::test]
    async fn unresolved_library_symbol_is_reported_as_partial_coverage() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("unresolved.kicad_sch");
        std::fs::write(&root, unresolved_symbol_schematic()).unwrap();

        let result = review(&root, None).await;
        let report = &result["design_review"];
        assert_eq!(report["status"], "partial");
        assert_eq!(report["coverage"]["schematic"]["unresolved_symbols"], 1);
        assert!(report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "unresolved_library_symbol"));
    }

    #[tokio::test]
    async fn supplied_board_with_zero_footprints_is_incomplete() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("clean.kicad_sch");
        let board = tmp.path().join("empty.kicad_pcb");
        std::fs::write(
            &root,
            single_unit_schematic("Resistor_SMD:R_0603_1608Metric"),
        )
        .unwrap();
        std::fs::write(
            &board,
            "(kicad_pcb (version 20250610) (generator pcbnew) (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal)))",
        )
        .unwrap();

        let result = review(&root, Some(&board)).await;
        let report = &result["design_review"];
        assert_eq!(report["status"], "partial");
        assert_eq!(report["coverage"]["board"]["footprints"], 0);
        assert!(report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "zero_footprints"));
    }

    /// A board KiCad wrote, in KiCad's own tab-indented layout, with pads
    /// where pads actually live: nested inside each `(footprint …)`.
    fn board_with_two_pads() -> &'static str {
        "(kicad_pcb\n\
         \t(version 20260206)\n\
         \t(generator \"pcbnew\")\n\
         \t(layers\n\t\t(0 \"F.Cu\" signal)\n\t\t(31 \"B.Cu\" signal)\n\t)\n\
         \t(footprint \"Resistor_SMD:R_0603_1608Metric\"\n\
         \t\t(layer \"F.Cu\")\n\
         \t\t(at 10 10)\n\
         \t\t(pad \"1\" smd roundrect\n\t\t\t(at -0.8 0)\n\t\t\t(size 0.9 0.95)\n\t\t)\n\
         \t\t(pad \"2\" smd roundrect\n\t\t\t(at 0.8 0)\n\t\t\t(size 0.9 0.95)\n\t\t)\n\
         \t)\n\
         )"
    }

    /// #247: this review never consulted DRC. It ran four schematic audits
    /// plus a DFM check, found nothing, and said `LOOKS GOOD` about a board
    /// carrying 25 DRC errors and an unrouted net.
    ///
    /// The test context has no kicad-cli, so DRC cannot run — which is the
    /// case that matters: missing evidence must produce `INCOMPLETE`, not a
    /// verdict that quietly means "clean except for everything I didn't
    /// check".
    #[tokio::test]
    async fn a_board_review_without_drc_evidence_cannot_look_good() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("clean.kicad_sch");
        let board = tmp.path().join("two_pads.kicad_pcb");
        std::fs::write(
            &root,
            single_unit_schematic("Resistor_SMD:R_0603_1608Metric"),
        )
        .unwrap();
        std::fs::write(&board, board_with_two_pads()).unwrap();

        let result = review(&root, Some(&board)).await;
        let report = &result["design_review"];

        assert_eq!(report["status"], "partial");
        assert_eq!(
            report["verdict"],
            "INCOMPLETE — review could not evaluate the full design"
        );
        assert!(report["drc"].is_null(), "no DRC ran, so no DRC summary");
        assert!(report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "drc_unavailable"));
    }

    /// A schematic-only review is unaffected: DRC is required when a board is
    /// in scope, not always. Otherwise this change would make every
    /// schematic review permanently INCOMPLETE.
    #[tokio::test]
    async fn a_schematic_only_review_does_not_need_drc() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("clean.kicad_sch");
        std::fs::write(
            &root,
            single_unit_schematic("Resistor_SMD:R_0603_1608Metric"),
        )
        .unwrap();

        let result = review(&root, None).await;
        let report = &result["design_review"];
        assert_eq!(report["status"], "complete");
        assert!(!report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "drc_unavailable"));
    }

    /// #246: `find_all` is direct-children-only and pads are nested, so the
    /// old `tree.find_all("pad")` on the board root reported 0 for every board
    /// ever reviewed — including this one, which plainly has two.
    #[tokio::test]
    async fn board_coverage_counts_pads_inside_footprints() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("clean.kicad_sch");
        let board = tmp.path().join("two_pads.kicad_pcb");
        std::fs::write(
            &root,
            single_unit_schematic("Resistor_SMD:R_0603_1608Metric"),
        )
        .unwrap();
        std::fs::write(&board, board_with_two_pads()).unwrap();

        let result = review(&root, Some(&board)).await;
        let report = &result["design_review"];
        assert_eq!(report["coverage"]["board"]["footprints"], 1);
        assert_eq!(report["coverage"]["board"]["pads"], 2);
        assert!(
            !report["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["code"] == "zero_pads"),
            "a board with pads must not be flagged as unread: {report}"
        );
    }

    /// Footprints but no pads is not a design, it is a failed read — and #185's
    /// coverage guard checked footprints and nets but never pads, so the
    /// impossible count passed straight through to a `LOOKS GOOD` verdict.
    #[tokio::test]
    async fn a_board_whose_footprints_have_no_pads_cannot_be_reviewed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("clean.kicad_sch");
        let board = tmp.path().join("padless.kicad_pcb");
        std::fs::write(
            &root,
            single_unit_schematic("Resistor_SMD:R_0603_1608Metric"),
        )
        .unwrap();
        std::fs::write(
            &board,
            "(kicad_pcb\n\
             \t(version 20260206)\n\
             \t(generator \"pcbnew\")\n\
             \t(footprint \"Resistor_SMD:R_0603_1608Metric\"\n\
             \t\t(layer \"F.Cu\")\n\
             \t\t(at 10 10)\n\
             \t)\n\
             )",
        )
        .unwrap();

        let result = review(&root, Some(&board)).await;
        let report = &result["design_review"];
        assert_eq!(report["coverage"]["board"]["footprints"], 1);
        assert_eq!(report["coverage"]["board"]["pads"], 0);
        assert_eq!(report["status"], "partial");
        assert_ne!(report["verdict"], "LOOKS GOOD — no critical issues found");
        assert!(report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "zero_pads"));
    }
}
