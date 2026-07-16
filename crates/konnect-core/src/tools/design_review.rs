//! `design_review` toolset — AI-powered design audits.
//!
//! Analyzes schematic and PCB files for common design issues. Returns structured
//! findings that Claude can explain, prioritize, and suggest fixes for.
//!
//! These tools work on the S-expression files directly — no KiCAD running required.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, ToolContext, ToolDef};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    parser::parse_sexp,
    schematic::{extract_lib_pins, extract_symbol_instances, pin_endpoint, read_schematic},
};
use serde_json::json;
use std::collections::HashSet;
use tracing::info;

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "audit_decoupling",
            "Check that all ICs have appropriate decoupling capacitors. Finds power pins \
             without nearby capacitors and flags wrong values. The #1 most common PCB design mistake.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "board": { "type": "string", "description": "Path to .kicad_pcb file (optional, for distance check)" },
                    "max_distance_mm": {
                        "type": "number",
                        "description": "Max allowed distance from power pin to decoupling cap on PCB (mm)",
                        "default": 5.0
                    }
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
             This is the tool to call when the user asks 'is my board ready?' or 'review my design'.",
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
        let lib_sym = lib_syms
            .iter()
            .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));
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
        serde_json::to_string_pretty(&json!({
            "audit": "decoupling",
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
        let lib_sym = match lib_syms
            .iter()
            .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id))
        {
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
        serde_json::to_string_pretty(&json!({
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
        serde_json::to_string_pretty(&json!({
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
        serde_json::to_string_pretty(&json!({
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

    let mut all_findings: Vec<serde_json::Value> = Vec::new();
    let mut audit_results = Vec::new();

    // Run schematic audits
    if args["schematic"].is_string() {
        let decoupling = handle_audit_decoupling(args, ctx).await?;
        audit_results.push(("decoupling", extract_findings(&decoupling)));

        let connections = handle_audit_connections(args, ctx).await?;
        audit_results.push(("connections", extract_findings(&connections)));

        let power = handle_audit_power_rails(args, ctx).await?;
        audit_results.push(("power_rails", extract_findings(&power)));

        let bom = handle_check_bom_health(args, ctx).await?;
        audit_results.push(("bom_health", extract_findings(&bom)));
    }

    // Run PCB audits
    if args["board"].is_string() {
        let dfm = handle_audit_manufacturing(args, ctx).await?;
        audit_results.push(("manufacturing", extract_findings(&dfm)));
    }

    // Collect and filter findings
    let mut error_count = 0;
    let mut warning_count = 0;
    let mut info_count = 0;

    for (audit_name, findings) in &audit_results {
        for finding in findings {
            let sev = finding["severity"].as_str().unwrap_or("info");
            let rank = match sev {
                "error" => {
                    error_count += 1;
                    2
                }
                "warning" => {
                    warning_count += 1;
                    1
                }
                _ => {
                    info_count += 1;
                    0
                }
            };
            if rank >= min_rank {
                let mut f = finding.clone();
                f["audit"] = json!(audit_name);
                all_findings.push(f);
            }
        }
    }

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

    let verdict = if error_count > 0 {
        "NOT READY — critical issues must be fixed before manufacturing"
    } else if warning_count > 0 {
        "NEEDS ATTENTION — review warnings before manufacturing"
    } else {
        "LOOKS GOOD — no critical issues found"
    };

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "design_review": {
                "verdict": verdict,
                "errors": error_count,
                "warnings": warning_count,
                "info": info_count,
                "severity_filter": severity_filter,
                "findings": all_findings
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
        serde_json::to_string_pretty(&json!({
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
        let lib_sym = lib_syms
            .iter()
            .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));
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

        let lib_sym = lib_syms
            .iter()
            .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));
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
        let lib_sym = lib_syms
            .iter()
            .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));
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

fn extract_findings(result: &CallToolResult) -> Vec<serde_json::Value> {
    if let Some(crate::mcp::protocol::ToolContent::Text { text }) = result.content.first() {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) {
            if let Some(findings) = parsed["findings"].as_array() {
                return findings.clone();
            }
        }
    }
    Vec::new()
}
