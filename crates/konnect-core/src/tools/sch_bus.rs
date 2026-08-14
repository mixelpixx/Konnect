//! `sch_bus` toolset — buses, bus entries, and pin-to-bus fan-out.
//!
//! A KiCad bus is three things drawn together, and all three have to agree or
//! the bus carries nothing:
//!
//!   1. the **bus segment** itself — geometrically a wire, distinguished only
//!      by the node name and by the label attached to it;
//!   2. a **bus entry** per member — the 45° tick that bridges the gap between
//!      a wire and the bus. It is not decoration: without it the wire and the
//!      bus do not touch, because they are deliberately drawn apart;
//!   3. a **label on every wire stub**, naming the member signal, plus a label
//!      on the bus naming the group.
//!
//! Membership is by *name*, never by geometry: a stub joins the bus because its
//! label matches a member of the bus label, so `connect_pins_to_bus` writes all
//! three parts from one call rather than leaving the caller to line them up.
//!
//! Bus label syntax, both accepted by `add_schematic_net_label`:
//!   - vector  `X_DIG[1..6]`   — members `X_DIG1` … `X_DIG6`
//!   - group   `{A B C}`       — members named individually

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, opt_f64, opt_str, require_f64, ToolContext, ToolDef};
use konnect_sexp::{
    parser::parse_sexp,
    schematic::{
        extract_lib_pins_for_unit, extract_symbol_instances, format_bus, format_bus_entry,
        format_net_label, format_wire, pin_endpoint, BusEntryDirection,
    },
    writer::write_atomic,
};
use serde_json::json;

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "add_bus",
            "Add a bus segment between two points. Geometrically a wire, but KiCad treats it as a \
             bus: it carries the members named by the bus label attached to it (vector \
             'NAME[1..6]' or group '{A B C}'), and ordinary wires join it only through a bus entry.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "x1": { "type": "number" },
                    "y1": { "type": "number" },
                    "x2": { "type": "number" },
                    "y2": { "type": "number" }
                },
                "required": ["schematic", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_add_bus(args, ctx).await }
        ),
        tool!(
            "batch_add_bus",
            "Add multiple bus segments in a single file read/write cycle.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "buses": {
                        "type": "array",
                        "description": "List of {x1,y1,x2,y2} segments",
                        "items": {
                            "type": "object",
                            "properties": {
                                "x1": { "type": "number" }, "y1": { "type": "number" },
                                "x2": { "type": "number" }, "y2": { "type": "number" }
                            },
                            "required": ["x1", "y1", "x2", "y2"]
                        }
                    }
                },
                "required": ["schematic", "buses"]
            }),
            |args, ctx| async move { handle_batch_add_bus(args, ctx).await }
        ),
        tool!(
            "add_bus_entry",
            "Add a bus entry — the 45-degree tick that connects a wire to a bus. Required: a wire \
             and a bus that merely touch are NOT connected without one. 'at' is the end on the \
             wire side; 'direction' says which way the tick runs to reach the bus.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "x": { "type": "number", "description": "X of the wire-side end" },
                    "y": { "type": "number", "description": "Y of the wire-side end" },
                    "direction": {
                        "type": "string",
                        "description": "Which way the tick runs from 'at' to the bus",
                        "enum": ["down_right", "down_left", "up_right", "up_left"],
                        "default": "down_right"
                    }
                },
                "required": ["schematic", "x", "y"]
            }),
            |args, ctx| async move { handle_add_bus_entry(args, ctx).await }
        ),
        tool!(
            "connect_pins_to_bus",
            "Fan a set of pins out onto a bus: for each pin, a wire stub from the pin to the bus \
             entry, the bus entry itself, and a net label naming the member. This is the whole \
             connection — a stub without a label joins nothing, since bus membership is by name. \
             Give the bus's fixed coordinate ('bus_y' for a horizontal bus, 'bus_x' for a \
             vertical one); the bus segment itself is drawn separately with add_bus.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "bus_y": { "type": "number", "description": "Y of a horizontal bus (omit if vertical)" },
                    "bus_x": { "type": "number", "description": "X of a vertical bus (omit if horizontal)" },
                    "connections": {
                        "type": "array",
                        "description": "List of {reference, pin_number, net} — net is the bus member name",
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "pin_number": { "type": "string" },
                                "net": { "type": "string" }
                            },
                            "required": ["reference", "pin_number", "net"]
                        }
                    },
                },
                "required": ["schematic", "connections"]
            }),
            |args, ctx| async move { handle_connect_pins_to_bus(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_add_bus(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let (x1, y1, x2, y2) = match read_xy_pair(args) {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    if (x1 - x2).abs() > 0.01 && (y1 - y2).abs() > 0.01 {
        return Ok(CallToolResult::error(
            "Bus segment must be horizontal or vertical",
        ));
    }

    let content = std::fs::read_to_string(&sch_path)?;
    let new_content =
        crate::tools::sch_wiring::insert_before_close(&content, &format_bus(x1, y1, x2, y2));
    write_atomic(&sch_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "added": "bus",
        "from": { "x": x1, "y": y1 },
        "to": { "x": x2, "y": y2 }
    })))
}

async fn handle_batch_add_bus(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let items = match args["buses"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'buses' array")),
    };

    let mut inserts = String::new();
    let mut added: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for (i, item) in items.iter().enumerate() {
        let (x1, y1, x2, y2) = match read_xy_pair(item) {
            Ok(v) => v,
            Err(_) => {
                errors.push(format!("Segment {i}: needs x1, y1, x2, y2"));
                continue;
            }
        };
        if (x1 - x2).abs() > 0.01 && (y1 - y2).abs() > 0.01 {
            errors.push(format!("Segment {i}: must be horizontal or vertical"));
            continue;
        }
        inserts.push('\n');
        inserts.push_str("  ");
        inserts.push_str(&format_bus(x1, y1, x2, y2));
        added.push(json!({ "from": {"x": x1, "y": y1}, "to": {"x": x2, "y": y2} }));
    }

    let content = std::fs::read_to_string(&sch_path)?;
    let new_content = crate::tools::sch_wiring::insert_before_close(&content, &inserts);
    write_atomic(&sch_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "added_count": added.len(),
        "added": added,
        "errors": errors
    })))
}

async fn handle_add_bus_entry(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let raw = opt_str(args, "direction").unwrap_or("down_right");
    let direction = match parse_direction(raw) {
        Some(d) => d,
        None => {
            return Ok(CallToolResult::error(format!(
                "Unknown direction '{raw}'. Valid: down_right, down_left, up_right, up_left"
            )))
        }
    };

    let content = std::fs::read_to_string(&sch_path)?;
    let new_content =
        crate::tools::sch_wiring::insert_before_close(&content, &format_bus_entry(x, y, direction));
    write_atomic(&sch_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "added": "bus_entry",
        "wire_side": { "x": x, "y": y },
        "bus_side": { "x": x + direction.size().0, "y": y + direction.size().1 }
    })))
}

async fn handle_connect_pins_to_bus(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let conns = match args["connections"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'connections' array")),
    };
    let bus_y = opt_f64(args, "bus_y");
    let bus_x = opt_f64(args, "bus_x");
    let size = 2.54; // BusEntryDirection fixes the tick at one grid step

    let horizontal = match (bus_y, bus_x) {
        (Some(_), None) => true,
        (None, Some(_)) => false,
        _ => {
            return Ok(CallToolResult::error(
                "Give exactly one of 'bus_y' (horizontal bus) or 'bus_x' (vertical bus)",
            ))
        }
    };

    let content = std::fs::read_to_string(&sch_path)?;
    let tree = match parse_sexp(&content) {
        Ok(t) => t,
        Err(e) => return Ok(CallToolResult::error(format!("Parse error: {e}"))),
    };
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let mut inserts = String::new();
    let mut added: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for conn in &conns {
        let (reference, pin_number, net) = match (
            conn["reference"].as_str(),
            conn["pin_number"].as_str(),
            conn["net"].as_str(),
        ) {
            (Some(r), Some(p), Some(n)) => (r, p, n),
            _ => {
                errors.push("Each connection needs reference, pin_number and net".into());
                continue;
            }
        };

        // Resolve against the unit that actually owns the pin — a multi-unit
        // part repeats its reference on every unit.
        let ep = instances
            .iter()
            .filter(|i| i.reference == reference)
            .find_map(|inst| {
                let sym = lib_syms
                    .iter()
                    .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id))?;
                extract_lib_pins_for_unit(sym, inst.unit)
                    .into_iter()
                    .find(|p| p.number == pin_number)
                    .map(|p| pin_endpoint(&p, inst.pin_transform()))
            });

        let (px, py) = match ep {
            Some(v) => v,
            None => {
                errors.push(format!("Pin {pin_number} of '{reference}' not found"));
                continue;
            }
        };

        // The entry bridges the last `size` before the bus; the stub covers the
        // rest of the way from the pin.
        let (entry_x, entry_y, dx, dy, sx, sy) = if horizontal {
            let by = bus_y.unwrap();
            let dir = if by > py { 1.0 } else { -1.0 };
            let ey = by - dir * size;
            (px, ey, dir * size, dir * size, px, py)
        } else {
            let bx = bus_x.unwrap();
            let dir = if bx > px { 1.0 } else { -1.0 };
            let ex = bx - dir * size;
            (ex, py, dir * size, dir * size, px, py)
        };

        // Stub from the pin up to the entry's wire-side end. Zero-length when
        // the pin already sits there; KiCad rejects a degenerate wire.
        if (sx - entry_x).abs() > 0.01 || (sy - entry_y).abs() > 0.01 {
            inserts.push_str("\n  ");
            inserts.push_str(&format_wire(sx, sy, entry_x, entry_y));
        }
        inserts.push_str("\n  ");
        inserts.push_str(&format_bus_entry(entry_x, entry_y, entry_dir(dx, dy)));
        // The label is what actually puts this stub on the bus.
        let rot = if horizontal { 90.0 } else { 0.0 };
        inserts.push_str("\n  ");
        inserts.push_str(&format_net_label(net, sx, sy, rot));

        added.push(json!({
            "reference": reference, "pin": pin_number, "net": net,
            "pin_at": { "x": px, "y": py },
            "entry_at": { "x": entry_x, "y": entry_y }
        }));
    }

    let new_content = crate::tools::sch_wiring::insert_before_close(&content, &inserts);
    write_atomic(&sch_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "connected_count": added.len(),
        "connected": added,
        "errors": errors
    })))
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Map a signed (dx, dy) tick offset onto KiCad's four bus-entry directions.
fn entry_dir(dx: f64, dy: f64) -> BusEntryDirection {
    match (dx > 0.0, dy > 0.0) {
        (true, true) => BusEntryDirection::DownRight,
        (false, true) => BusEntryDirection::DownLeft,
        (true, false) => BusEntryDirection::UpRight,
        (false, false) => BusEntryDirection::UpLeft,
    }
}

fn parse_direction(s: &str) -> Option<BusEntryDirection> {
    Some(match s {
        "down_right" => BusEntryDirection::DownRight,
        "down_left" => BusEntryDirection::DownLeft,
        "up_right" => BusEntryDirection::UpRight,
        "up_left" => BusEntryDirection::UpLeft,
        _ => return None,
    })
}

fn read_xy_pair(v: &serde_json::Value) -> Result<(f64, f64, f64, f64), CallToolResult> {
    Ok((
        require_f64(v, "x1")?,
        require_f64(v, "y1")?,
        require_f64(v, "x2")?,
        require_f64(v, "y2")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_sexp_is_a_bus_node_not_a_wire() {
        let s = format_bus(10.0, 20.0, 50.0, 20.0);
        assert!(s.starts_with("(bus"), "got {s}");
        assert!(s.contains("(xy 10 20) (xy 50 20)"));
    }

    /// `size` is the offset to the bus side, so the two ends are `at` and
    /// `at + size`. Getting the sign wrong puts the tick on the far side of the
    /// bus, where it connects nothing.
    #[test]
    fn entry_direction_follows_the_offset_signs() {
        assert!(matches!(
            entry_dir(2.54, 2.54),
            BusEntryDirection::DownRight
        ));
        assert!(matches!(
            entry_dir(-2.54, 2.54),
            BusEntryDirection::DownLeft
        ));
        assert!(matches!(entry_dir(2.54, -2.54), BusEntryDirection::UpRight));
        assert!(matches!(entry_dir(-2.54, -2.54), BusEntryDirection::UpLeft));
    }

    #[test]
    fn direction_names_round_trip() {
        for (name, dir) in [
            ("down_right", BusEntryDirection::DownRight),
            ("down_left", BusEntryDirection::DownLeft),
            ("up_right", BusEntryDirection::UpRight),
            ("up_left", BusEntryDirection::UpLeft),
        ] {
            assert_eq!(parse_direction(name).unwrap().size(), dir.size());
        }
        assert!(parse_direction("sideways").is_none());
    }

    #[test]
    fn all_four_tools_are_registered() {
        let names: Vec<_> = tools().iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "add_bus",
                "batch_add_bus",
                "add_bus_entry",
                "connect_pins_to_bus"
            ]
        );
    }
}
