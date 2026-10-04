# Board item bounds goldens (#688)

KiCad 10.0.5's own answers for where every item of seven checked-in boards lies:
one row per item, as `GetBoundingBox` in `BBM_ITEM_ONLY` mode returned it over
KiCad's IPC API, with the board open in pcbnew. `tests/board_bounds.rs` checks
the saved-file measurement in `src/bounds.rs` against them, item by item.

| Golden | Board | Rows |
|---|---|---|
| `ecc83-pp.tsv` | `../ecc83-pp.kicad_pcb` | 112 |
| `pic_programmer.tsv` | `../pic_programmer.kicad_pcb` | 711 |
| `RoyalBlue54L-NFC-Antenna.tsv` | `../RoyalBlue54L-NFC-Antenna.kicad_pcb` | 187 |
| `placement_fixture.tsv` | `../placement/placement_fixture.kicad_pcb` | 63 |
| `bounds_annotation_layers.tsv` | `bounds_annotation_layers.kicad_pcb` | 24 |
| `bounds_zone_arcs.tsv` | `bounds_zone_arcs.kicad_pcb` | 2 |
| `bounds_text_boxes.tsv` | `bounds_text_boxes.kicad_pcb` | 52 |

Two of the boards are cut from larger KiCad demos, because no committed board
exercises the rule they pin:

- `bounds_annotation_layers.kicad_pcb` holds two footprints from
  `jetson-agx-thor-baseboard/jetson-agx-thor-baseboard.kicad_pcb` (SHA-256
  `f34dbf61…97ec57`): the Jetson module and the SFP28 connector. Both draw on
  annotation layers beyond their copper, which moves their box by up to 2.95 mm
  if those layers are counted. That demo is Copyright (c) 2025-2026 Antmicro,
  licensed Apache-2.0 (the demo's `LICENSE`).
- `bounds_zone_arcs.kicad_pcb` holds two zones with arc segments in their
  outlines from `royalblue54L_feather/RoyalBlue54L-Feather.kicad_pcb` (SHA-256
  `5f54a63b…52f6f1`), licensed CERN-OHL-P v2 (the demo's `LICENSE`).

`bounds_text_boxes.kicad_pcb` is written by KiCad itself (#688). No demo board
has a board text box, and the only stock footprints with one put it on an
annotation layer, so the board was built for the rule. A source file holding
the items below was opened in `pcbnew.exe` 10.0.5. KiCad's own serialization of
it (`SaveDocumentToString`) is the committed file, byte for byte. Reopened, it
re-serializes to the same bytes.

- **Board text boxes:** borders of 0.2 mm and 1.5 mm, no border, a hidden 1 mm
  border, a box turned 30° (KiCad writes it as four `pts`), and text overflowing
  a small box.
- **Footprint text boxes:**
  - a one-pad footprint turned 30°, with its text box on `F.Fab`;
  - the same footprint unturned, with its text box on `Dwgs.User`;
  - the stock `RF_Module:Raytac_MDBT42Q` from KiCad 10.0.5's library, with two
    text boxes on `Dwgs.User` and two keep-out zones. Its zones were moved to
    board coordinates, as a board stores a footprint's zones. The KiCad
    libraries are CC BY-SA 4.0 with an exception that waives its conditions for
    designs using the footprints, which this board is.

Everything else on the board is Konnect's own.

Each cut board is the source's own header (version through nets), then the
selected top-level items, byte for byte, then the closing paren.
`kicad-cli pcb drc` 10.0.5 loads both. Their goldens were captured from the cut
boards themselves, and every row is identical to KiCad's answer for the same item
on the full source board.

Each file's header records the SHA-256 of the board it was captured from.
Columns are `class`, `uuid` (the KIID), and the box's `x`, `y`, `w`, `h` in
nanometres, exactly as KiCad returned them. KiCad's `GetItems` was asked for
footprints, shapes, text, text boxes, tracks, arcs, vias, zones, dimensions,
barcodes, reference images and pads, one class per request. Every KIID it
returned was then sent in one `GetBoundingBox` request.

**How they were captured:**

1. A throwaway `#[ignore]` test attached to `konnect-ipc`'s client, not
   committed, ran against a copy of each board.
2. Each copy was opened alone in `pcbnew.exe` 10.0.5 on Windows 11.
3. The copy was closed without saving.

**Before trusting a change to these files:** re-capture against KiCad rather
than editing rows by hand. A row edited to match the code would make the test
prove nothing.
