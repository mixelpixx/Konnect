# KiCad 10 position export fixture

`positions_pic_programmer_kicad10.csv` was generated verbatim with KiCad
10.0.6 from the existing real-board fixture at
`crates/konnect-sexp/tests/fixtures/pic_programmer.kicad_pcb`:

```text
kicad-cli pcb export pos --output positions_pic_programmer_kicad10.csv --format csv --units mm --side both crates/konnect-sexp/tests/fixtures/pic_programmer.kicad_pcb
```

It intentionally covers a bottom-side component, negative and positive
rotations, through-hole footprints, UTF-8, and a quoted value containing a
comma. The JLCPCB conversion test uses the native file unchanged rather than a
hand-authored approximation of KiCad's CSV dialect.
