# Written by aispice. Puts a fill layout into a chip: reads the chip, reads
# the fill (its cells renamed where they clash), places the fill's top cell
# at the origin of the chip's top cell and writes the result. Inputs come as
# -rd variables: `chip`, `fill` and `out` (paths), `top` and `fill_top`
# (cell names); they are checked here and never evaluated.
import os
import re

import pya

CELL = re.compile(r"^[A-Za-z_][A-Za-z0-9_]{0,127}$")


def path(v):
    if not os.path.isabs(v) or any(ord(c) < 32 for c in v):
        raise ValueError("bad path")
    return v


def name(v):
    if not CELL.match(v):
        raise ValueError("bad cell name")
    return v


ly = pya.Layout()
ly.read(path(chip))  # noqa: F821
chip_top = ly.cell(name(top))  # noqa: F821
if chip_top is None:
    raise RuntimeError("the chip has no top cell %s" % top)  # noqa: F821
options = pya.LoadLayoutOptions()
options.cell_conflict_resolution = pya.LoadLayoutOptions.RenameCell
before = set(c.cell_index() for c in ly.each_cell())
ly.read(path(fill), options)  # noqa: F821
added = [
    c
    for c in ly.top_cells()
    if c.cell_index() not in before and c.name.startswith(name(fill_top))  # noqa: F821
]
if len(added) != 1:
    raise RuntimeError("the fill layout has no single top cell %s" % fill_top)  # noqa: F821
chip_top.insert(pya.CellInstArray(added[0].cell_index(), pya.Trans(0, 0)))
for c in list(ly.top_cells()):
    if c.cell_index() != chip_top.cell_index():
        c.prune_cell()
save = pya.SaveLayoutOptions()
save.write_context_info = False
ly.write(path(out), save)  # noqa: F821
print("aispice: merged the fill")
