# Written by aispice. Draws a layout plan with IHP's SG13G2 pcells and
# writes it as flat GDS. Inputs come as -rd variables: `pdk_python` (the
# PDK's libs.tech/klayout/python folder), `plan` (a JSON file aispice wrote)
# and `out` (the GDS to write). Nothing is evaluated from them; the plan is
# data, and its names and numbers are checked again here.
import json
import re
import sys

for p in (pdk_python + "/pycell4klayout-api/source/python", pdk_python):  # noqa: F821
    if p not in sys.path:
        sys.path.insert(0, p)

import pya  # noqa: E402
import sg13g2_pycell_lib  # noqa: E402,F401 (registers the SG13_dev library)

NAME = re.compile(r"^[A-Za-z0-9_$.<>-]{1,128}$")
LIMIT = 50_000_000


def integer(v):
    if not isinstance(v, int) or isinstance(v, bool) or abs(v) > LIMIT:
        raise ValueError("bad number in plan: %r" % (v,))
    return v


def um(nm):
    return "%gu" % (nm / 1000.0)


with open(plan) as f:  # noqa: F821
    data = json.load(f)

cell_name = data["cell"]
if not isinstance(cell_name, str) or not NAME.match(cell_name):
    raise ValueError("bad cell name")

if pya.Library.library_by_name("SG13_dev") is None:
    raise RuntimeError("the SG13_dev pcell library did not load")

ly = pya.Layout()
ly.dbu = 0.001
top = ly.create_cell(cell_name)

for inst in data["instances"]:
    pcell = inst["pcell"]
    if pcell not in ("nmos", "pmos"):
        raise ValueError("bad pcell %r" % (pcell,))
    w, l, ng = integer(inst["w"]), integer(inst["l"]), integer(inst["ng"])
    if ng < 1 or ng > 1000 or w <= 0 or l <= 0:
        raise ValueError("bad device size")
    c = ly.create_cell(pcell, "SG13_dev", {"w": um(w), "l": um(l), "ng": ng})
    if c is None:
        raise RuntimeError("pcell %s did not build" % pcell)
    top.insert(pya.CellInstArray(c.cell_index(), pya.Trans(integer(inst["x"]), integer(inst["y"]))))

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
    top.shapes(layer(l, d)).insert(pya.Box(integer(x1), integer(y1), integer(x2), integer(y2)))

for t in data["texts"]:
    l, d, text, x, y = t
    if not isinstance(text, str) or not NAME.match(text):
        raise ValueError("bad label")
    top.shapes(layer(l, d)).insert(pya.Text(text, pya.Trans(integer(x), integer(y))))

# One flat cell: the pcell variants are copied in and dropped.
top.flatten(True)
ly.write(out)  # noqa: F821
print("aispice: wrote %s" % cell_name)
