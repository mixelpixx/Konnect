# Sheet instance path fixture (#478)

A one-project hierarchy whose child files carry the instance paths Konnect
wrote before #478 was fixed: a path naming a deleted sheet beside the live one,
and an unplaced file whose only path names a deleted sheet.

## Provenance

Built over stdio with the `main` build at `9488e5f`, against KiCad's stock
`Device:R`:

1. `create_project` `demo`.
2. `add_hierarchical_sheet` Child → `child.kicad_sch`, `add_schematic_component`
   R1 into it, `delete_sheet` Child, `add_hierarchical_sheet` Child again.
3. `add_hierarchical_sheet` Shared A and Shared B, both → `shared.kicad_sch`,
   then `add_schematic_component` R2 into it.
4. `add_hierarchical_sheet` Mid → `mid.kicad_sch`, left empty.
5. `add_hierarchical_sheet` Leaf → `leaf.kicad_sch`, `add_schematic_component`
   R3 into it, `delete_sheet` Leaf.

Then every file was force-resaved by KiCad 10.0.6. The board file was not kept.

```text
kicad-cli sch upgrade --force demo.kicad_sch   # and child, shared, mid, leaf
```

## KiCad's own answer

```text
kicad-cli sch export netlist --output demo.net demo.kicad_sch
```

| Sheet | `tstamps` KiCad resolves | Symbol | Paths the file records under `demo` |
|---|---|---|---|
| Child | `/a92b6a1a-7166-40e6-b18f-7a9e05809f8e/` | R1 | that sheet, and `8cfbc41c-…`, which is no sheet |
| Shared A | `/cec11030-6492-493f-9861-1e0342d636b0/` | R2 | both Shared sheets |
| Shared B | `/960dfe25-e6b4-4474-bf97-e779fae4b5e1/` | R2 | (same symbol) |
| Mid | `/5c0eb261-ed77-46f8-b89a-8b0c68833e04/` | — | — |
| (none) | `leaf.kicad_sch` is not placed | R3 | `824b7475-…` only, which is no sheet |

The root UUID is `d0dc0cde-fd00-4536-b512-b225e546d529`. Every path in the
files is that UUID followed by KiCad's `tstamps`. ERC reports only
`pin_not_connected` on the unwired resistors.

After the fixed build adds Leaf under Mid, deletes Shared A, and deletes and
re-adds Child, the netlist resolves R3 at `/Mid/Leaf/` (`tstamps`
`/5c0eb261-…/<new Leaf uuid>/`), R2 at `/Shared B/` and R1 at the new Child.
Each symbol then records exactly `/d0dc0cde-…` plus that `tstamps`, and nothing
else.
