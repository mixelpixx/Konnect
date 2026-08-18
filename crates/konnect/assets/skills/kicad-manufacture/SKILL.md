---
name: kicad-manufacture
description: |
  Manufacturing and fabrication workflow for KiCAD projects via MCP tools. Triggers on: "send to fab",
  "order boards", "gerbers", "JLCPCB", "manufacturing", "export for production", "pick and place",
  "assembly files", "generate fabrication outputs", "BOM for fab", "production files", "fab house".
argument-hint: "[fab house or export task]"
---

# KiCAD Manufacturing & Fabrication Workflow

This skill guides Claude through preparing a KiCAD design for manufacturing using Konnect MCP tools.
ALL modifications go through MCP tools — never edit project files directly.

---

## Toolset Loading

Before any manufacturing work, load the required toolsets:

```
load_toolset('pcb_export')       # export_gerber, export_bom, export_position_file, get_drc_violations
load_toolset('manufacturing')    # export_manufacturing_package, validate_for_manufacturing, estimate_cost
```

Load additional toolsets as needed:

```
load_toolset('sch_analysis')     # inspect nets, verify footprints assigned
load_toolset('integration')      # search_jlcpcb_parts, suggest_jlcpcb_alternatives
load_toolset('pcb_export')       # export_3d for visual verification
```

Always call `get_active_toolsets()` first to see what is already loaded.

---

## Pre-Flight Checklist

Run these checks BEFORE generating any manufacturing outputs. Stop and fix issues at each stage.

### 1. DRC — Zero Errors Required

```
get_drc_violations()
```

- All errors must be resolved. Warnings should be reviewed but may be waived.
- Common blockers: unrouted nets, clearance violations, minimum width violations.
- Do NOT proceed to export if any DRC errors remain.

### 2. Manufacturing Validation

```
validate_for_manufacturing()
```

- Checks board outline is closed
- Checks all pads have copper
- Checks drill sizes are within fabrication limits
- Checks silkscreen does not overlap pads

### 3. Verify Footprints Assigned

Every schematic symbol must have a footprint assigned. Check for:
- Missing footprint assignments (shows as empty Footprint field)
- Mismatched footprints (wrong pad count for the symbol)
- Non-existent footprint references (library not found)

---

## Export Workflow

### One-Shot Export (Recommended)

```
export_manufacturing_package(board, output_dir, fab_house?, schematic?)
```

`fab_house` selects the house profile (there is no `format` argument). Pass
`schematic` when you want the BOM generated as part of the package.

Generates all manufacturing files in one call:
- Gerbers (all copper layers + mask + silkscreen + edge cuts)
- Drill files (Excellon format)
- BOM (CSV)
- Pick-and-place / component position file (CPL)
- Job file (optional, fab-house specific)

### Manual Export (When You Need Control)

Use individual tools when you need specific settings per file:

#### Step 1: Gerbers

```
export_gerber(board, output_dir, layers?, drill_file?)
```

Standard layers to export:
- F.Cu, B.Cu (and inner layers if present)
- F.Mask, B.Mask
- F.SilkS, B.SilkS
- F.Paste, B.Paste (for stencils)
- Edge.Cuts (board outline)

#### Step 2: Bill of Materials

```
export_bom(schematic, output, format?, fields?, group_by?, labels?, exclude_dnp?)
```

Include fields: Reference, Value, Footprint, LCSC (if targeting JLCPCB).

#### Step 3: Component Position File

```
export_position_file(board, output, format?, side?, units?)
```

Required for SMT assembly. Contains X/Y/Rotation for each component.
Export separately for top and bottom if double-sided assembly.

---

## JLCPCB-Specific Guidance

### Part Sourcing

```
search_jlcpcb_parts(query)                     # Find LCSC part numbers
suggest_jlcpcb_alternatives(value, footprint)  # Find alternatives for OOS parts
```

### Part Categories

| Category           | Description                              | Extra Cost             |
|--------------------|------------------------------------------|------------------------|
| **Basic**          | ~700 common parts, pre-loaded on machine | None                   |
| **Preferred Ext.** | Popular extended parts                   | No feeder loading fee  |
| **Extended**       | 300k+ parts, loaded on demand            | $3 per unique part     |

**Strategy**: Use basic parts wherever possible. Every extended part adds $3 to assembly cost.
Search with `search_jlcpcb_parts` and filter by `basic: true` when looking for alternatives.

### Minimum Design Rules (JLCPCB Standard Process)

| Parameter              | Minimum Value |
|------------------------|---------------|
| Trace width            | 0.127mm (5mil)  |
| Trace spacing          | 0.127mm (5mil)  |
| Via drill              | 0.3mm          |
| Via annular ring       | 0.15mm (6mil)   |
| Min hole size          | 0.3mm          |
| Pad-to-pad clearance   | 0.254mm (10mil) |
| Silkscreen line width  | 0.15mm         |
| Board thickness        | 0.8-2.0mm (1.6 default) |
| Min board size         | 10x10mm        |

Use `add_design_rule` or `list_design_rules` to configure project rules to match.

### JLCPCB BOM Requirements

- Column headers must be exactly: `Designator`, `Comment`, `Footprint`, `LCSC Part #`
- LCSC Part # format: `Cxxxxxx` (e.g., C14663)
- Group identical parts on one row with comma-separated designators

### JLCPCB CPL (Position File) Requirements

- Columns: `Designator`, `Mid X`, `Mid Y`, `Layer`, `Rotation`
- Coordinates in millimeters
- Rotation in degrees (0-360)
- Layer values: `Top` or `Bottom`

---

## Cost Estimation

```
estimate_cost(board, quantity?, layers?, fab_house?)
```

Factors that increase cost:
- Layer count (2 vs 4 vs 6+)
- Board size
- Number of unique extended parts
- Double-sided assembly
- Special finishes (ENIG vs HASL)
- Tight tolerances below standard minimums
- Expedited turnaround

---

## 3D Verification

Before submitting to fab, always generate a 3D view:

```
export_3d(board, output, format?, include_unspecified?)
```

Visual checks:
- Component clearance (tall parts near board edges)
- Connector accessibility and orientation
- Mounting hole alignment
- Heatsink/thermal pad clearance
- Enclosure fit (if applicable)

---

## Common Mistakes

1. **Exporting with DRC errors** — Always run DRC first. A clearance violation can short traces on the fab board.
2. **Wrong drill file format** — JLCPCB expects Excellon format. PTH and NPTH in separate files.
3. **Missing board outline** — Edge.Cuts layer must be a closed polygon. Open outlines cause fab rejection.
4. **Silkscreen on pads** — Silkscreen ink on exposed copper pads prevents soldering. Remove overlaps.
5. **Wrong position file origin** — CPL origin must match board origin. Use board center or bottom-left corner consistently.
6. **Forgetting paste layer** — If ordering stencils, F.Paste/B.Paste must be exported.
7. **Out-of-stock parts in BOM** — Always verify availability with `search_jlcpcb_parts` before ordering.
8. **Rotation offsets** — JLCPCB may apply rotation corrections. Review their orientation guide for ICs and polarized components.
9. **Panelization not accounted for** — If panelizing, export from the panel file, not the individual board.
10. **Missing fiducials** — SMT assembly with fine-pitch parts requires at least 2 fiducial marks on each assembly side.

---

## Rules

1. **Never export without passing DRC** — zero errors required
2. **Never skip validate_for_manufacturing** — catches issues DRC misses
3. **Always verify part availability** before finalizing BOM for assembly
4. **Export 3D model** before submitting order — visual sanity check
5. **Save project before export** — ensures exported files match current state
6. **Load toolsets first** — check `get_active_toolsets()` and load what you need
7. **Use one-shot export when possible** — `export_manufacturing_package` ensures consistency
8. **Double-check fab house requirements** — each house has slightly different file format expectations
