# Title-block hierarchy fixture

KiCad's own `complex_hierarchy` demo
(`/usr/share/kicad/demos/complex_hierarchy/`, KiCad 10.0.6 package),
resaved by KiCad 10.0.6. Nothing is hand-written. The root places
`ampli_ht.kicad_sch` twice (`/ampli_ht_vertical/`, `/ampli_ht_horizontal/`),
so a hierarchy walk meets the same sheet file twice. It backs
`set_schematic_title_block` (#714).

## Provenance

```sh
cp /usr/share/kicad/demos/complex_hierarchy/{complex_hierarchy.kicad_sch,ampli_ht.kicad_sch,complex_hierarchy.kicad_pro} .
kicad-cli sch upgrade --force complex_hierarchy.kicad_sch   # resaves the root only
kicad-cli sch upgrade --force ampli_ht.kicad_sch
```

Both sheets are then `(version 20260306)`, `generator_version "10.0"`. A second
`upgrade --force` changes no byte. The `.kicad_pro` is the demo's, unchanged;
it makes the root the project root that owns `ampli_ht.kicad_sch`.

| File | SHA-256 |
|------|---------|
| `complex_hierarchy.kicad_sch` | `d923288f49bb4723c38e89a551c69a1c3134b17c5e05b66ed69a1ef121a6d783` |
| `ampli_ht.kicad_sch` | `a4aaab60cfedca73514b12a58506fbb64bcbb025c5603fd3ffbd5f70cbe6a493` |
| `complex_hierarchy.kicad_pro` | `55438416ca7fd46b5bad22da2693e9c43240060977d06daabcba084aa17e2804` |

## Oracle

`set_schematic_title_block` with `scope: "hierarchy"`, `revision: "B"`,
`date: "2026-10-20"`, `company: "Precision \"Racing\""`,
`comment_1: "${PROJECTNAME}"`, `comment_9: "ninth"`, run on a copy:

- Each file differs from the fixture only inside `(title_block …)`.
- `kicad-cli sch upgrade --force` on both edited files changes no byte, so the
  node is KiCad's own serialization.
- `kicad-cli sch export netlist` reports, for every sheet instance:

| Sheet | Source | title | date | rev | company | comment 1 | comment 9 |
|-------|--------|-------|------|-----|---------|-----------|-----------|
| `/` | `complex_hierarchy.kicad_sch` | Complex hierarchy: demo | 2026-10-20 | B | Precision "Racing" | complex_hierarchy | ninth |
| `/ampli_ht_vertical/` | `ampli_ht.kicad_sch` | Complex hierarchy: demo | 2026-10-20 | B | Precision "Racing" | complex_hierarchy | ninth |
| `/ampli_ht_horizontal/` | `ampli_ht.kicad_sch` | Complex hierarchy: demo | 2026-10-20 | B | Precision "Racing" | complex_hierarchy | ninth |

KiCad resolves `${PROJECTNAME}` in comment 1 to `complex_hierarchy`; the file
keeps the variable. `schematic_title_block_tests.rs` asserts the edited files
byte for byte against the fixture with that title block, which the resave
showed is KiCad's.
