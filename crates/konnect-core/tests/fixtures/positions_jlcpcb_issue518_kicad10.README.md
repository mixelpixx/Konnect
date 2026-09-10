# JLCPCB issue #518 KiCad position fixture

`positions_jlcpcb_issue518_kicad10.csv` was captured from KiCad 10.0.6 on
2026-09-10 with:

```text
kicad-cli pcb export pos --format csv --units mm --side both --exclude-dnp --output <fixture> konnect-jlcpcb-blinker.kicad_pcb
```

The source board is the exact benchmark design used for the JLCPCB uploader
validation recorded in Konnect issue #518. JLCPCB accepted the uncorrected CPL
but rendered U1 (`SOIC-8_3.9x4.9mm_P1.27mm`) 90 degrees off. The fixture keeps
KiCad's native `Package` column so correction matching is tested after the real
geometry export rather than against a hand-authored approximation.
