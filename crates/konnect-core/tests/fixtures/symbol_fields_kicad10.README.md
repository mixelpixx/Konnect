# Symbol fields fixture

`symbol_fields_kicad10.kicad_sym` holds two library symbols, one with every
field `create_symbol` can write and one with none, so `create_symbol` and
`set_symbol_properties` can be checked against KiCad's own reading of the
fields (#892). `symbol_fields_kicad10.edited.kicad_sym` is the same library
after two `set_symbol_properties` calls, as KiCad resaves it.

## Provenance

Both symbols were created over stdio by the `create_symbol` tool, then
force-resaved by KiCad 10.0.6:

```text
create_symbol FIELDS_FULL  value TPS7A02, datasheet https://www.ti.com/lit/ds/symlink/tps7a02.pdf,
                           footprint Package_TO_SOT_SMD:SOT-23-5,
                           description "200 mA LDO, fixture symbol with every field",
                           keywords "ldo regulator", fp_filters ["SOT?23*"],
                           fields {MPN: TPS7A0233PDBVR, LCSC: C2862740}
create_symbol FIELDS_BARE  value CH347F, no other field
kicad-cli sym upgrade --force symbol_fields_kicad10.kicad_sym -o symbol_fields_kicad10.kicad_sym
```

The edited file is that library after these calls, resaved the same way:

```text
set_symbol_properties FIELDS_BARE  footprint Package_DFN_QFN:QFN-20-1EP_3x3mm_P0.4mm_EP1.65x1.65mm,
                                   description 'USB 2.0 to "UART\JTAG" bridge',
                                   keywords "usb uart jtag",
                                   fp_filters ["QFN*3x3mm*P0.4mm*", "TSSOP*"],
                                   fields {Manufacturer: WCH, MPN: CH347F}
set_symbol_properties FIELDS_FULL  fields {MPN: TPS7A0230PDBVR}, keywords "", fp_filters [],
                                   description unchanged
kicad-cli sym upgrade --force edited.kicad_sym -o symbol_fields_kicad10.edited.kicad_sym
```

Custom fields are written in name order, so `LCSC` comes before `MPN`.
KiCad keeps fields in file order on a resave.

KiCad's resave of the tool's output was byte-identical to that output, so the
edited fixture is both what the tool writes and what KiCad writes.

## Oracle

KiCad's resave lists these fields. A field it did not parse would not be
written back.

| Symbol | Field | Input fixture | Edited fixture |
|---|---|---|---|
| `FIELDS_FULL` | `Footprint` | `Package_TO_SOT_SMD:SOT-23-5` | unchanged |
| `FIELDS_FULL` | `Description` | `200 mA LDO, fixture symbol with every field` | unchanged |
| `FIELDS_FULL` | `LCSC` | `C2862740` | unchanged |
| `FIELDS_FULL` | `MPN` | `TPS7A0233PDBVR` | `TPS7A0230PDBVR` |
| `FIELDS_FULL` | `ki_keywords` | `ldo regulator` | absent |
| `FIELDS_FULL` | `ki_fp_filters` | `SOT?23*` | absent |
| `FIELDS_BARE` | `Footprint` | empty | `Package_DFN_QFN:QFN-20-1EP_3x3mm_P0.4mm_EP1.65x1.65mm` |
| `FIELDS_BARE` | `Description` | empty | `USB 2.0 to "UART\JTAG" bridge` |
| `FIELDS_BARE` | `MPN`, `Manufacturer` | absent | `CH347F`, `WCH`, hidden |
| `FIELDS_BARE` | `ki_keywords` | absent | `usb uart jtag` |
| `FIELDS_BARE` | `ki_fp_filters` | absent | `QFN*3x3mm*P0.4mm* TSSOP*` |

KiCad writes custom fields after `Description` and before `ki_keywords`, then
`ki_fp_filters` last. It writes no `ki_keywords` or `ki_fp_filters` property
when the value is empty, which is why an empty `keywords` or `fp_filters`
removes the property.
