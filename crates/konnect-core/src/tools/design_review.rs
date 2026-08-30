//! `design_review` toolset — AI-powered design audits.
//!
//! Analyzes schematic and PCB files for common design issues. Returns structured
//! findings that Claude can explain, prioritize, and suggest fixes for.
//!
//! These tools work on the S-expression files directly — no KiCAD running required.

use super::cli;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::sch_connectivity::{net_graph_for, NetGraph};
use crate::tools::{
    get_path, invalid_arg, placed_pins_by_reference, project_name_for, sch_hierarchy, ToolContext,
    ToolDef,
};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    parser::parse_sexp,
    schematic::{
        extract_all_net_labels, extract_lib_pins, extract_lib_pins_for_unit,
        extract_symbol_instances, extract_wires, find_lib_symbol, pin_endpoint, read_schematic,
        Label, LabelKind,
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
             for a physical review. Defaults to one file; set schematic_scope to 'hierarchy' \
             to audit every reachable sheet instance.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "schematic_scope": {
                        "type": "string",
                        "enum": ["file", "hierarchy"],
                        "description": "Audit only the supplied file (default) or every reachable hierarchy instance",
                        "default": "file"
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_audit_decoupling(args, ctx).await }
        ),
        tool!(
            "audit_connections",
            "Check for common connection mistakes: missing pull-ups on I2C/reset, \
             missing series resistors on LEDs, floating inputs, outputs shorted together. \
             Defaults to one file; set schematic_scope to 'hierarchy' to audit every \
             reachable sheet instance.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "schematic_scope": {
                        "type": "string",
                        "enum": ["file", "hierarchy"],
                        "description": "Audit only the supplied file (default) or every reachable hierarchy instance",
                        "default": "file"
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_audit_connections(args, ctx).await }
        ),
        tool!(
            "audit_power_rails",
            "Check power rail integrity: missing bulk capacitance, no test points on power rails, \
             voltage regulator output caps missing. Defaults to one file; set schematic_scope \
             to 'hierarchy' to audit every reachable sheet instance.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "schematic_scope": {
                        "type": "string",
                        "enum": ["file", "hierarchy"],
                        "description": "Audit only the supplied file (default) or every reachable hierarchy instance",
                        "default": "file"
                    }
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
             low stock, parts not available from preferred distributors. Defaults to one \
             file; set schematic_scope to 'hierarchy' to audit every reachable sheet instance.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "schematic_scope": {
                        "type": "string",
                        "enum": ["file", "hierarchy"],
                        "description": "Audit only the supplied file (default) or every reachable hierarchy instance",
                        "default": "file"
                    }
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

#[derive(Clone, Copy)]
enum StandaloneSchematicAudit {
    Decoupling,
    Connections,
    PowerRails,
    BomHealth,
}

impl StandaloneSchematicAudit {
    fn name(self) -> &'static str {
        match self {
            Self::Decoupling => "decoupling",
            Self::Connections => "connections",
            Self::PowerRails => "power_rails",
            Self::BomHealth => "bom_health",
        }
    }

    async fn run_file(self, args: &Value, ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
        match self {
            Self::Decoupling => handle_audit_decoupling_file(args, ctx).await,
            Self::Connections => handle_audit_connections_file(args, ctx).await,
            Self::PowerRails => handle_audit_power_rails_file(args, ctx).await,
            Self::BomHealth => handle_check_bom_health_file(args, ctx).await,
        }
    }
}

async fn handle_audit_decoupling(
    args: &Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    handle_scoped_schematic_audit(StandaloneSchematicAudit::Decoupling, args, ctx).await
}

async fn handle_audit_connections(
    args: &Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    handle_scoped_schematic_audit(StandaloneSchematicAudit::Connections, args, ctx).await
}

async fn handle_audit_power_rails(
    args: &Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    handle_scoped_schematic_audit(StandaloneSchematicAudit::PowerRails, args, ctx).await
}

async fn handle_check_bom_health(
    args: &Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    handle_scoped_schematic_audit(StandaloneSchematicAudit::BomHealth, args, ctx).await
}

// ─── Decoupling audit ────────────────────────────────────────────────────────

async fn handle_audit_decoupling_file(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    info!(schematic = %sch_path.display(), "[BETA] Running decoupling audit");
    let (_, tree) = read_schematic(&sch_path)?;

    let placed = placed_pins_by_reference(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_all_net_labels(&tree);
    let mut graph = net_graph_for(&tree, &wires, &labels);

    let mut findings = Vec::new();
    let mut pass_count = 0;
    let mut total_power_pins = 0;

    // Collect all capacitor references and their net connections
    let (cap_nets, _) = capacitor_nets(&mut graph, &placed);

    // For each IC (non-passive, non-connector component), check power pins
    for (inst, pins) in &placed {
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
        for (pin, transform) in pins {
            let is_power_pin = is_power_pin_name(&pin.name);
            if !is_power_pin {
                continue;
            }
            total_power_pins += 1;

            // Get the endpoint position of this power pin
            let (px, py) = pin_endpoint(pin, *transform);

            // Check if there's a capacitor connected to a net that this pin is on
            let pin_net = graph.net_at(px, py);

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

async fn handle_audit_connections_file(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    info!(schematic = %sch_path.display(), "[BETA] Running connection audit");
    let (_, tree) = read_schematic(&sch_path)?;

    let placed = placed_pins_by_reference(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_all_net_labels(&tree);
    let mut graph = net_graph_for(&tree, &wires, &labels);
    let pull_up_nets = pull_up_nets(&mut graph, &placed);

    let mut findings = Vec::new();

    for (inst, pins) in &placed {
        // Check for I2C pull-ups
        if has_i2c_pins(pins) {
            let named = |needle: &str| {
                pins.iter()
                    .find(|(pin, _)| pin.name.to_uppercase().contains(needle))
            };

            for (pin_name, placed_pin) in [("SDA", named("SDA")), ("SCL", named("SCL"))] {
                if let Some((pin, transform)) = placed_pin {
                    let (px, py) = pin_endpoint(pin, *transform);
                    let net = graph.net_at(px, py);
                    if let Some(ref net_name) = net {
                        if !pull_up_nets.contains(net_name) {
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
        for (pin, transform) in pins {
            let name_upper = pin.name.to_uppercase();
            if (name_upper.contains("RESET") || name_upper.contains("NRST") || name_upper == "RST")
                && !name_upper.contains("OUT")
            {
                let (px, py) = pin_endpoint(pin, *transform);
                let net = graph.net_at(px, py);
                if let Some(ref net_name) = net {
                    if !pull_up_nets.contains(net_name) {
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

async fn handle_audit_power_rails_file(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    info!(schematic = %sch_path.display(), "[BETA] Running power rail audit");
    let (_, tree) = read_schematic(&sch_path)?;

    let placed = placed_pins_by_reference(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_all_net_labels(&tree);
    let mut graph = net_graph_for(&tree, &wires, &labels);

    let mut findings = Vec::new();

    // Find all power nets (from power symbols and labels)
    let power_nets = collect_power_nets(&labels);

    // Check each power net for bulk capacitance
    let (cap_nets, bulk_cap_nets) = capacitor_nets(&mut graph, &placed);

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
    let test_point_nets = test_point_nets(&mut graph, &placed);
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

#[derive(Clone)]
struct SchematicSheetInstance {
    path: PathBuf,
    instance_path: String,
}

#[derive(Default)]
struct SchematicHierarchyTraversal {
    sheet_instances: usize,
    schematic_files: Vec<PathBuf>,
    auditable_instances: Vec<SchematicSheetInstance>,
    diagnostics: Vec<Value>,
    seen_files: HashSet<PathBuf>,
}

fn collect_hierarchy(
    path: &Path,
    instance_path: &str,
    node: &Value,
    traversal: &mut SchematicHierarchyTraversal,
) {
    traversal.sheet_instances += 1;
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if traversal.seen_files.insert(canonical) {
        traversal.schematic_files.push(path.to_path_buf());
    }

    let hierarchy_error = node.get("error").and_then(Value::as_str);
    if let Some(error) = hierarchy_error {
        traversal.diagnostics.push(json!({
            "code": "hierarchy_error",
            "source": path.display().to_string(),
            "sheet_instance_path": instance_path,
            "message": error
        }));
    } else {
        traversal.auditable_instances.push(SchematicSheetInstance {
            path: path.to_path_buf(),
            instance_path: instance_path.to_string(),
        });
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if let Some(children) = node.get("children").and_then(Value::as_array) {
        for (index, child) in children.iter().enumerate() {
            let Some(file) = child.get("file").and_then(Value::as_str) else {
                traversal.diagnostics.push(json!({
                    "code": "hierarchy_error",
                    "source": path.display().to_string(),
                    "sheet_instance_path": instance_path,
                    "message": "hierarchy entry has no child file"
                }));
                continue;
            };
            let child_uuid = child
                .get("uuid")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    traversal.diagnostics.push(json!({
                        "code": "hierarchy_error",
                        "source": path.display().to_string(),
                        "sheet_instance_path": instance_path,
                        "message": "hierarchy entry has no sheet UUID"
                    }));
                    format!("unknown-{index}")
                });
            collect_hierarchy(
                &parent.join(file),
                &format!("{instance_path}{child_uuid}/"),
                child,
                traversal,
            );
        }
    }
}

fn inspect_hierarchy(root_path: &Path) -> anyhow::Result<SchematicHierarchyTraversal> {
    let project_name = project_name_for(root_path);
    let mut hierarchy_visited = HashSet::new();
    let hierarchy =
        sch_hierarchy::build_hierarchy_node(root_path, &project_name, 0, &mut hierarchy_visited)?;
    let mut traversal = SchematicHierarchyTraversal::default();
    collect_hierarchy(root_path, "/", &hierarchy, &mut traversal);
    Ok(traversal)
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
    coverage.named_nets += extract_all_net_labels(&tree)
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

        // A count, not a diagnostic: the audits select the placed unit's own
        // pins, so a multi-unit component is reviewed like any other. It stays
        // in coverage because "which parts here have several units" is what a
        // reader checks this number against.
        if extract_lib_pins(lib_symbol).len()
            > extract_lib_pins_for_unit(lib_symbol, instance.unit).len()
            && multi_unit_references.insert(instance.reference.clone())
        {
            coverage.multi_unit_symbols += 1;
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

fn requested_schematic_scope(args: &Value) -> Result<&'static str, CallToolResult> {
    match args.get("schematic_scope") {
        None | Some(Value::Null) => Ok("file"),
        Some(Value::String(scope)) if scope == "file" => Ok("file"),
        Some(Value::String(scope)) if scope == "hierarchy" => Ok("hierarchy"),
        Some(Value::String(_)) => Err(invalid_arg(
            "schematic_scope",
            "expected 'file' or 'hierarchy'",
        )),
        Some(_) => Err(invalid_arg("schematic_scope", "expected a string")),
    }
}

fn audit_result_json(result: CallToolResult) -> anyhow::Result<Value> {
    let text = match result.content.first() {
        Some(crate::mcp::protocol::ToolContent::Text { text }) => text,
        Some(_) => anyhow::bail!("audit returned non-text content"),
        None => anyhow::bail!("audit returned no content"),
    };
    if result.is_error {
        anyhow::bail!("audit returned an error result: {text}");
    }
    let body: Value = serde_json::from_str(text)?;
    if !body.is_object() {
        anyhow::bail!("audit result was not a JSON object");
    }
    Ok(body)
}

fn schematic_symbol_count(path: &Path) -> anyhow::Result<usize> {
    let (_, tree) = read_schematic(path)?;
    Ok(extract_symbol_instances(&tree).len())
}

fn decorate_file_audit_result(body: &mut Value, symbol_instances: usize) {
    body["schematic_scope"] = json!("file");
    body["status"] = json!("complete");
    body["coverage"] = json!({
        "sheet_instances": 1,
        "audited_sheet_instances": 1,
        "schematic_files": 1,
        "symbol_instances": symbol_instances
    });
    body["diagnostics"] = json!([]);
}

fn sum_sheet_metric(sheet_results: &[Value], key: &str) -> u64 {
    sheet_results
        .iter()
        .filter_map(|result| result.get(key).and_then(Value::as_u64))
        .sum()
}

async fn handle_scoped_schematic_audit(
    audit: StandaloneSchematicAudit,
    args: &Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let schematic_scope = match requested_schematic_scope(args) {
        Ok(scope) => scope,
        Err(error) => return Ok(error),
    };
    let root_path = get_path(args, "schematic")?;

    if schematic_scope == "file" {
        let result = audit.run_file(args, ctx).await?;
        let mut body = audit_result_json(result)?;
        decorate_file_audit_result(&mut body, schematic_symbol_count(&root_path)?);
        return Ok(CallToolResult::text(serde_json::to_string(&body)?));
    }

    let traversal = inspect_hierarchy(&root_path)?;
    let sheet_instances = traversal.sheet_instances;
    let schematic_files = traversal.schematic_files.len();
    let mut diagnostics = traversal.diagnostics;
    let mut sheet_results = Vec::new();
    let mut findings = Vec::new();
    let mut symbol_instances = 0usize;

    for sheet in &traversal.auditable_instances {
        let sheet_args = args_for_schematic(args, &sheet.path);
        let result = match audit.run_file(&sheet_args, ctx).await {
            Ok(result) => result,
            Err(error) => {
                diagnostics.push(json!({
                    "code": "audit_failed",
                    "audit": audit.name(),
                    "source": sheet.path.display().to_string(),
                    "sheet_instance_path": sheet.instance_path,
                    "message": error.to_string()
                }));
                continue;
            }
        };
        let mut body = match audit_result_json(result) {
            Ok(body) => body,
            Err(error) => {
                diagnostics.push(json!({
                    "code": "invalid_audit_result",
                    "audit": audit.name(),
                    "source": sheet.path.display().to_string(),
                    "sheet_instance_path": sheet.instance_path,
                    "message": error.to_string()
                }));
                continue;
            }
        };
        let sheet_symbol_instances = match schematic_symbol_count(&sheet.path) {
            Ok(count) => count,
            Err(error) => {
                diagnostics.push(json!({
                    "code": "schematic_parse_failed",
                    "source": sheet.path.display().to_string(),
                    "sheet_instance_path": sheet.instance_path,
                    "message": error.to_string()
                }));
                continue;
            }
        };
        symbol_instances += sheet_symbol_instances;
        decorate_file_audit_result(&mut body, sheet_symbol_instances);
        body["source"] = json!(sheet.path.display().to_string());
        body["sheet_instance_path"] = json!(sheet.instance_path);
        if let Some(sheet_findings) = body.get_mut("findings").and_then(Value::as_array_mut) {
            for finding in sheet_findings {
                finding["source"] = json!(sheet.path.display().to_string());
                finding["sheet_instance_path"] = json!(sheet.instance_path);
                findings.push(finding.clone());
            }
        }
        sheet_results.push(body);
    }

    let status = if sheet_results.is_empty() {
        "failed"
    } else if diagnostics.is_empty() {
        "complete"
    } else {
        "partial"
    };
    let audited_sheet_instances = sheet_results.len();
    let finding_count = findings.len();
    let mut body = json!({
        "audit": audit.name(),
        "schematic_scope": "hierarchy",
        "status": status,
        "coverage": {
            "sheet_instances": sheet_instances,
            "audited_sheet_instances": audited_sheet_instances,
            "schematic_files": schematic_files,
            "symbol_instances": symbol_instances
        },
        "findings": findings,
        "sheet_results": sheet_results.clone(),
        "diagnostics": diagnostics,
        "summary": format!(
            "{} audit covered {}/{} sheet instances across {} schematic files; {} findings.",
            audit.name(),
            audited_sheet_instances,
            sheet_instances,
            schematic_files,
            finding_count
        )
    });

    match audit {
        StandaloneSchematicAudit::Decoupling => {
            // Preserve the pre-existing field that distinguishes connectivity
            // review from a physical PCB-distance check. `schematic_scope` is
            // the new file-versus-hierarchy contract.
            body["scope"] = json!("schematic_connectivity");
            body["pcb_distance_checked"] = json!(false);
            body["pass_count"] = json!(sum_sheet_metric(&sheet_results, "pass_count"));
            body["total_power_pins"] = json!(sum_sheet_metric(&sheet_results, "total_power_pins"));
        }
        StandaloneSchematicAudit::Connections => {}
        StandaloneSchematicAudit::PowerRails => {
            body["power_nets"] = json!(sheet_results
                .iter()
                .filter_map(|result| result.get("power_nets").and_then(Value::as_array))
                .flatten()
                .cloned()
                .collect::<Vec<_>>());
        }
        StandaloneSchematicAudit::BomHealth => {
            for key in [
                "total_components",
                "missing_mpn",
                "missing_footprint",
                "missing_value",
            ] {
                body[key] = json!(sum_sheet_metric(&sheet_results, key));
            }
        }
    }

    Ok(CallToolResult::text(serde_json::to_string(&body)?))
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
    let traversal = inspect_hierarchy(&root_path)?;
    let mut diagnostics = traversal.diagnostics;
    let mut schematic_coverage = SchematicReviewCoverage {
        sheet_instances: traversal.sheet_instances,
        schematic_files: traversal.schematic_files.len(),
        ..SchematicReviewCoverage::default()
    };
    for sheet in &traversal.auditable_instances {
        // A reused child file represents a distinct KiCad sheet instance each
        // time it appears. Count its symbols and named nets once per instance,
        // while the file-level audit calls below remain deduplicated.
        inspect_schematic_coverage(&sheet.path, &mut schematic_coverage, &mut diagnostics);
    }

    let mut audits = vec![
        AuditAggregate::new("decoupling"),
        AuditAggregate::new("connections"),
        AuditAggregate::new("power_rails"),
        AuditAggregate::new("bom_health"),
    ];

    for schematic_path in &traversal.schematic_files {
        let sheet_args = args_for_schematic(args, schematic_path);

        let result = handle_audit_decoupling_file(&sheet_args, ctx).await;
        audits[0].record(schematic_path, result, &mut diagnostics);
        let result = handle_audit_connections_file(&sheet_args, ctx).await;
        audits[1].record(schematic_path, result, &mut diagnostics);
        let result = handle_audit_power_rails_file(&sheet_args, ctx).await;
        audits[2].record(schematic_path, result, &mut diagnostics);
        let result = handle_check_bom_health_file(&sheet_args, ctx).await;
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
                        "items": violation.items,
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

async fn handle_check_bom_health_file(
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

fn has_i2c_pins(pins: &[PlacedPin]) -> bool {
    let names: Vec<String> = pins.iter().map(|(p, _)| p.name.to_uppercase()).collect();
    names.iter().any(|n| n.contains("SDA")) && names.iter().any(|n| n.contains("SCL"))
}

/// One pin of one placed unit, with the transform that put it on the sheet.
type PlacedPin = (
    konnect_sexp::schematic::LibPin,
    konnect_sexp::geometry::PinTransform,
);

/// A placed unit and the pins it draws — one entry of [`placed_pins_by_reference`].
type PlacedUnit = (konnect_sexp::schematic::SymbolInstance, Vec<PlacedPin>);

/// Every net a placed unit's pins reach.
///
/// Nets come from the shared net graph, so a pin reaches its net along the
/// wires and junctions it is drawn with, and a rail named by a power symbol is
/// a net like any other. The audits used to scan the file for a label within
/// 0.5 mm of the pin, which followed neither.
///
/// `pins` comes from [`placed_pins_by_reference`], so it holds this unit's pins
/// only: a triode of an ECC83 does not report the heater's net at its own
/// coordinates (#182).
fn pin_nets(graph: &mut NetGraph, pins: &[PlacedPin]) -> HashSet<String> {
    let mut nets = HashSet::new();
    for (pin, transform) in pins {
        let (px, py) = pin_endpoint(pin, *transform);
        if let Some(net) = graph.net_at(px, py) {
            nets.insert(net);
        }
    }
    nets
}

/// `C1` is a capacitor, `CN1` is a connector.
fn is_capacitor(inst: &konnect_sexp::schematic::SymbolInstance) -> bool {
    inst.reference.starts_with('C') && !inst.reference.starts_with("CN")
}

/// Whether a capacitor's value reads as bulk capacitance (>= 10uF).
///
/// The `10µ`/`22µ`/`47µ` arms never match — `to_uppercase` maps U+00B5 MICRO
/// SIGN to Greek `Μ`. Left as found: that is a value-parsing bug, not a
/// net-resolution one.
fn is_bulk_capacitor_value(value: &str) -> bool {
    let upper = value.to_uppercase();
    upper.contains("10U")
        || upper.contains("22U")
        || upper.contains("47U")
        || upper.contains("100U")
        || upper.contains("220U")
        || upper.contains("470U")
        || upper.contains("1000U")
        || upper.contains("10µ")
        || upper.contains("22µ")
        || upper.contains("47µ")
}

/// Nets with a capacitor on them, and the subset carrying bulk capacitance
/// (>= 10uF). One pass, since the second is a subset of the first.
fn capacitor_nets(
    graph: &mut NetGraph,
    placed: &[PlacedUnit],
) -> (HashSet<String>, HashSet<String>) {
    let mut all = HashSet::new();
    let mut bulk = HashSet::new();
    for (inst, pins) in placed.iter().filter(|(inst, _)| is_capacitor(inst)) {
        let nets = pin_nets(graph, pins);
        if is_bulk_capacitor_value(&inst.value) {
            bulk.extend(nets.iter().cloned());
        }
        all.extend(nets);
    }
    (all, bulk)
}

/// Nets with a test point on them.
fn test_point_nets(graph: &mut NetGraph, placed: &[PlacedUnit]) -> HashSet<String> {
    let is_test_point = |inst: &konnect_sexp::schematic::SymbolInstance| {
        inst.reference.starts_with("TP") || inst.value.to_uppercase().contains("TESTPOINT")
    };
    let mut nets = HashSet::new();
    for (_, pins) in placed.iter().filter(|(inst, _)| is_test_point(inst)) {
        nets.extend(pin_nets(graph, pins));
    }
    nets
}

/// Nets a resistor pulls up: one of its pins is on a power net, so the nets
/// its other pins reach are pulled to that rail.
///
/// Built once per sheet. Asking per pin instead re-walked every resistor on
/// the sheet for each I2C or reset pin found.
fn pull_up_nets(graph: &mut NetGraph, placed: &[PlacedUnit]) -> HashSet<String> {
    let mut pulled = HashSet::new();
    for (_, pins) in placed
        .iter()
        .filter(|(inst, _)| inst.reference.starts_with('R'))
    {
        let nets = pin_nets(graph, pins);
        if nets.iter().any(|net| is_power_net_name(net)) {
            pulled.extend(nets);
        }
    }
    pulled
}

/// The power rails on the sheet: every net a power symbol names, plus every
/// label whose name reads like a rail.
///
/// `labels` must be `extract_all_net_labels` — a `+3V3` symbol names a rail
/// exactly as a label does. `PWR_FLAG` is not a rail and does not appear: its
/// pin is `power_out`, which the extractor skips.
fn collect_power_nets(labels: &[Label]) -> Vec<String> {
    labels
        .iter()
        .filter(|label| label.kind == LabelKind::PowerSymbol || is_power_net_name(&label.net))
        .map(|label| label.net.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
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

/// A context with no `kicad_cli`, no board and no project — every field of
/// `ServerConfig` at its default. Both test modules below run tools that only
/// read a schematic off disk.
#[cfg(test)]
fn test_ctx() -> ToolContext {
    ToolContext::new(
        crate::tools::ServerConfig::default(),
        std::sync::Arc::new(crate::router::ToolRouter::new()),
    )
}

#[cfg(test)]
mod review_completion_tests {
    use super::*;
    use konnect_sexp::schematic::HierarchicalSheetSpec;
    use konnect_sexp::schematic::{format_blank_schematic, format_hierarchical_sheet};
    use tempfile::TempDir;

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

    fn root_with_reused_child(file: &str) -> String {
        let mut root = root_with_child(file);
        let insert_at = root.rfind(')').expect("blank schematic has a root close");
        let block = format_hierarchical_sheet(HierarchicalSheetSpec {
            name: "Power B",
            file,
            x: 120.0,
            y: 20.0,
            width: 80.0,
            height: 50.0,
            project_name: "root",
            parent_instance_path: "/11111111-1111-4111-8111-111111111111",
            page: "3",
        });
        root.insert_str(insert_at, &block);
        root
    }

    fn tool_json(result: CallToolResult) -> Value {
        assert!(!result.is_error, "audit must return a structured result");
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("audit result must be text JSON")
        };
        serde_json::from_str(text).expect("audit result must be valid JSON")
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

    #[test]
    fn every_standalone_schematic_audit_declares_hierarchy_scope() {
        for name in [
            "audit_decoupling",
            "audit_connections",
            "audit_power_rails",
            "check_bom_health",
        ] {
            let tool = tools()
                .into_iter()
                .find(|tool| tool.name == name)
                .expect("standalone audit must exist");
            assert_eq!(
                tool.input_schema["properties"]["schematic_scope"]["enum"],
                json!(["file", "hierarchy"]),
                "{name} must make its schematic scope explicit"
            );
            assert_eq!(
                tool.input_schema["properties"]["schematic_scope"]["default"], "file",
                "{name} must preserve the existing one-file default"
            );
        }
    }

    #[tokio::test]
    async fn hierarchy_scope_counts_reused_child_instances_for_every_audit() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root.kicad_sch");
        let child = tmp.path().join("shared.kicad_sch");
        std::fs::write(&root, root_with_reused_child("shared.kicad_sch")).unwrap();
        std::fs::write(&child, single_unit_schematic("")).unwrap();
        let args = json!({
            "schematic": root.display().to_string(),
            "schematic_scope": "hierarchy"
        });

        let results = [
            handle_audit_decoupling(&args, &test_ctx()).await.unwrap(),
            handle_audit_connections(&args, &test_ctx()).await.unwrap(),
            handle_audit_power_rails(&args, &test_ctx()).await.unwrap(),
            handle_check_bom_health(&args, &test_ctx()).await.unwrap(),
        ];

        for result in results {
            let audit = tool_json(result);
            assert_eq!(audit["schematic_scope"], "hierarchy", "{audit}");
            assert_eq!(audit["status"], "complete", "{audit}");
            assert_eq!(audit["coverage"]["sheet_instances"], 3, "{audit}");
            assert_eq!(audit["coverage"]["schematic_files"], 2, "{audit}");
            assert_eq!(audit["coverage"]["symbol_instances"], 2, "{audit}");
            assert_eq!(audit["sheet_results"].as_array().unwrap().len(), 3);
            assert!(audit["diagnostics"].as_array().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn design_review_uses_shared_instance_aware_hierarchy_coverage() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root.kicad_sch");
        let child = tmp.path().join("shared.kicad_sch");
        std::fs::write(&root, root_with_reused_child("shared.kicad_sch")).unwrap();
        std::fs::write(&child, single_unit_schematic("")).unwrap();

        let result = review(&root, None).await;
        let report = &result["design_review"];
        assert_eq!(report["coverage"]["schematic"]["sheet_instances"], 3);
        assert_eq!(report["coverage"]["schematic"]["schematic_files"], 2);
        assert_eq!(report["coverage"]["schematic"]["symbol_instances"], 2);
        assert_eq!(
            report["audits"]["bom_health"]["requested"], 2,
            "file-level review work remains deduplicated even though coverage is instance-aware"
        );
    }

    #[tokio::test]
    async fn standalone_file_scope_remains_the_default() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root.kicad_sch");
        let child = tmp.path().join("child.kicad_sch");
        std::fs::write(&root, root_with_child("child.kicad_sch")).unwrap();
        std::fs::write(&child, single_unit_schematic("")).unwrap();

        let result = tool_json(
            handle_check_bom_health(
                &json!({"schematic": root.display().to_string()}),
                &test_ctx(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(result["schematic_scope"], "file");
        assert_eq!(result["status"], "complete");
        assert_eq!(result["total_components"], 0);
        assert_eq!(result["coverage"]["sheet_instances"], 1);
        assert_eq!(result["coverage"]["schematic_files"], 1);
        assert_eq!(result["coverage"]["symbol_instances"], 0);
    }

    #[tokio::test]
    async fn invalid_schematic_scope_is_a_structured_argument_error() {
        let result = handle_check_bom_health(
            &json!({"schematic": "unused.kicad_sch", "schematic_scope": "project"}),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("argument error must be text JSON")
        };
        let body: Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["error"]["kind"], "invalid_argument");
        assert_eq!(body["error"]["field"], "schematic_scope");
    }

    #[tokio::test]
    async fn missing_or_cyclic_hierarchy_is_incomplete_with_diagnostics() {
        let tmp = TempDir::new().unwrap();
        let missing_root = tmp.path().join("missing_root.kicad_sch");
        std::fs::write(&missing_root, root_with_child("missing.kicad_sch")).unwrap();

        let missing = tool_json(
            handle_check_bom_health(
                &json!({
                    "schematic": missing_root.display().to_string(),
                    "schematic_scope": "hierarchy"
                }),
                &test_ctx(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(missing["status"], "partial", "{missing}");
        assert!(missing["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "hierarchy_error"));

        let cyclic_root = tmp.path().join("cyclic_root.kicad_sch");
        std::fs::write(&cyclic_root, root_with_child("cyclic_root.kicad_sch")).unwrap();
        let cyclic = tool_json(
            handle_check_bom_health(
                &json!({
                    "schematic": cyclic_root.display().to_string(),
                    "schematic_scope": "hierarchy"
                }),
                &test_ctx(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(cyclic["status"], "partial", "{cyclic}");
        assert!(cyclic["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("cycle detected"))));
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

    /// A multi-unit part is counted, not held against the review: the audits
    /// walk each placement's own pins, so nothing about it is unreviewed.
    #[tokio::test]
    async fn multi_unit_symbol_is_counted_without_degrading_the_review() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("multi.kicad_sch");
        std::fs::write(&root, multi_unit_schematic()).unwrap();

        let result = review(&root, None).await;
        let report = &result["design_review"];
        assert_eq!(report["status"], "complete");
        assert_eq!(report["coverage"]["schematic"]["multi_unit_symbols"], 1);
        assert!(report["diagnostics"].as_array().unwrap().is_empty());
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

/// The audits used to resolve a pin's net by scanning the file for a label
/// within 0.5 mm of it. On a sheet drawn the normal way — a wire from the pin
/// to a label somewhere along it, rails named by power symbols — almost every
/// pin was netless, so `audit_decoupling` reported decoupled ICs as
/// undecoupled, `audit_power_rails` reported capped rails as uncapped and did
/// not see power-symbol rails at all, and `run_design_review` returned
/// NOT READY on a correct schematic. These sheets are that shape.
#[cfg(test)]
mod net_resolution_tests {
    use super::*;
    use std::io::Write;

    fn sheet(body: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        file.write_all(body.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    fn audit_json(result: CallToolResult) -> Value {
        assert!(!result.is_error, "audit failed: {:?}", result.content);
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("audit result must be text JSON")
        };
        serde_json::from_str(text).expect("audit result must be valid JSON")
    }

    /// U1's VCC pin, a wire to a `VBUS` label 20 mm away, C1 (100 nF) hanging
    /// off that wire under a junction dot, a test point at the wire's far end,
    /// and a `+3V3` rail named by a power symbol with C2 (10 µF) on it.
    ///
    /// Nothing here is near enough to a label for the old scan: C1's pin is
    /// 11.8 mm from the `VBUS` label, and `+3V3` has no label at all.
    const WIRED_SHEET: &str = r##"(kicad_sch
  (version 20250610)
  (generator "konnect")
  (generator_version "10.0")
  (uuid "11111111-1111-4111-8111-111111111111")
  (paper "A4")
  (lib_symbols
    (symbol "power:+3V3"
      (power)
      (symbol "+3V3_0_1"
        (pin power_in line (at 0 0 270) (length 0) (name "+3V3") (number "1"))
      )
    )
    (symbol "Device:C"
      (symbol "C_1_1"
        (pin passive line (at 0 3.81 270) (length 2.54) (name "~") (number "1"))
        (pin passive line (at 0 -3.81 90) (length 2.54) (name "~") (number "2"))
      )
    )
    (symbol "Connector:TestPoint"
      (symbol "TestPoint_1_1"
        (pin passive line (at 0 0 90) (length 0) (name "~") (number "1"))
      )
    )
    (symbol "MCU:U"
      (symbol "U_1_1"
        (pin power_in line (at 0 0 180) (length 2.54) (name "VCC") (number "1"))
      )
    )
  )
  (wire (pts (xy 100 100) (xy 130 100)) (uuid "aaaaaaaa-0000-4000-8000-000000000001"))
  (wire (pts (xy 110 106.19) (xy 110 100)) (uuid "aaaaaaaa-0000-4000-8000-000000000002"))
  (wire (pts (xy 140 120) (xy 140 130)) (uuid "aaaaaaaa-0000-4000-8000-000000000003"))
  (junction (at 110 100) (uuid "bbbbbbbb-0000-4000-8000-000000000001"))
  (label "VBUS" (at 120 100 0) (uuid "cccccccc-0000-4000-8000-000000000001"))
  (symbol
    (lib_id "MCU:U")
    (at 100 100 0)
    (unit 1)
    (uuid "dddddddd-0000-4000-8000-000000000001")
    (property "Reference" "U1" (at 100 100 0))
    (property "Value" "MCU" (at 100 100 0))
    (property "Footprint" "Package_QFP:LQFP-48_7x7mm_P0.5mm" (at 100 100 0))
  )
  (symbol
    (lib_id "Device:C")
    (at 110 110 0)
    (unit 1)
    (uuid "dddddddd-0000-4000-8000-000000000002")
    (property "Reference" "C1" (at 110 110 0))
    (property "Value" "100nF" (at 110 110 0))
    (property "Footprint" "Capacitor_SMD:C_0402_1005Metric" (at 110 110 0))
  )
  (symbol
    (lib_id "Connector:TestPoint")
    (at 130 100 0)
    (unit 1)
    (uuid "dddddddd-0000-4000-8000-000000000003")
    (property "Reference" "TP1" (at 130 100 0))
    (property "Value" "TestPoint" (at 130 100 0))
    (property "Footprint" "TestPoint:TestPoint_Pad_D1.0mm" (at 130 100 0))
  )
  (symbol
    (lib_id "power:+3V3")
    (at 140 120 0)
    (unit 1)
    (uuid "dddddddd-0000-4000-8000-000000000004")
    (property "Reference" "#PWR01" (at 140 120 0))
    (property "Value" "+3V3" (at 140 120 0))
  )
  (symbol
    (lib_id "Device:C")
    (at 140 133.81 0)
    (unit 1)
    (uuid "dddddddd-0000-4000-8000-000000000005")
    (property "Reference" "C2" (at 140 133.81 0))
    (property "Value" "10uF" (at 140 133.81 0))
    (property "Footprint" "Capacitor_SMD:C_0805_2012Metric" (at 140 133.81 0))
  )
)
"##;

    /// A capacitor reaches a power pin through the wire they share, not by
    /// sitting on top of the label that names it.
    #[tokio::test]
    async fn a_capacitor_across_a_wire_decouples_the_pin_at_the_other_end() {
        let file = sheet(WIRED_SHEET);
        let body = audit_json(
            handle_audit_decoupling(
                &json!({ "schematic": file.path().to_str().unwrap() }),
                &test_ctx(),
            )
            .await
            .unwrap(),
        );

        assert_eq!(body["total_power_pins"], 1, "{body}");
        assert_eq!(body["pass_count"], 1, "{body}");
        assert_eq!(body["findings"].as_array().unwrap().len(), 0, "{body}");
    }

    /// A rail named by a power symbol is a rail, and the caps, bulk caps and
    /// test points on it are found through the same graph.
    #[tokio::test]
    async fn a_power_symbol_rail_is_audited_like_a_labelled_one() {
        let file = sheet(WIRED_SHEET);
        let body = audit_json(
            handle_audit_power_rails(
                &json!({ "schematic": file.path().to_str().unwrap() }),
                &test_ctx(),
            )
            .await
            .unwrap(),
        );

        assert_eq!(body["power_nets"], json!(["+3V3", "VBUS"]), "{body}");

        let findings = body["findings"].as_array().unwrap();
        assert!(
            !findings
                .iter()
                .any(|finding| finding["severity"] == "error"),
            "both rails are decoupled: {body}"
        );
        // VBUS carries only a 100 nF part, and only +3V3 lacks a test point.
        let issues: Vec<&str> = findings
            .iter()
            .map(|finding| finding["issue"].as_str().unwrap())
            .collect();
        assert_eq!(
            issues,
            [
                "Power rail 'VBUS' has no bulk capacitance (>= 10uF)",
                "Power rail '+3V3' has no test point"
            ],
            "{body}"
        );
    }

    /// `run_design_review`'s coverage counts the rails power symbols name, and
    /// the review it fronts finds no fault on this sheet.
    #[tokio::test]
    async fn a_correctly_drawn_sheet_is_not_reported_as_not_ready() {
        let file = sheet(WIRED_SHEET);
        let body = audit_json(
            handle_run_design_review(
                &json!({ "schematic": file.path().to_str().unwrap(), "severity_filter": "info" }),
                &test_ctx(),
            )
            .await
            .unwrap(),
        );
        let review = &body["design_review"];

        assert_eq!(review["coverage"]["schematic"]["named_nets"], 2, "{review}");
        assert_eq!(review["errors"], 0, "{review}");
        assert!(
            !review["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| finding["category"] == "decoupling"),
            "{review}"
        );
    }

    /// U2's SDA and SCL pins, each on a wire whose label sits 8 mm along it.
    /// `pull_ups` is R1 and R2 tying those wires to a `+3V3` power symbol —
    /// two resistors whose own pins are 5 mm from any label.
    fn i2c_sheet(pull_ups: &str) -> String {
        format!(
            r##"(kicad_sch
  (version 20250610)
  (generator "konnect")
  (generator_version "10.0")
  (uuid "11111111-1111-4111-8111-111111111111")
  (paper "A4")
  (lib_symbols
    (symbol "power:+3V3"
      (power)
      (symbol "+3V3_0_1"
        (pin power_in line (at 0 0 270) (length 0) (name "+3V3") (number "1"))
      )
    )
    (symbol "Device:R"
      (symbol "R_1_1"
        (pin passive line (at 0 2.54 270) (length 1.27) (name "~") (number "1"))
        (pin passive line (at 0 -2.54 90) (length 1.27) (name "~") (number "2"))
      )
    )
    (symbol "MCU:I2C"
      (symbol "I2C_1_1"
        (pin bidirectional line (at 0 0 180) (length 2.54) (name "SDA") (number "1"))
        (pin bidirectional line (at 0 -5.08 180) (length 2.54) (name "SCL") (number "2"))
      )
    )
  )
  (wire (pts (xy 100 100) (xy 115 100)) (uuid "aaaaaaaa-0000-4000-8000-000000000001"))
  (wire (pts (xy 100 105.08) (xy 125 105.08)) (uuid "aaaaaaaa-0000-4000-8000-000000000002"))
  (label "SDA" (at 108 100 0) (uuid "cccccccc-0000-4000-8000-000000000001"))
  (label "SCL" (at 108 105.08 0) (uuid "cccccccc-0000-4000-8000-000000000002"))
  (symbol
    (lib_id "MCU:I2C")
    (at 100 100 0)
    (unit 1)
    (uuid "dddddddd-0000-4000-8000-000000000001")
    (property "Reference" "U2" (at 100 100 0))
    (property "Value" "SENSOR" (at 100 100 0))
    (property "Footprint" "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm" (at 100 100 0))
  )
{pull_ups})
"##
        )
    }

    const I2C_PULL_UPS: &str = r##"  (symbol
    (lib_id "Device:R")
    (at 115 97.46 0)
    (unit 1)
    (uuid "dddddddd-0000-4000-8000-000000000002")
    (property "Reference" "R1" (at 115 97.46 0))
    (property "Value" "4.7k" (at 115 97.46 0))
    (property "Footprint" "Resistor_SMD:R_0402_1005Metric" (at 115 97.46 0))
  )
  (symbol
    (lib_id "Device:R")
    (at 125 102.54 0)
    (unit 1)
    (uuid "dddddddd-0000-4000-8000-000000000003")
    (property "Reference" "R2" (at 125 102.54 0))
    (property "Value" "4.7k" (at 125 102.54 0))
    (property "Footprint" "Resistor_SMD:R_0402_1005Metric" (at 125 102.54 0))
  )
  (symbol
    (lib_id "power:+3V3")
    (at 115 94.92 0)
    (unit 1)
    (uuid "dddddddd-0000-4000-8000-000000000004")
    (property "Reference" "#PWR02" (at 115 94.92 0))
    (property "Value" "+3V3" (at 115 94.92 0))
  )
  (symbol
    (lib_id "power:+3V3")
    (at 125 100 0)
    (unit 1)
    (uuid "dddddddd-0000-4000-8000-000000000005")
    (property "Reference" "#PWR03" (at 125 100 0))
    (property "Value" "+3V3" (at 125 100 0))
  )
"##;

    async fn connection_findings(schematic: &str) -> Vec<Value> {
        let file = sheet(schematic);
        let body = audit_json(
            handle_audit_connections(
                &json!({ "schematic": file.path().to_str().unwrap() }),
                &test_ctx(),
            )
            .await
            .unwrap(),
        );
        body["findings"].as_array().unwrap().clone()
    }

    /// The missing pull-up this audit exists to report. A pin whose net the
    /// old scan could not resolve was skipped silently, so the fault went
    /// unreported — the same blindness as the false positives, pointing the
    /// other way.
    #[tokio::test]
    async fn a_missing_i2c_pull_up_is_reported_with_the_net_it_is_missing_from() {
        let findings = connection_findings(&i2c_sheet("")).await;
        let issues: Vec<&str> = findings
            .iter()
            .map(|finding| finding["issue"].as_str().unwrap())
            .collect();
        assert_eq!(
            issues,
            [
                "I2C SDA pin on U2 (net: SDA) has no pull-up resistor",
                "I2C SCL pin on U2 (net: SCL) has no pull-up resistor"
            ],
            "{findings:?}"
        );
    }

    /// And the same sheet with the pull-ups fitted is silent. Neither resistor
    /// pin sits on a label: one reaches its net down a wire, the other through
    /// the power symbol naming the rail.
    #[tokio::test]
    async fn a_pull_up_is_found_through_the_wire_and_the_power_symbol() {
        let findings = connection_findings(&i2c_sheet(I2C_PULL_UPS)).await;
        assert!(findings.is_empty(), "{findings:?}");
    }

    /// A dual amplifier whose power pin lives on a third unit: unit 1 at
    /// (100, 100) and unit 2 at (100, 120) carry only an output, unit 3 at
    /// (140, 100) carries `V+`, wired to a `VCC` label with C1 on it.
    ///
    /// Every pin sits at its unit's origin, so a walk that superimposes all
    /// three units reports a `V+` pin at (100, 100) and (100, 120) too — where
    /// no rail is drawn (#182).
    const MULTI_UNIT_POWER_SHEET: &str = r##"(kicad_sch
  (version 20250610)
  (generator "konnect")
  (generator_version "10.0")
  (uuid "11111111-1111-4111-8111-111111111111")
  (paper "A4")
  (lib_symbols
    (symbol "Device:C"
      (symbol "C_1_1"
        (pin passive line (at 0 3.81 270) (length 0) (name "~") (number "1"))
        (pin passive line (at 0 -3.81 90) (length 0) (name "~") (number "2"))
      )
    )
    (symbol "Amplifier:DUAL"
      (symbol "DUAL_1_1"
        (pin output line (at 0 0 180) (length 0) (name "OUT") (number "1"))
      )
      (symbol "DUAL_2_1"
        (pin output line (at 0 0 180) (length 0) (name "OUT") (number "7"))
      )
      (symbol "DUAL_3_1"
        (pin power_in line (at 0 0 270) (length 0) (name "V+") (number "8"))
      )
    )
  )
  (wire (pts (xy 140 100) (xy 160 100)) (uuid "aaaaaaaa-0000-4000-8000-000000000001"))
  (label "VCC" (at 150 100 0) (uuid "cccccccc-0000-4000-8000-000000000001"))
  (symbol
    (lib_id "Amplifier:DUAL")
    (at 100 100 0)
    (unit 1)
    (uuid "dddddddd-0000-4000-8000-000000000001")
    (property "Reference" "U1" (at 100 100 0))
    (property "Value" "DUAL" (at 100 100 0))
    (property "Footprint" "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm" (at 100 100 0))
  )
  (symbol
    (lib_id "Amplifier:DUAL")
    (at 100 120 0)
    (unit 2)
    (uuid "dddddddd-0000-4000-8000-000000000002")
    (property "Reference" "U1" (at 100 120 0))
    (property "Value" "DUAL" (at 100 120 0))
    (property "Footprint" "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm" (at 100 120 0))
  )
  (symbol
    (lib_id "Amplifier:DUAL")
    (at 140 100 0)
    (unit 3)
    (uuid "dddddddd-0000-4000-8000-000000000003")
    (property "Reference" "U1" (at 140 100 0))
    (property "Value" "DUAL" (at 140 100 0))
    (property "Footprint" "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm" (at 140 100 0))
  )
  (symbol
    (lib_id "Device:C")
    (at 150 103.81 0)
    (unit 1)
    (uuid "dddddddd-0000-4000-8000-000000000004")
    (property "Reference" "C1" (at 150 103.81 0))
    (property "Value" "100nF" (at 150 103.81 0))
    (property "Footprint" "Capacitor_SMD:C_0402_1005Metric" (at 150 103.81 0))
  )
)
"##;

    /// The power pin belongs to the unit that draws it: one pin, decoupled,
    /// and no phantom copy of it reported at the other two placements.
    #[tokio::test]
    async fn a_power_pin_is_audited_only_at_the_unit_that_draws_it() {
        let file = sheet(MULTI_UNIT_POWER_SHEET);
        let body = audit_json(
            handle_audit_decoupling(
                &json!({ "schematic": file.path().to_str().unwrap() }),
                &test_ctx(),
            )
            .await
            .unwrap(),
        );

        assert_eq!(body["total_power_pins"], 1, "{body}");
        assert_eq!(body["pass_count"], 1, "{body}");
        assert_eq!(body["findings"].as_array().unwrap().len(), 0, "{body}");
    }

    /// The same sheet through `run_design_review`: a multi-unit part no longer
    /// makes the review incomplete, because its units are now audited.
    #[tokio::test]
    async fn a_multi_unit_part_no_longer_holds_the_review_open() {
        let file = sheet(MULTI_UNIT_POWER_SHEET);
        let body = audit_json(
            handle_run_design_review(
                &json!({ "schematic": file.path().to_str().unwrap() }),
                &test_ctx(),
            )
            .await
            .unwrap(),
        );
        let review = &body["design_review"];

        assert_eq!(review["status"], "complete", "{review}");
        assert_eq!(review["coverage"]["schematic"]["multi_unit_symbols"], 1);
        assert_eq!(review["errors"], 0, "{review}");
    }

    /// A real three-unit part from the KiCAD demos: each ECC83 placement walks
    /// its own pins and no others. The two triodes and the heater are placed
    /// apart, so a unit-agnostic walk would put pins 4/5/9 on a triode's
    /// coordinates and report nets from there.
    ///
    /// Asserted on the pins the audits walk rather than on their findings:
    /// this sheet labels none of the nets at those pins.
    #[test]
    fn each_placed_ecc83_unit_walks_only_its_own_pins() {
        let tree = konnect_sexp::parser::parse_sexp(include_str!(
            "../../tests/fixtures/ecc83_multiunit.kicad_sch"
        ))
        .unwrap();

        let tube_units: Vec<(u32, Vec<String>)> = placed_pins_by_reference(&tree)
            .into_iter()
            .filter(|(inst, _)| inst.lib_id.ends_with(":ECC83"))
            .map(|(inst, pins)| {
                let mut numbers: Vec<String> =
                    pins.into_iter().map(|(pin, _)| pin.number).collect();
                numbers.sort();
                (inst.unit, numbers)
            })
            .collect();

        assert_eq!(
            tube_units,
            vec![
                (1, vec!["6".into(), "7".into(), "8".into()]),
                (2, vec!["1".into(), "2".into(), "3".into()]),
                (3, vec!["4".into(), "5".into(), "9".into()]),
            ]
        );
    }
}
