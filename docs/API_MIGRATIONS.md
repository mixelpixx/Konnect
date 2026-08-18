# Konnect MCP API migrations

Konnect's tool schemas are public API. This file records intentional argument
removals and the supported replacement workflow.

## Unreleased: remove inputs that never affected an operation

The following optional inputs were advertised but never read by their handlers.
Keeping them would let a client believe a request was honoured when the result was
identical without it.

| Removed input | Migration |
|---|---|
| `import_sheet_pins.project_name` | Omit it. Importing hierarchical labels as sheet pins does not modify project-instance paths. |
| `refill_zones.zones` | Omit it. KiCad IPC refills every zone on the active board and exposes no per-net selector. |
| `run_drc.tests` | Omit it. `kicad-cli pcb drc` runs the complete configured ruleset. Configure rules/waivers in KiCad; `severity` and `limit` only filter Konnect's returned report. |
| `audit_decoupling.board` and `audit_decoupling.max_distance_mm` | Run `audit_decoupling(schematic)` for net-connectivity coverage, then use PCB placement/clearance inspection for physical capacitor distance. The audit never measured PCB distance. |
| `export_manufacturing_package.quantity` | Omit it from export. Manufacturing files are quantity-independent; pass `quantity` to `estimate_cost` for pricing context. |
| `validate_for_manufacturing.schematic` | Run the board validator without it. Use `check_bom_health(schematic)` for the separate schematic/BOM review. |
| `estimate_cost.schematic` | Omit it. The estimator counts placed board footprints, which are the components relevant to assembly pricing. |

These removals narrow the schema to behavior Konnect can verify. They do not change
the generated files or analysis because the removed values had no implementation.
