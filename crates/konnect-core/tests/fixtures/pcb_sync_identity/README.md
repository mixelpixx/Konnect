# PCB sync identity fixture

This independent fixture contains six resistor instances in three saved sheets:

| References | Location | Purpose |
|---|---|---|
| R1 | Root | Assigned footprint, root-page identity |
| R2 | Root | No assigned footprint |
| R3, R4 | ChannelB, ChannelA | One symbol UUID reused in two sheet instances |
| R5, R6 | ChannelB/Leaf, ChannelA/Leaf | One nested symbol UUID reused through both channels |

It contains no user design. It was built with Konnect's `create_project`,
`add_schematic_component`, `add_hierarchical_sheet` and `annotate_schematic`
MCP tools, using only the stock `Device:R` and
`Resistor_SMD:R_0603_1608Metric` libraries. KiCad 10.0.6 then loaded and
serialized each sheet:

```sh
kicad-cli sch upgrade --force pcb_sync_identity.kicad_sch
kicad-cli sch upgrade --force channel.kicad_sch
kicad-cli sch upgrade --force leaf.kicad_sch
kicad-cli sch export netlist --format kicadsexpr \
  --output pcb_sync_identity.net pcb_sync_identity.kicad_sch
```

The `.kicad_sch` files are that KiCad output. The `.net` file is the actual
export, with only the absolute `design/source` filename replaced by its
basename. Export dates and tool versions are informational. The resistors are
deliberately unwired; electrical completion is not the subject of these tests.

## Independent identity evidence

The root UUID is `957827ba-fa90-4f43-a1ca-8515c9493fca`. The tests' explicit
`IDENTITY_PATHS` table comes from the saved symbol instance paths plus each
symbol's UUID, independently of Konnect's netlist parser. KiCad's export
omits that root UUID on **every** sheet, including the nested reused leaves.
The local UUID of `channel.kicad_sch` is not the identity of either instance.

KiCad 10.0.6 provides the reason for preserving both representations:

- [`SCH_SHEET_PATH::PathAsString()`](https://github.com/KiCad/kicad-source-mirror/blob/10.0.6/eeschema/sch_sheet_path.cpp#L445-L458)
  skips the root; `Path()` in the same file retains a non-virtual root.
- [The netlist exporter](https://github.com/KiCad/kicad-source-mirror/blob/10.0.6/eeschema/netlist_exporters/netlist_exporter_xml.cpp#L573-L577)
  uses `PathAsString()` for `sheetpath/tstamps`.
- [The PCB updater](https://github.com/KiCad/kicad-source-mirror/blob/10.0.6/pcbnew/netlist_reader/board_netlist_updater.cpp#L480-L503)
  uses the component sheet path followed by the first symbol UUID. Existing
  native boards such as the repository's `konnect-sexp` `ecc83-pp.kicad_pcb`
  fixture carry root-relative paths. The fix must not assume every PCB path
  includes a root or rewrite an existing path merely to normalize it.

The test boards are controlled planner inputs with paths taken from this
table, either complete or with the exact root segment removed. They are not
claimed to be new live PCB captures. The served-dispatch test adapts the
existing real KiCad `issue_474_r1.ipc.bin` footprint capture to those identities, the assigned library ID
and exported pad nets, then serves it through the protobuf mock. It verifies
the real tool dispatch and absence of IPC writes, not a live-editor apply.
Foreign roots, foreign sheet/symbol UUIDs, prefix lookalikes, reference changes,
and duplicate aliases are deliberate test variations, not additional exports.
