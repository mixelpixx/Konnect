# Title-block fixtures

Every file here is a pcbnew 10.0.6 save (`version 20260206`), built with KiCad's
own Python API. None is hand-written. They back `konnect_sexp::title_block` and
`set_board_title_block` (#714).

`board.kicad_pcb` is the base board. Each other file is what KiCad itself saved
after loading `board.kicad_pcb` (or `board_empty.kicad_pcb`), making one edit
through `BOARD.GetTitleBlock()` / `SetTitleBlock()`, and saving. Konnect's file
path must produce exactly the same bytes for the same edit. A plain
load-and-save of `board.kicad_pcb` is byte-identical to it, so every difference
between KiCad's input and output files is the title block.

| File | Made from | KiCad edit | What it shows |
|------|-----------|------------|---------------|
| `board.kicad_pcb` | new `BOARD()` | title `Rig "Interface" ${PROJECTNAME}`, date `2026-10-20`, revision `B`, company `C:\new\tab`, comments 1, 3 and 9 | quoting of `"` and `\`, a text variable, `rev` for revision, comment gaps |
| `board_empty.kicad_pcb` | new `BOARD()` | none | a board with no `(title_block …)` node |
| `board_edited.kicad_pcb` | `board.kicad_pcb` | revision `C`, comment 2 `second`, comment 3 cleared | replacing fields in place, KiCad's field order |
| `board_cleared.kicad_pcb` | `board.kicad_pcb` | every field cleared | KiCad drops the whole node |
| `board_inserted.kicad_pcb` | `board_empty.kicad_pcb` | title `Fresh`, comment 4 `fourth` | KiCad inserts the node right after `(paper …)` |

KiCad also writes a title of `a<LF>b<TAB>c` as `(title "a\nb<TAB>c")`: line
breaks are escaped and tabs are kept. That save is not committed. The
`line_breaks_are_escaped_and_tabs_kept` test asserts it.

## Recreate

```python
# python3 -I make.py <dir>   (KiCad 10.0.6 pcbnew module)
import sys, pcbnew
d = sys.argv[1]

b = pcbnew.BOARD(); b.SetCopperLayerCount(2)
tb = b.GetTitleBlock()
tb.SetTitle('Rig "Interface" ${PROJECTNAME}'); tb.SetDate("2026-10-20")
tb.SetRevision("B"); tb.SetCompany("C:\\new\\tab")
tb.SetComment(0, "first"); tb.SetComment(2, "third"); tb.SetComment(8, "ninth")
s = pcbnew.PCB_SHAPE(b); s.SetShape(pcbnew.SHAPE_T_RECT)
s.SetStart(pcbnew.VECTOR2I(0, 0))
s.SetEnd(pcbnew.VECTOR2I(pcbnew.FromMM(20), pcbnew.FromMM(10)))
s.SetLayer(pcbnew.Edge_Cuts); s.SetWidth(pcbnew.FromMM(0.1)); b.Add(s)
pcbnew.SaveBoard(f"{d}/board.kicad_pcb", b)

e = pcbnew.BOARD(); e.SetCopperLayerCount(2)
pcbnew.SaveBoard(f"{d}/board_empty.kicad_pcb", e)

def edit(src, out, change):
    b = pcbnew.LoadBoard(f"{d}/{src}"); tb = b.GetTitleBlock()
    change(tb); b.SetTitleBlock(tb); pcbnew.SaveBoard(f"{d}/{out}", b)

def edited(tb):
    tb.SetRevision("C"); tb.SetComment(2, ""); tb.SetComment(1, "second")
def cleared(tb):
    for f in (tb.SetTitle, tb.SetDate, tb.SetRevision, tb.SetCompany): f("")
    for i in range(9): tb.SetComment(i, "")
def inserted(tb):
    tb.SetTitle("Fresh"); tb.SetComment(3, "fourth")

edit("board.kicad_pcb", "board_edited.kicad_pcb", edited)
edit("board.kicad_pcb", "board_cleared.kicad_pcb", cleared)
edit("board_empty.kicad_pcb", "board_inserted.kicad_pcb", inserted)
```

The board's UUIDs are random, so a rerun gives different files with the same
title-block bytes.

## SHA-256

```
94b96cfee951884d0effd047838486a968262a9ef04326debb4167bf41aec031  board.kicad_pcb
18fd24fb61cadb9cd703ed16647ce12846d9004c76e25f3c18d3383ec13a4e48  board_cleared.kicad_pcb
1b7d29f9a93bf5060df9a3e46c9fbf994d20762809881ed2570b8ac3e26b88b3  board_edited.kicad_pcb
7829957e7338e332a94dc7033609c47f00a6f41b9c34c8ad4c085579b7a6338f  board_empty.kicad_pcb
b8bc82807a2710cd44889ae698ccf5270ff96e21d1e979f79eea19db10f409e1  board_inserted.kicad_pcb
```
