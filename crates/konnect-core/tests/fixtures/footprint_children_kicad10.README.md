# Footprints owning groups, points and variants (#836)

`footprint_children_kicad10.kicad_pcb` is a board saved by pcbnew 10.0.6 and
not edited since. It was made in KiCad's GUI:

1. A new project with `Timer:NE555P` as U1, on `Package_DIP:DIP-8_W7.62mm`,
   and `Device:R` as R1, on `Resistor_SMD:R_0603_1608Metric`, placed with
   Update PCB from Schematic. The stock DIP-8 brings its own point.
2. U1 opened in the footprint editor, two of its silkscreen lines grouped,
   and the footprint saved back to the board: U1's own group.
3. U1 and R1 grouped on the board: the board-level group.
4. A design variant, `NoTimer`, in which U1 is DNP, then Update PCB from
   Schematic again, which writes U1's `(variant …)`.

So U1 owns a group, a point and a variant, R1 owns none of them, and both are
members of the board-level group. The tests answer `SaveDocumentToString` with
this text, as KiCad would for the board it holds.
