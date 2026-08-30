---
name: kicad-review
description: |
  Design review and validation workflow for KiCAD projects via MCP tools. Triggers on: "review my design",
  "check for errors", "audit", "DRC", "ERC", "find problems", "design review", "is this ready",
  "validate", "check my schematic", "check my PCB", "what's wrong", "run checks", "pre-fab review".
argument-hint: "[what to review]"
---

# KiCAD Design Review & Validation Workflow

This skill guides Claude through systematic design review of a KiCAD project using MCP tools.
ALL checks are performed through MCP tools — never parse .kicad_sch or .kicad_pcb files directly.

---

## Toolset Loading

Load the required toolsets for design review:

```
load_toolset('sch_analysis')     # find_orphan_items, find_shorted_nets, find_single_pin_nets
load_toolset('verification')     # run_drc, check_clearance, get_design_rules
load_toolset('sch_export')       # run_erc
load_toolset('pcb_export')       # get_drc_violations
load_toolset('manufacturing')    # validate_for_manufacturing
load_toolset('design_review')    # audit_decoupling, audit_connections, audit_power_rails, etc.
```

Optional (for deeper analysis):

```
load_toolset('sch_analysis')     # get net info, trace connections, inspect components
load_toolset('pcb_routing')      # query_traces, get_nets_list
```

Always call `get_active_toolsets()` first to see what is already loaded.

## Evidence hierarchy

Judge findings in this order:

1. Exact design requirements and manufacturer datasheets.
2. Direct KiCad ERC/DRC and saved or exported connectivity.
3. Direct Konnect net, short, pad, trace, via, unrouted, and inventory evidence.
4. Aggregate review and manufacturing summaries.
5. Heuristic orphan, single-pin, decoupling, protection, and best-practice findings.

A weaker finding may ask a question; it does not override stronger contradictory
evidence. Any required check that did not run, returned impossible coverage, or
remains inconsistent with stronger evidence makes the verdict `INCOMPLETE`.

## References by review branch

- Read [`references/design-checklist.md`](references/design-checklist.md) for a
  comprehensive or pre-fabrication review. Mark an item only from evidence
  collected in this run.
- Read [`references/error-taxonomy.md`](references/error-taxonomy.md) when
  classifying a finding or assigning the final verdict. Direct ERC, DRC, and
  connectivity evidence outrank heuristic classifications.

---

## Quick Checks (Escalating Severity)

Run these first — they are fast and catch the most critical issues.

### Level 1: Structural Integrity

```
find_orphan_items()
```

Finds floating wires, labels, and symbols not connected to anything. Treat the
result as a heuristic candidate list and corroborate it with direct connectivity
or ERC before calling an item a defect.

### Level 2: Critical Net Issues

```
find_shorted_nets()
```

Detects nets that are connected together but should not be. A shorted net means:
- Two different net labels on the same wire
- Power rails bridged unintentionally
- Signal nets merged by accident

A confirmed unintended short is critical. Resolve disagreement with requirements
or direct ERC/connectivity evidence before assigning severity.

### Level 3: Suspicious Connections

```
find_single_pin_nets()
```

A one-pin net is a heuristic review candidate:
- Incomplete wiring (forgot to connect the other end)
- Orphan net labels (typo in name, so it does not match)
- Leftover stubs from deleted components

---

## Formal Checks

### ERC — Electrical Rules Check

```
run_erc()
```

Checks schematic-level rules:
- Pin type conflicts (output driving output, unconnected inputs)
- Power pin connections
- Missing no-connect flags
- Duplicate reference designators
- Missing net connections

Review each violation. Some can be waived (e.g., intentional unconnected pins marked with no-connect flag).

### DRC — Design Rules Check

```
get_drc_violations()
```

Checks PCB-level rules:
- Clearance violations (copper-to-copper, copper-to-edge)
- Minimum trace width violations
- Minimum drill size violations
- Unrouted connections (incomplete routing)
- Zone fill issues
- Courtyard overlaps

**Every DRC error must be resolved or explicitly justified before manufacturing.**

---

## Design Audits

These go beyond rule checking — they evaluate design quality and best practices.

The standalone schematic audits and `check_bom_health` default to the supplied
file only. When the supplied file is a hierarchy root, pass
`schematic_scope: "hierarchy"` to cover every reachable sheet instance. Read
`status`, `coverage`, and `diagnostics` before interpreting a hierarchy result;
missing or cyclic child references make the result incomplete. Reused child
files have one result per KiCad sheet instance, identified by the
`sheet_instance_path` response field.

### Decoupling Audit

```
audit_decoupling(schematic, schematic_scope="hierarchy")
```

Checks:
- Every IC power pin has a bypass capacitor
- Capacitor is placed close to the pin (PCB proximity)
- Appropriate capacitor values (100nF ceramic minimum)
- Bulk capacitance present for high-current ICs

### Connection Audit

```
audit_connections(schematic, schematic_scope="hierarchy")
```

Checks:
- All expected connections are made
- No nets with unexpected fan-out
- Signal integrity basics (termination on long traces)
- Pull-up/pull-down resistors where required (I2C, reset pins, enable pins)

### Power Rail Audit

```
audit_power_rails(schematic, schematic_scope="hierarchy")
```

Checks:
- All power rails have proper source (regulator, connector, etc.)
- Current capacity matches expected load
- Voltage levels are consistent (no 3.3V device on 5V rail)
- Power sequencing considered for multi-rail designs
- Power flags present (avoids ERC false positives)

### Manufacturing Audit

```
audit_manufacturing()
```

Checks:
- All footprints are fab-house compatible
- Pad sizes meet minimum requirements
- Silkscreen readability
- Test point accessibility
- Fiducial marks present (for SMT assembly)
- Mechanical clearances around mounting holes

---

## Full Review Shortcut

```
run_design_review()
```

Runs the aggregate design audits and produces a consolidated report. Use it to
organize findings, not as a substitute for the direct ERC, DRC, and connectivity
checks above.

Read `status`, `coverage`, and `diagnostics` before interpreting the findings.
If `status` is `partial` or `failed`, the verdict is `INCOMPLETE — review could
not evaluate the full design`. Report that verdict verbatim, explain the
diagnostics and unevaluated coverage, and do not describe the design as ready,
passing, clean, or looking good. Findings gathered before the coverage gap are
still valid and should still be reported.

Collect direct `run_erc`, `get_drc_violations`, short, and connectivity evidence
separately. Then compare the aggregate findings with that stronger evidence and
report any disagreement.

---

## Severity Classification

### CRITICAL — Must fix before manufacturing

| Finding                            | Why Critical                                    |
|------------------------------------|-------------------------------------------------|
| Shorted nets                       | Short circuit on the board, may damage components |
| Missing ground connection          | Circuit will not function                       |
| Reversed polarity on power IC      | Immediate destruction on power-up               |
| Unrouted nets                      | Missing connections on fabricated board          |
| DRC clearance violation            | May cause electrical short on fab board          |
| Power pin unconnected              | IC will not operate                             |
| Wrong voltage on IC power pin      | Exceeds absolute maximum, destroys part         |

### WARNING — Should fix, design risk

| Finding                            | Why a Warning                                   |
|------------------------------------|-------------------------------------------------|
| Missing decoupling capacitor       | Noise susceptibility, possible oscillation      |
| No test points on key signals      | Cannot debug in production                      |
| No ESD protection on connectors    | Vulnerable to ESD damage in the field           |
| Single-point-of-failure nets       | No redundancy for critical signals              |
| Pull-up/pull-down missing          | Floating input, unpredictable behavior          |
| Tight clearances (near DRC limit)  | Higher fab defect rate                          |

### SUGGESTION — Improvement opportunities

| Finding                            | Why a Suggestion                                |
|------------------------------------|-------------------------------------------------|
| Consolidate passive values         | Fewer unique BOM lines, lower assembly cost     |
| Add net labels to unnamed nets     | Improves schematic readability                  |
| Missing silkscreen designators     | Harder to assemble and debug manually           |
| Components could be closer         | Shorter traces, better signal integrity         |
| Consider bulk capacitor addition   | Better transient response on power rails        |
| Add board revision marking         | Traceability for manufacturing runs             |

---

## Reporting Format

Present findings grouped by severity with actionable fix suggestions:

```
## Design Review Results

### CRITICAL (X issues) — Must fix

1. **[Finding title]**
   - Location: [component reference or net name]
   - Issue: [what is wrong]
   - Fix: [specific action to take using MCP tools]

### WARNING (X issues) — Should fix

1. **[Finding title]**
   - Location: [component reference or net name]
   - Issue: [what is wrong]
   - Fix: [specific action to take]

### SUGGESTION (X items) — Optional improvements

1. **[Finding title]**
   - Detail: [what could be better]
   - Action: [suggested improvement]

### Summary
- Critical: X (must resolve)
- Warnings: X (recommended)
- Suggestions: X (optional)
- Verdict: [LOOKS GOOD / NEEDS ATTENTION / NOT READY / INCOMPLETE]
- Coverage status: [complete / partial / failed]
- Coverage diagnostics: [none, or each unevaluated sheet/object/audit]
```

---

## Review Workflow

### Quick Review (5-minute check)

1. `find_shorted_nets()` — catch fatal issues
2. `run_erc()` — schematic rule check
3. `get_drc_violations()` — PCB rule check
4. Report findings

### Full Review (comprehensive)

1. Load all review toolsets
2. Run direct short/connectivity checks, `run_erc`, and `get_drc_violations`
3. `run_design_review()` — aggregate audit suite
4. Check `status`, `coverage`, and `diagnostics`; never approve an incomplete review
5. Reconcile aggregate or heuristic findings with stronger direct evidence
6. Classify all gathered findings by severity
7. Present report with fix suggestions
8. Offer to fix CRITICAL issues immediately

### Pre-Manufacturing Review

1. Full review (above)
2. Run `validate_for_manufacturing()`, then inspect `verdict`, `issues`, and
   `drc` against the handler's limited contract; it does not replace outline,
   drill, silkscreen, artifact, BOM/CPL, or order-preview acceptance
3. Verify BOM completeness
4. Check part availability (if targeting specific fab house)
5. Final verdict: ready to manufacture or not

---

## Rules

1. **Never skip quick checks** — find_shorted_nets catches the worst bugs fast
2. **Classify every finding** — severity helps the user prioritize
3. **Provide specific fixes** — name the MCP tool and parameters to resolve each issue
4. **Run DRC after fixes** — verify that corrections did not introduce new violations
5. **Do not approve a design with CRITICAL issues** — even if the user says "it's fine"
6. **Load toolsets first** — check `get_active_toolsets()` and load what you need
7. **Save before reviewing** — ensures checks run against current state
8. **Offer to fix** — after reporting, offer to use MCP tools to resolve issues
9. **Re-run after fixes** — always verify fixes resolved the issue and created no new ones
10. **Document waivers** — if user explicitly waives a warning, note it in the report
11. **Never soften `INCOMPLETE`** — partial or failed coverage is not a passing review
