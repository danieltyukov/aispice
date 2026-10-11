# Written by aispice. Draws a cell of a GDS file to a PNG with the PDK's
# layer colours. Inputs come as -rd variables: `gds`, `cell`, `lyp` (the
# PDK's layer properties file), `out` (the PNG), `width` and `height`.
# Nothing is evaluated from them.
import re

import pya

NAME = re.compile(r"^[A-Za-z0-9_$.<>-]{1,128}$")
if not NAME.match(cell):  # noqa: F821
    raise ValueError("bad cell name")
w = int(width)  # noqa: F821
h = int(height)  # noqa: F821
if not (64 <= w <= 4096 and 64 <= h <= 4096):
    raise ValueError("bad image size")

view = pya.LayoutView()
view.set_config("background-color", "#ffffff")
view.set_config("grid-visible", "false")
view.set_config("text-visible", "true")
view.set_config("default-text-size", "0.4")
index = view.load_layout(gds, True)  # noqa: F821
if lyp:  # noqa: F821
    view.load_layer_props(lyp)  # noqa: F821
layout = view.cellview(index).layout()
target = layout.cell(cell)  # noqa: F821
if target is None:
    raise ValueError("no such cell")
view.select_cell(target.cell_index(), index)
view.max_hier()
view.zoom_fit()
view.save_image(out, w, h)  # noqa: F821
print("aispice: rendered")
