# Written by aispice. Assembles a chip from a plan aispice wrote: the cells
# of the GDS files the plan names (the PDK's IO library, each block's
# layout), instances of them, IHP's bond pad and seal ring pcells, the
# top-level routes and labels. Writes the chip and its LVS view: the same
# chip without the IO cells, bond pads and seal ring.
#
# Inputs come as -rd variables: `pdk_python` (the PDK's
# libs.tech/klayout/python folder) and `plan` (a JSON file aispice wrote).
# Nothing is evaluated from them; names, numbers and paths in the plan are
# checked again here.
import json
import os
import re
import sys

for p in (pdk_python + "/pycell4klayout-api/source/python", pdk_python):  # noqa: F821
    if p not in sys.path:
        sys.path.insert(0, p)

import pya  # noqa: E402
import sg13g2_pycell_lib  # noqa: E402,F401 (registers the SG13_dev library)

CELL = re.compile(r"^[A-Za-z_][A-Za-z0-9_]{0,127}$")
LABEL = re.compile(r"^[A-Za-z0-9_$.<>\[\]-]{1,128}$")
VALUE = re.compile(r"^[A-Za-z0-9_.]{1,32}$")
PCELLS = ("bondpad", "sealring")
LIMIT = 10**9


def integer(v):
    if not isinstance(v, int) or isinstance(v, bool) or abs(v) > LIMIT:
        raise ValueError("bad number in plan: %r" % (v,))
    return v


def name(v, rule=CELL):
    if not isinstance(v, str) or not rule.match(v):
        raise ValueError("bad name in plan: %r" % (v,))
    return v


def path(v):
    if (
        not isinstance(v, str)
        or not os.path.isabs(v)
        or any(ord(c) < 32 for c in v)
    ):
        raise ValueError("bad path in plan")
    return v


with open(plan) as f:  # noqa: F821
    data = json.load(f)

if pya.Library.library_by_name("SG13_dev") is None:
    raise RuntimeError("the SG13_dev pcell library did not load")

ly = pya.Layout()
ly.dbu = 0.001
options = pya.LoadLayoutOptions()
# A cell of a later file never merges into an earlier one of the same name;
# it is renamed, so two blocks cannot overwrite each other's cells.
options.cell_conflict_resolution = pya.LoadLayoutOptions.RenameCell
for lib in data["libraries"]:
    ly.read(path(lib["file"]), options)
    for c in lib["cells"]:
        if ly.cell(name(c)) is None:
            raise RuntimeError("%s has no cell %s" % (lib["file"], c))

top_name = name(data["top"])
if ly.cell(top_name) is not None:
    raise RuntimeError("the chip's name is already a cell of a library or block")
top = ly.create_cell(top_name)

for inst in data["instances"]:
    cell = ly.cell(name(inst["cell"]))
    if cell is None:
        raise RuntimeError("no cell %s" % inst["cell"])
    rot = integer(inst["rot"])
    if rot not in (0, 1, 2, 3) or not isinstance(inst["mirror"], bool):
        raise ValueError("bad placement")
    trans = pya.Trans(rot, inst["mirror"], integer(inst["x"]), integer(inst["y"]))
    top.insert(pya.CellInstArray(cell.cell_index(), trans))

pcell_cells = set()
for pc in data["pcells"]:
    kind = pc["pcell"]
    if kind not in PCELLS:
        raise ValueError("bad pcell %r" % (kind,))
    params = {}
    for k, v in pc["params"].items():
        params[name(k)] = name(v, VALUE)
    cell = ly.create_cell(kind, "SG13_dev", params)
    if cell is None:
        raise RuntimeError("pcell %s did not build" % kind)
    pcell_cells.add(cell.cell_index())
    top.insert(
        pya.CellInstArray(
            cell.cell_index(), pya.Trans(integer(pc["x"]), integer(pc["y"]))
        )
    )

layers = {}


def layer(l, d):
    key = (integer(l), integer(d))
    if not (0 <= key[0] < 256 and 0 <= key[1] < 256):
        raise ValueError("bad layer")
    if key not in layers:
        layers[key] = ly.layer(key[0], key[1])
    return layers[key]


for s in data["shapes"]:
    l, d, x1, y1, x2, y2 = s
    top.shapes(layer(l, d)).insert(
        pya.Box(integer(x1), integer(y1), integer(x2), integer(y2))
    )
for t in data["texts"]:
    l, d, text, x, y = t
    top.shapes(layer(l, d)).insert(
        pya.Text(name(text, LABEL), pya.Trans(integer(x), integer(y)))
    )


def prune_others():
    for c in list(ly.top_cells()):
        if c.cell_index() != top.cell_index():
            c.prune_cell()


# Cells nobody places (the IO library's gallery, unused IO cells) go.
prune_others()
save = pya.SaveLayoutOptions()
save.write_context_info = False
ly.write(path(data["out"]), save)

drop = set(name(c) for c in data["lvs_drop"])
for inst in list(top.each_inst()):
    ci = inst.cell_index
    if ci in pcell_cells or ly.cell(ci).name in drop:
        inst.delete()
prune_others()
ly.write(path(data["lvs_out"]), save)
print("aispice: assembled %s" % top_name)
