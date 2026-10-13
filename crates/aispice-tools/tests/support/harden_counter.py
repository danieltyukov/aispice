#!/usr/bin/env python3
"""Harden a two-bit counter with LibreLane on IHP SG13G2 and package it as a
macro/v1 package, the way a digital tool hands a block to aispice.

The chip acceptance test (crates/aispice-tools/tests/chip_ihp.rs) places the
package when AISPICE_DIGITAL_MACRO names it. The counter has the shared
fixture's interface (clk, rst_n, en, q[1:0], VDD, VSS). Its power grid is one
layer of Metal4 straps that reach the block's edges, and its IO pins sit on
the edges on Metal2 and Metal3, so aispice's chip router reaches every pin
from outside the block.

The package holds PDK cell layouts, so it is a test output, never committed.
Its GDS keeps the standard cells it uses under their own names, byte for byte
the PDK's, which macro/v1's pdk.cell_name accepts from a check that has the
PDK's libraries (the chip's has them). Its SPICE netlist instantiates those
cells and defines only the block, since LVS takes the cells from the PDK's CDL.

    python3 harden_counter.py OUT_DIR [--librelane librelane]

writes OUT_DIR/tiny_counter.macro/. Needs the `librelane` command (here a
wrapper around the LibreLane 3.0.0.dev44 container). Standard library only.
"""

import argparse
import hashlib
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

NAME = "tiny_counter"

RTL = """// tiny_counter: two-bit counter with enable and active-low reset.
module tiny_counter (
`ifdef USE_POWER_PINS
    inout VDD,
    inout VSS,
`endif
    input clk,
    input rst_n,
    input en,
    output reg [1:0] q
);
    always @(posedge clk or negedge rst_n) begin
        if (!rst_n) q <= 2'b00;
        else if (en) q <= q + 2'b01;
    end
endmodule
"""

PINS = """#S
clk
rst_n
en
#N
q\\[0\\]
q\\[1\\]
"""

CONFIG = {
    "DESIGN_NAME": NAME,
    "VERILOG_FILES": ["dir::tiny_counter.v"],
    "VERILOG_DEFINES": [],
    "CLOCK_PORT": "clk",
    "CLOCK_PERIOD": 20,
    "FP_SIZING": "absolute",
    "DIE_AREA": [0, 0, 60, 60],
    "PL_TARGET_DENSITY_PCT": 45,
    "VDD_NETS": ["VDD"],
    "GND_NETS": ["VSS"],
    # One layer of Metal4 straps out to the block's edges: the power pins
    # are then on the edge, where a chip's router reaches them.
    "PDN_MULTILAYER": False,
    "PDN_VERTICAL_LAYER": "Metal4",
    "PDN_HORIZONTAL_LAYER": "Metal5",
    "PDN_EXTEND_TO": "boundary",
    "PDN_VPITCH": 25,
    "PDN_VOFFSET": 10,
    "PDN_VWIDTH": 1.2,
    "PDN_VSPACING": 4,
    "IO_PIN_ORDER_CFG": "dir::pins.cfg",
    "IO_PIN_MIN_DISTANCE": 4,
    "RT_MAX_LAYER": "Metal4",
}


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def lef_pins(text):
    """Pin name to (direction, layers) from a LEF macro."""
    pins = {}
    for m in re.finditer(r"\n\s*PIN (\S+)(.*?)\n\s*END \1", text, re.S):
        body = m.group(2)
        d = re.search(r"DIRECTION (\w+)", body)
        layers = re.findall(r"LAYER (\w+)", body)
        pins[m.group(1)] = (d.group(1).lower() if d else "inout", layers)
    size = re.search(r"SIZE ([\d.]+) BY ([\d.]+)", text)
    return pins, (float(size.group(1)), float(size.group(2)))


def cdl_ports(cdl_text):
    """Port order of every standard cell in the PDK's CDL."""
    ports = {}
    lines = cdl_text.replace("\n+", " ").splitlines()
    for line in lines:
        w = line.split()
        if len(w) >= 2 and w[0].upper() == ".SUBCKT":
            ports[w[1]] = w[2:]
    return ports


def verilog_instances(text):
    """(cell, instance, {pin: net}) of a flat gate-level netlist."""
    text = re.sub(r"//.*", "", text)
    out = []
    for m in re.finditer(r"\b(sg13g2_\w+)\s+(\\?\S+)\s*\((.*?)\)\s*;", text, re.S):
        cell, inst, body = m.group(1), m.group(2), m.group(3)
        conns = {}
        for c in re.finditer(r"\.(\w+)\s*\(\s*([^()]*?)\s*\)", body):
            conns[c.group(1)] = c.group(2).strip()
        out.append((cell, inst.lstrip("\\").strip(), conns))
    return out


def net_name(net, k):
    net = net.lstrip("\\").strip()
    return net if net else "_nc%d" % k


def spice(pnl_text, cdl_text):
    ports = cdl_ports(cdl_text)
    lines = [
        "* %s: cell-level netlist for LVS, from LibreLane's powered netlist, with the" % NAME,
        "* PDK's standard-cell port order. The cells are the PDK's (sg13g2_stdcell.cdl).",
        ".subckt %s clk rst_n en q[0] q[1] VDD VSS" % NAME,
        "*.PININFO clk:I rst_n:I en:I q[0]:O q[1]:O VDD:B VSS:B",
    ]
    k = 0
    for cell, inst, conns in verilog_instances(pnl_text):
        if cell not in ports:
            raise SystemExit("the PDK's CDL has no cell %s" % cell)
        nets = []
        for pin in ports[cell]:
            k += 1
            nets.append(net_name(conns.get(pin, ""), k))
        lines.append("X%s %s %s" % (re.sub(r"[^\w\[\]]", "_", inst), " ".join(nets), cell))
    lines.append(".ends %s" % NAME)
    return "\n".join(lines) + "\n"


def fix_liberty(text):
    """LibreLane's block liberty, with the supplies as pg_pins (macro/v1
    needs them) and a missing semicolon after `area` added."""
    text = re.sub(r"(\barea\s*:\s*[\d.]+)\s*\n", r"\1 ;\n", text)
    pg = (
        '    pg_pin (VDD) {\n      pg_type : primary_power ;\n      voltage_name : VDD ;\n    }\n'
        '    pg_pin (VSS) {\n      pg_type : primary_ground ;\n      voltage_name : VSS ;\n    }\n'
    )
    text = re.sub(r'(cell\s*\(\s*"?%s"?\s*\)\s*\{\n)' % NAME, lambda m: m.group(1) + pg, text, count=1)
    return text


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("out", type=Path)
    ap.add_argument("--librelane", default="librelane")
    ap.add_argument(
        "--package-only",
        action="store_true",
        help="package an earlier run in OUT_DIR/build without running LibreLane",
    )
    args = ap.parse_args()
    out = args.out.resolve()
    build = out / "build"
    build.mkdir(parents=True, exist_ok=True)
    (build / "tiny_counter.v").write_text(RTL)
    (build / "pins.cfg").write_text(PINS)
    (build / "config.json").write_text(json.dumps(CONFIG, indent=2))
    if not args.package_only:
        subprocess.run(
            [args.librelane, "--run-tag", "aispice", "--overwrite", "config.json"],
            cwd=build,
            check=True,
        )
    run = build / "runs" / "aispice"
    final = run / "final"
    resolved = json.loads((run / "resolved.json").read_text())
    metrics = json.loads((final / "metrics.json").read_text())
    drc = int(metrics.get("klayout__drc_error__count", -1))
    lvs = int(metrics.get("design__lvs_error__count", -1))
    if drc != 0 or lvs != 0:
        raise SystemExit("LibreLane's DRC (%d) or LVS (%d) is not clean" % (drc, lvs))
    pdk_root = Path(resolved["PDK_ROOT"]) / resolved["PDK"]
    commit = re.search(r"versions/([0-9a-f]{7,40})", str(pdk_root))
    commit = commit.group(1) if commit else "0" * 40
    cdl = (pdk_root / "libs.ref/sg13g2_stdcell/cdl/sg13g2_stdcell.cdl").read_text()

    pkg = out / ("%s.macro" % NAME)
    if pkg.exists():
        shutil.rmtree(pkg)
    (pkg / "evidence").mkdir(parents=True)
    shutil.copy(final / "gds" / ("%s.gds" % NAME), pkg / ("%s.gds" % NAME))
    lef = (final / "lef" / ("%s.lef" % NAME)).read_text()
    # LibreLane writes every signal pin as USE SIGNAL; the clock is a clock.
    lef = re.sub(r"(PIN clk\b.*?USE )SIGNAL", r"\1CLOCK", lef, count=1, flags=re.S)
    (pkg / ("%s.lef" % NAME)).write_text(lef)
    libs = {
        "": "nom_typ_1p20V_25C",
        "__ss": "nom_slow_1p08V_125C",
        "__ff": "nom_fast_1p32V_m40C",
    }
    for suffix, corner in libs.items():
        src = next((final / "lib" / corner).glob("*.lib"))
        (pkg / ("%s%s.lib" % (NAME, suffix))).write_text(fix_liberty(src.read_text()))
    (pkg / ("%s.v" % NAME)).write_text(RTL)
    pnl = (final / "pnl" / ("%s.pnl.v" % NAME)).read_text()
    (pkg / ("%s.spice" % NAME)).write_text(spice(pnl, cdl))

    pins, (w, h) = lef_pins(lef)
    toml = [
        'schema = "macro/v1"',
        'name = "%s"' % NAME,
        'kind = "digital"',
        'description = "Two-bit counter with enable and active-low reset, hardened by LibreLane for aispice\'s tests."',
        "",
        "[pdk]",
        'name = "ihp-sg13g2"',
        'commit = "%s"' % commit,
        "",
        "[producer]",
        'tool = "librelane"',
        'version = "3.0.0.dev44"',
        'source = "crates/aispice-tools/tests/support/harden_counter.py"',
        "",
        "[size]",
        "width_um = %s" % w,
        "height_um = %s" % h,
        "",
        "[[domains]]",
        'name = "core"',
        "voltage = 1.2",
        'power = ["VDD"]',
        'ground = ["VSS"]',
    ]
    for name, direction, kind, width in [
        ("clk", "input", "clock", None),
        ("rst_n", "input", "signal", None),
        ("en", "input", "signal", None),
        ("q", "output", "signal", 2),
    ]:
        toml += ["", "[[pins]]", 'name = "%s"' % name, 'direction = "%s"' % direction,
                 'kind = "%s"' % kind, 'domain = "core"']
        if width:
            toml.append("width = %d" % width)
    toml += ["", "[[pins]]", 'name = "VDD"', 'direction = "inout"', 'kind = "power"',
             "", "[[pins]]", 'name = "VSS"', 'direction = "inout"', 'kind = "ground"']
    toml += [
        "", "[[corners]]", 'name = "tt"', 'process = "tt"', "temperature = 25",
        "", "[[corners]]", 'name = "ss"', 'process = "ss"', "temperature = 125",
        "voltages = { core = 1.08 }", 'liberty = "%s__ss.lib"' % NAME,
        "", "[[corners]]", 'name = "ff"', 'process = "ff"', "temperature = -40",
        "voltages = { core = 1.32 }", 'liberty = "%s__ff.lib"' % NAME,
    ]
    (pkg / "macro.toml").write_text("\n".join(toml) + "\n")
    for p in ("clk", "rst_n", "en", "q[0]", "q[1]", "VDD", "VSS"):
        if p not in pins:
            raise SystemExit("LibreLane's LEF has no pin %s" % p)

    def evidence(op, summary, inputs):
        doc = {
            "schema": "macro.evidence/v1",
            "operation": op,
            "tool": {"name": "librelane", "version": "3.0.0.dev44"},
            "pdk": {"name": "ihp-sg13g2", "commit": commit},
            "engineering": {"status": "pass", "summary": summary},
            "inputs": [{"path": f, "sha256": sha256(pkg / f)} for f in inputs],
        }
        (pkg / "evidence" / ("%s.json" % op)).write_text(json.dumps(doc, indent=2) + "\n")

    evidence("drc", "LibreLane's KLayout DRC with IHP's deck: 0 violations", ["%s.gds" % NAME])
    evidence(
        "lvs",
        "LibreLane's Netgen LVS: 0 differences (against its own netlist; tiny_counter.spice is that netlist with the PDK's port order)",
        ["%s.gds" % NAME],
    )
    print("wrote %s" % pkg)


if __name__ == "__main__":
    sys.exit(main())
