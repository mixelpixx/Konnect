# `suggest_alternatives.tsv`

Thirty-three rows of the `components` table Konnect builds from the JLCPCB
catalogue feed, copied verbatim. The `suggest_jlcpcb_alternatives` tests
(#785) load them into a temporary database.

## Source

- Feed: `https://bouni.github.io/kicad-jlcpcb-tools`, the feed
  `download_jlcpcb_database` uses.
- Downloaded on 2026-10-03 by `download_jlcpcb_database` in a release build of
  `main` at `bf7bfd6`. The resulting database held 797,477 parts.
- Rows were selected by LCSC number. Every column is exactly as the import
  stored it, including a stored price of `0.0` where the feed gave none
  (#582).

The data is factual catalogue information: part numbers, packages, prices,
stock and descriptions published by JLCPCB and LCSC.

## Why these rows

| Case | Rows |
|---|---|
| `100nF` in `0402`: two Basic parts, then Extended parts; four of them have 8 to 19 units in stock | C1525, C307331, C285038, C359190, C285045, C2838746, C2932181, C3152530, C18255880 |
| `10k` in `0402`, with the `110kΩ` and `510kΩ` parts the old substring match returned and two `0402x4` arrays | C25744, C22356213, C174175, C25532, C25564, C227291, C170418, C7500235, C326872, C25725 |
| `20pF` with the `220pF` parts the old match returned | C1554, C107000, C1530, C107025 |
| A Preferred part (`510kΩ`) between Basic and Extended | C11616 |
| An unknown price (`1.5MΩ`, stored as `0.0`) beside known ones | C11812, C22369344, C138034 |
| `AMS1117-3.3` in `SOT-223` and `SOT-223-3` | C6186, C2992570 |
| `STM32F411CEU6`, listed as `UFQFPN-48(7x7)`, the part KiCad's STM32F411CEUx symbol places on `QFN-48-1EP_7x7mm` | C60420 |
| `LM358` in `SOIC-8` and `SOP-8` | C7950, C5423, C5252902 |

The stock and prices are a snapshot. The tests assert this snapshot's
ranking, not today's catalogue.
