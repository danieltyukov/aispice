"""Write the shared macro package fixtures (macro/v1).

Run from this folder's parent repository with any Python 3.10 or newer:

    python tests/fixtures/macro/generate.py

It needs only the standard library, writes every fixture next to itself and then
SHA256SUMS. The output is byte for byte reproducible. chipleap and aispice carry
identical copies of this folder; expected.json is written by hand and pins what
both validators report for each case.

Everything here is synthetic: rectangles on documented IHP SG13G2 layer numbers
(from the PDK's layer table), hand-written LEF, liberty, Verilog and SPICE, invented
spec values and evidence that says no check was run. No PDK file is copied.
"""

from __future__ import annotations

import hashlib
import json
import shutil
import struct
from collections.abc import Callable
from pathlib import Path

HERE = Path(__file__).resolve().parent
PDK_COMMIT = "22f2a25f1734796de3debbbf29cf697cbbc54081"

# IHP SG13G2 layer numbers (layer, datatype), from the PDK's sg13g2.lyp.
ACTIV = (1, 0)
GATPOLY = (5, 0)
METAL1 = (8, 0)
METAL1_PIN = (8, 2)
METAL1_TEXT = (8, 25)
METAL2 = (10, 0)
METAL2_PIN = (10, 2)
METAL2_TEXT = (10, 25)
PR_BOUNDARY = (189, 4)

Files = dict[str, bytes]


# GDSII writer: just the records the fixtures use.


def _real8(x: float) -> bytes:
    if x == 0:
        return bytes(8)
    sign = 0x80 if x < 0 else 0
    x = abs(x)
    exp = 64
    while x >= 1:
        x /= 16
        exp += 1
    while x < 1 / 16:
        x *= 16
        exp -= 1
    mant = round(x * 2**56)
    if mant >= 2**56:
        mant //= 16
        exp += 1
    return bytes([sign | exp]) + mant.to_bytes(7, "big")


def _rec(rtype: int, dtype: int, payload: bytes = b"") -> bytes:
    return struct.pack(">HBB", 4 + len(payload), rtype, dtype) + payload


def _i16(*v: int) -> bytes:
    return struct.pack(f">{len(v)}h", *v)


def _i32(*v: int) -> bytes:
    return struct.pack(f">{len(v)}i", *v)


def _str(s: str) -> bytes:
    b = s.encode("ascii")
    return b + b"\0" if len(b) % 2 else b


DATE = _i16(2026, 10, 11, 0, 0, 0) * 2


class Cell:
    def __init__(self, name: str) -> None:
        self.name = name
        self.body = b""

    def rect(self, layer: tuple[int, int], x0: int, y0: int, x1: int, y1: int) -> Cell:
        pts = _i32(x0, y0, x1, y0, x1, y1, x0, y1, x0, y0)
        self.body += (
            _rec(0x08, 0x00)
            + _rec(0x0D, 0x02, _i16(layer[0]))
            + _rec(0x0E, 0x02, _i16(layer[1]))
            + _rec(0x10, 0x03, pts)
            + _rec(0x11, 0x00)
        )
        return self

    def path(self, layer: tuple[int, int], width: int, pathtype: int, *xy: int) -> Cell:
        self.body += (
            _rec(0x09, 0x00)
            + _rec(0x0D, 0x02, _i16(layer[0]))
            + _rec(0x0E, 0x02, _i16(layer[1]))
            + _rec(0x21, 0x02, _i16(pathtype))
            + _rec(0x0F, 0x03, _i32(width))
            + _rec(0x10, 0x03, _i32(*xy))
            + _rec(0x11, 0x00)
        )
        return self

    def text(self, layer: tuple[int, int], x: int, y: int, s: str) -> Cell:
        self.body += (
            _rec(0x0C, 0x00)
            + _rec(0x0D, 0x02, _i16(layer[0]))
            + _rec(0x16, 0x02, _i16(layer[1]))
            + _rec(0x10, 0x03, _i32(x, y))
            + _rec(0x19, 0x06, _str(s))
            + _rec(0x11, 0x00)
        )
        return self

    def sref(self, child: str, x: int, y: int, angle: float = 0, mirror: bool = False) -> Cell:
        body = _rec(0x0A, 0x00) + _rec(0x12, 0x06, _str(child))
        if angle or mirror:
            body += _rec(0x1A, 0x01, struct.pack(">H", 0x8000 if mirror else 0))
            if angle:
                body += _rec(0x1C, 0x05, _real8(angle))
        self.body += body + _rec(0x10, 0x03, _i32(x, y)) + _rec(0x11, 0x00)
        return self

    def aref(self, child: str, cols: int, rows: int, x: int, y: int, dx: int, dy: int) -> Cell:
        xy = _i32(x, y, x + cols * dx, y, x, y + rows * dy)
        self.body += (
            _rec(0x0B, 0x00)
            + _rec(0x12, 0x06, _str(child))
            + _rec(0x13, 0x02, _i16(cols, rows))
            + _rec(0x10, 0x03, xy)
            + _rec(0x11, 0x00)
        )
        return self

    def encode(self) -> bytes:
        return (
            _rec(0x05, 0x02, DATE)
            + _rec(0x06, 0x06, _str(self.name))
            + self.body
            + _rec(0x07, 0x00)
        )


def gds(lib: str, cells: list[Cell]) -> bytes:
    out = (
        _rec(0x00, 0x02, _i16(600))
        + _rec(0x01, 0x02, DATE)
        + _rec(0x02, 0x06, _str(lib))
        + _rec(0x03, 0x05, _real8(1e-3) + _real8(1e-9))
    )
    for c in cells:
        out += c.encode()
    return out + _rec(0x04, 0x00)


# The analog macro: a five-transistor OTA, 12 x 8 um.


def ota_gds(top: str = "tiny_ota", extra_rect: bool = False) -> bytes:
    pair = Cell("tiny_ota_pair")
    pair.rect(ACTIV, 0, 0, 2000, 1000).rect(GATPOLY, 400, -200, 600, 1200)
    pair.rect(GATPOLY, 1400, -200, 1600, 1200)
    cell = Cell(top)
    cell.rect(PR_BOUNDARY, 0, 0, 12000, 8000)
    cell.rect(METAL1, 0, 7400, 12000, 8000).rect(METAL1_PIN, 0, 7400, 12000, 8000)
    cell.rect(METAL1, 0, 0, 12000, 600).rect(METAL1_PIN, 0, 0, 12000, 600)
    cell.text(METAL1_TEXT, 6000, 7700, "vdd").text(METAL1_TEXT, 6000, 300, "vss")
    # The input pair, rotated a quarter turn, and the mirror load, mirrored.
    cell.sref("tiny_ota_pair", 5000, 2000, angle=90)
    cell.sref("tiny_ota_pair", 7000, 6000, mirror=True)
    for name, x0, y0 in (
        ("inp", 1000, 3000),
        ("inn", 1000, 4500),
        ("ibias", 1000, 1500),
        ("out", 10600, 3500),
    ):
        cell.rect(METAL2, x0, y0, x0 + 400, y0 + 1000)
        cell.rect(METAL2_PIN, x0, y0, x0 + 400, y0 + 1000)
        cell.text(METAL2_TEXT, x0 + 200, y0 + 500, name)
    if extra_rect:
        cell.rect(METAL1, 3000, 1000, 3400, 1400)
    return gds("tiny_ota", [pair, cell])


OTA_TOML = f"""\
schema = "macro/v1"
name = "tiny_ota"
kind = "analog"
description = "Five-transistor OTA. Synthetic test fixture, not a working design."

[pdk]
name = "ihp-sg13g2"
commit = "{PDK_COMMIT}"

[producer]
tool = "aispice"
version = "0.2.0"
source = "tests/fixtures/macro/generate.py"

[size]
width_um = 12.0
height_um = 8.0

[[domains]]
name = "core"
voltage = 1.2
power = ["vdd"]
ground = ["vss"]

[[pins]]
name = "inp"
direction = "input"
kind = "analog"
domain = "core"
layer = "Metal2"
description = "Non-inverting input"

[[pins]]
name = "inn"
direction = "input"
kind = "analog"
domain = "core"
layer = "Metal2"
description = "Inverting input"

[[pins]]
name = "out"
direction = "output"
kind = "analog"
domain = "core"
layer = "Metal2"

[[pins]]
name = "ibias"
direction = "input"
kind = "analog"
domain = "core"
layer = "Metal2"
description = "Bias current in, 10 uA"

[[pins]]
name = "vdd"
direction = "inout"
kind = "power"
layer = "Metal1"

[[pins]]
name = "vss"
direction = "inout"
kind = "ground"
layer = "Metal1"

[[corners]]
name = "tt"
process = "tt"
temperature = 27

[[corners]]
name = "ss"
process = "ss"
temperature = 125
voltages = {{ core = 1.08 }}

[[corners]]
name = "ff"
process = "ff"
temperature = -40
voltages = {{ core = 1.32 }}
"""


def _lef_pin(name: str, direction: str, use: str, layer: str, rect: str) -> str:
    return (
        f"  PIN {name}\n"
        f"    DIRECTION {direction} ;\n"
        f"    USE {use} ;\n"
        f"    PORT\n"
        f"      LAYER {layer} ;\n"
        f"        RECT {rect} ;\n"
        f"    END\n"
        f"  END {name}\n"
    )


def ota_lef(drop: str = "") -> str:
    pins = [
        ("inp", "INPUT", "ANALOG", "Metal2", "1 3 1.4 4"),
        ("inn", "INPUT", "ANALOG", "Metal2", "1 4.5 1.4 5.5"),
        ("out", "OUTPUT", "ANALOG", "Metal2", "10.6 3.5 11 4.5"),
        ("ibias", "INPUT", "ANALOG", "Metal2", "1 1.5 1.4 2.5"),
        ("vdd", "INOUT", "POWER", "Metal1", "0 7.4 12 8"),
        ("vss", "INOUT", "GROUND", "Metal1", "0 0 12 0.6"),
    ]
    body = "".join(_lef_pin(*p) for p in pins if p[0] != drop)
    return (
        "# tiny_ota abstract. Synthetic test fixture.\n"
        "VERSION 5.8 ;\n"
        'BUSBITCHARS "[]" ;\n'
        'DIVIDERCHAR "/" ;\n'
        "MACRO tiny_ota\n"
        "  CLASS BLOCK ;\n"
        "  ORIGIN 0 0 ;\n"
        "  FOREIGN tiny_ota 0 0 ;\n"
        "  SIZE 12 BY 8 ;\n"
        "  SYMMETRY X Y ;\n"
        f"{body}"
        "  OBS\n"
        "    LAYER Metal1 ;\n"
        "      RECT 0.5 1 11.5 7 ;\n"
        "  END\n"
        "END tiny_ota\n"
        "END LIBRARY\n"
    )


OTA_LIB = """\
/* tiny_ota liberty. Synthetic test fixture: the numbers are invented. */
library (tiny_ota) {
  delay_model : table_lookup ;
  time_unit : "1ns" ;
  voltage_unit : "1V" ;
  current_unit : "1uA" ;
  capacitive_load_unit (1, pf) ;
  leakage_power_unit : "1nW" ;
  pulling_resistance_unit : "1kohm" ;
  nom_process : 1 ;
  nom_temperature : 27 ;
  nom_voltage : 1.2 ;
  voltage_map (vdd, 1.2) ;
  voltage_map (vss, 0) ;
  operating_conditions (tt) {
    process : 1 ;
    temperature : 27 ;
    voltage : 1.2 ;
  }
  default_operating_conditions : tt ;
  cell (tiny_ota) {
    area : 96 ;
    dont_touch : true ;
    dont_use : true ;
    cell_leakage_power : 12 ;
    pg_pin (vdd) {
      pg_type : primary_power ;
      voltage_name : vdd ;
    }
    pg_pin (vss) {
      pg_type : primary_ground ;
      voltage_name : vss ;
    }
    pin (inp) {
      direction : input ;
      capacitance : 0.012 ;
      related_power_pin : vdd ;
      related_ground_pin : vss ;
    }
    pin (inn) {
      direction : input ;
      capacitance : 0.012 ;
      related_power_pin : vdd ;
      related_ground_pin : vss ;
    }
    pin (out) {
      direction : output ;
      max_capacitance : 0.5 ;
      related_power_pin : vdd ;
      related_ground_pin : vss ;
    }
    pin (ibias) {
      direction : input ;
      capacitance : 0.004 ;
      related_power_pin : vdd ;
      related_ground_pin : vss ;
    }
  }
}
"""

OTA_V = """\
// Blackbox model of tiny_ota for digital-on-top integration. Synthetic test fixture.
(* blackbox *)
module tiny_ota (
`ifdef USE_POWER_PINS
    inout  vdd,
    inout  vss,
`endif
    input  inp,
    input  inn,
    input  ibias,
    output out
);
endmodule
"""


def ota_spice(ports: str = "inp inn out ibias vdd vss") -> str:
    info = " ".join(
        f"{p}:{d}"
        for p, d in (
            ("inp", "I"),
            ("inn", "I"),
            ("out", "O"),
            ("ibias", "I"),
            ("vdd", "B"),
            ("vss", "B"),
        )
        if p in ports.split()
    )
    return (
        "* tiny_ota: five-transistor OTA. Synthetic test fixture, not a working design.\n"
        f".subckt tiny_ota {ports}\n"
        f"*.PININFO {info}\n"
        "XM1 n1 inp tail vss sg13_lv_nmos w=1u l=0.5u\n"
        "XM2 out inn tail vss sg13_lv_nmos w=1u l=0.5u\n"
        "XM3 n1 n1 vdd vdd sg13_lv_pmos w=2u l=0.5u\n"
        "XM4 out n1 vdd vdd sg13_lv_pmos w=2u l=0.5u\n"
        "XM5 tail ibias vss vss sg13_lv_nmos w=2u l=1u\n"
        "+ m=1\n"
        "XM6 ibias ibias vss vss sg13_lv_nmos w=2u l=1u\n"
        ".ends tiny_ota\n"
    )


def ota_specs(ss_gain: float = 33.8) -> str:
    return f"""\
schema = "macro.specs/v1"

# Synthetic test fixture: the values are invented, not simulated.

[[specs]]
name = "dc_gain"
description = "Open-loop gain at 1 kHz into 100 fF"
unit = "dB"
min = 30
typ = 36
measured = {{ tt = 36.2, ss = {ss_gain}, ff = 37.1 }}
definition = "dc_gain = gain_db_at(V(out)/V(inp), 1k) >= 30"

[[specs]]
name = "ugbw"
description = "Unity-gain bandwidth into 100 fF"
unit = "Hz"
min = 10e6
measured = {{ tt = 42.0e6, ss = 28.5e6, ff = 55.3e6 }}

[[specs]]
name = "supply_current"
unit = "A"
max = 25e-6
measured = {{ tt = 20.1e-6, ss = 18.7e-6, ff = 22.4e-6 }}
"""


# The digital macro: a 2-bit counter with enable and active-low reset, 20 x 10 um.


def counter_gds(
    top: str = "tiny_counter", hierarchy: str = "", tap_name: str = "tiny_counter_tap"
) -> bytes:
    """The counter's layout; ``hierarchy`` adds references the validators must refuse
    or measure without visiting every instance."""
    tap = Cell(tap_name)
    tap.rect(ACTIV, 0, 0, 300, 300)
    cell = Cell(top)
    cell.rect(PR_BOUNDARY, 0, 0, 20000, 10000)
    cell.rect(METAL1, 0, 9400, 20000, 10000).rect(METAL1_PIN, 0, 9400, 20000, 10000)
    cell.rect(METAL1, 0, 0, 20000, 600).rect(METAL1_PIN, 0, 0, 20000, 600)
    cell.text(METAL1_TEXT, 10000, 9700, "VDD").text(METAL1_TEXT, 10000, 300, "VSS")
    # A routed net with flush ends that stop on the boundary.
    cell.path(METAL1, 200, 0, 0, 5000, 4000, 5000, 4000, 8000)
    cell.aref(tap_name, 4, 2, 6000, 1000, 2000, 1000)
    for name, x0, y0 in (
        ("clk", 0, 2000),
        ("rst_n", 0, 3500),
        ("en", 0, 6500),
        ("q[0]", 19600, 3000),
        ("q[1]", 19600, 6000),
    ):
        cell.rect(METAL2, x0, y0, x0 + 400, y0 + 800)
        cell.rect(METAL2_PIN, x0, y0, x0 + 400, y0 + 800)
        cell.text(METAL2_TEXT, x0 + 200, y0 + 400, name)
    cells = [tap, cell]
    if hierarchy == "self-reference":
        cell.sref("tiny_counter_loop", 1000, 1000)
        loop = Cell("tiny_counter_loop").rect(METAL1, 0, 0, 100, 100)
        cells.append(loop.sref("tiny_counter_loop", 0, 0))
    elif hierarchy == "cycle":
        cell.sref("tiny_counter_a", 1000, 1000)
        cells.append(Cell("tiny_counter_a").sref("tiny_counter_b", 0, 0))
        cells.append(
            Cell("tiny_counter_b").rect(METAL1, 0, 0, 100, 100).sref("tiny_counter_a", 0, 0)
        )
    elif hierarchy == "huge-array":
        # 32767 x 32767 instances, 1 nm apart: the box comes from the corner instances.
        cell.aref("tiny_counter_dot", 32767, 32767, 0, 0, 1, 1)
        cells.append(Cell("tiny_counter_dot").rect(METAL1, 0, 0, 10, 10))
    elif hierarchy == "deep":
        # 65 levels of references below the top cell, one more than the limit.
        cell.sref("tiny_counter_d0", 1000, 1000)
        cells += [
            Cell(f"tiny_counter_d{i}").sref(f"tiny_counter_d{i + 1}", 0, 0) for i in range(64)
        ]
        cells.append(Cell("tiny_counter_d64").rect(METAL1, 0, 0, 100, 100))
    return gds("tiny_counter", cells)


def counter_toml(width_um: str = "20.0") -> str:
    return f"""\
schema = "macro/v1"
name = "tiny_counter"
kind = "digital"
description = "2-bit counter with enable and active-low reset. Synthetic test fixture."

[pdk]
name = "ihp-sg13g2"
commit = "{PDK_COMMIT}"

[producer]
tool = "chipleap"
version = "0.3.0"

[size]
width_um = {width_um}
height_um = 10.0

[[domains]]
name = "core"
voltage = 1.2
power = ["VDD"]
ground = ["VSS"]

[[pins]]
name = "clk"
direction = "input"
kind = "clock"
domain = "core"
layer = "Metal2"

[[pins]]
name = "rst_n"
direction = "input"
kind = "signal"
domain = "core"
layer = "Metal2"

[[pins]]
name = "en"
direction = "input"
kind = "signal"
domain = "core"

[[pins]]
name = "q"
direction = "output"
kind = "signal"
domain = "core"
width = 2
layer = "Metal2"

[[pins]]
name = "VDD"
direction = "inout"
kind = "power"

[[pins]]
name = "VSS"
direction = "inout"
kind = "ground"

[[corners]]
name = "tt"
process = "tt"
temperature = 27

[[corners]]
name = "ss"
process = "ss"
temperature = 125
voltages = {{ core = 1.08 }}
liberty = "tiny_counter__ss.lib"

[extra.chipleap]
clock_period_ns = 10.0
"""


def counter_lef(width: str = "20", vss_use: str = "GROUND") -> str:
    pins = [
        ("clk", "INPUT", "CLOCK", "Metal2", "0 2 0.4 2.8"),
        ("rst_n", "INPUT", "SIGNAL", "Metal2", "0 3.5 0.4 4.3"),
        ("en", "INPUT", "SIGNAL", "Metal2", "0 6.5 0.4 7.3"),
        ("q[0]", "OUTPUT", "SIGNAL", "Metal2", "19.6 3 20 3.8"),
        ("q[1]", "OUTPUT", "SIGNAL", "Metal2", "19.6 6 20 6.8"),
        ("VDD", "INOUT", "POWER", "Metal1", "0 9.4 20 10"),
        ("VSS", "INOUT", vss_use, "Metal1", "0 0 20 0.6"),
    ]
    return (
        "# tiny_counter abstract. Synthetic test fixture.\n"
        "VERSION 5.8 ;\n"
        'BUSBITCHARS "[]" ;\n'
        'DIVIDERCHAR "/" ;\n'
        "UNITS\n"
        "  DATABASE MICRONS 1000 ;\n"
        "END UNITS\n"
        "MACRO tiny_counter\n"
        "  CLASS BLOCK ;\n"
        "  FOREIGN tiny_counter ;\n"
        "  ORIGIN 0 0 ;\n"
        f"  SIZE {width} BY 10 ;\n" + "".join(_lef_pin(*p) for p in pins) + "  OBS\n"
        "    LAYER Metal1 ;\n"
        "      RECT 1 1 19 9 ;\n"
        "    LAYER Metal2 ;\n"
        "      RECT 1 1 19 9 ;\n"
        "  END\n"
        "END tiny_counter\n"
        "END LIBRARY\n"
    )


def counter_lib(corner: str, voltage: str, temp: str, q_timing: bool = True) -> str:
    q_arcs = (
        """      timing () {
        related_pin : "clk" ;
        timing_type : rising_edge ;
        timing_sense : non_unate ;
        cell_rise (scalar) { values ("0.35") ; }
        cell_fall (scalar) { values ("0.32") ; }
        rise_transition (scalar) { values ("0.08") ; }
        fall_transition (scalar) { values ("0.07") ; }
      }
"""
        if q_timing
        else ""
    )
    setup_hold = """      timing () {
        related_pin : "clk" ;
        timing_type : setup_rising ;
        rise_constraint (scalar) { values ("0.12") ; }
        fall_constraint (scalar) { values ("0.12") ; }
      }
      timing () {
        related_pin : "clk" ;
        timing_type : hold_rising ;
        rise_constraint (scalar) { values ("0.03") ; }
        fall_constraint (scalar) { values ("0.03") ; }
      }
"""
    return f"""\
/* tiny_counter liberty, corner {corner}. Synthetic test fixture: the numbers are invented. */
library (tiny_counter_{corner}) {{
  delay_model : table_lookup ;
  time_unit : "1ns" ;
  voltage_unit : "1V" ;
  current_unit : "1mA" ;
  capacitive_load_unit (1, pf) ;
  leakage_power_unit : "1nW" ;
  pulling_resistance_unit : "1kohm" ;
  nom_process : 1 ;
  nom_temperature : {temp} ;
  nom_voltage : {voltage} ;
  voltage_map (VDD, {voltage}) ;
  voltage_map (VSS, 0) ;
  type (bus2) {{
    base_type : array ;
    data_type : bit ;
    bit_width : 2 ;
    bit_from : 1 ;
    bit_to : 0 ;
    downto : true ;
  }}
  cell (tiny_counter) {{
    area : 200 ;
    cell_leakage_power : 3.5 ;
    pg_pin (VDD) {{
      pg_type : primary_power ;
      voltage_name : VDD ;
    }}
    pg_pin (VSS) {{
      pg_type : primary_ground ;
      voltage_name : VSS ;
    }}
    pin (clk) {{
      direction : input ;
      clock : true ;
      capacitance : 0.003 ;
    }}
    pin (rst_n) {{
      direction : input ;
      capacitance : 0.002 ;
{setup_hold}    }}
    pin (en) {{
      direction : input ;
      capacitance : 0.002 ;
{setup_hold}    }}
    bus (q) {{
      bus_type : bus2 ;
      direction : output ;
      max_capacitance : 0.1 ;
{q_arcs}    }}
  }}
}}
"""


def counter_v(en_dir: str = "input", q_range: str = "[1:0]") -> str:
    return f"""\
// Behavioral model of tiny_counter. Synthetic test fixture.
module tiny_counter (
`ifdef USE_POWER_PINS
    inout VDD,
    inout VSS,
`endif
    input clk,
    input rst_n,
    {en_dir} en,
    output reg {q_range} q
);
    // A comment with input and output words in it does not declare anything.
    always @(posedge clk or negedge rst_n) begin
        if (!rst_n) q <= 2'b00;
        else if (en) q <= q + 2'b01;
    end
endmodule
"""


COUNTER_SPICE = """\
* tiny_counter: 2-bit counter with enable and active-low reset. Synthetic test fixture.
* Cell-level netlist; the cells are the IHP SG13G2 standard cells, defined in the PDK.
.SUBCKT tiny_counter clk rst_n en q[0] q[1]
+ VDD VSS
*.PININFO clk:I rst_n:I en:I q[0]:O q[1]:O VDD:B VSS:B
Xff0 q[0] qn0 clk d0 rst_n VDD VSS sg13g2_dfrbp_1
Xff1 q[1] qn1 clk d1 rst_n VDD VSS sg13g2_dfrbp_1
Xx0 d0 q[0] en VDD VSS sg13g2_xor2_1
Xa0 c0 q[0] en VDD VSS sg13g2_and2_1
Xx1 d1 q[1] c0 VDD VSS sg13g2_xor2_1
.ENDS tiny_counter
"""


# Evidence


def _sha(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()


def evidence(
    files: Files,
    operation: str,
    inputs: list[str],
    status: str = "pass",
    pdk_name: str = "ihp-sg13g2",
) -> bytes:
    doc = {
        "schema": "macro.evidence/v1",
        "operation": operation,
        "tool": {"name": "synthetic", "version": "1"},
        "pdk": {"name": pdk_name, "commit": PDK_COMMIT},
        "engineering": {
            "status": status,
            "summary": f"Synthetic test fixture: no {operation.upper()} was run.",
        },
        "inputs": [{"path": p, "sha256": _sha(files[p])} for p in inputs],
    }
    return (json.dumps(doc, indent=2) + "\n").encode()


def openada_drc(files: Files, gds_name: str) -> bytes:
    doc = {
        "schema": "openada.result/v0alpha1",
        "operation": "drc",
        "tool": {"name": "klayout", "path": None, "version": None},
        "execution": {"status": "completed", "exit_code": 0, "duration_ms": 0, "command": []},
        "engineering": {
            "status": "pass",
            "summary": "Synthetic test fixture in OpenADA's envelope: no DRC was run.",
        },
        "inputs": [
            {
                "kind": "gds",
                "role": "layout",
                "path": f"/design/{gds_name}",
                "exists": True,
                "bytes": len(files[gds_name]),
                "sha256": _sha(files[gds_name]),
            }
        ],
        "artifacts": [],
        "diagnostics": [],
        "data": {"drc_clean": True},
        "provenance": {
            "openada_version": "0.4.0",
            "created_at": "2026-10-11T00:00:00Z",
            "host": {"system": "Linux", "machine": "x86_64", "python": "3.12.3"},
        },
    }
    return (json.dumps(doc, indent=2) + "\n").encode()


def with_evidence(files: Files, name: str, **drc: str) -> Files:
    files["evidence/drc.json"] = evidence(files, "drc", [f"{name}.gds"], **drc)
    files["evidence/lvs.json"] = evidence(files, "lvs", [f"{name}.gds", f"{name}.spice"])
    return files


def ota() -> Files:
    files: Files = {
        "macro.toml": OTA_TOML.encode(),
        "tiny_ota.gds": ota_gds(),
        "tiny_ota.lef": ota_lef().encode(),
        "tiny_ota.lib": OTA_LIB.encode(),
        "tiny_ota.v": OTA_V.encode(),
        "tiny_ota.spice": ota_spice().encode(),
        "tiny_ota.specs.toml": ota_specs().encode(),
    }
    return with_evidence(files, "tiny_ota")


def counter() -> Files:
    files: Files = {
        "macro.toml": counter_toml().encode(),
        "tiny_counter.gds": counter_gds(),
        "tiny_counter.lef": counter_lef().encode(),
        "tiny_counter.lib": counter_lib("tt", "1.2", "27").encode(),
        "tiny_counter__ss.lib": counter_lib("ss", "1.08", "125").encode(),
        "tiny_counter.v": counter_v().encode(),
        "tiny_counter.spice": COUNTER_SPICE.encode(),
        "README.md": b"Synthetic test fixture for the macro/v1 contract. Not a real block.\n",
    }
    return with_evidence(files, "tiny_counter")


def _replace(files: Files, name: str, old: str, new: str) -> Files:
    text = files[name].decode()
    assert old in text, (name, old)
    files[name] = text.replace(old, new, 1).encode()
    return files


def _stale(files: Files) -> Files:
    # The layout changed after DRC and LVS ran: the summaries keep the old hashes.
    files["tiny_ota.gds"] = ota_gds(extra_rect=True)
    return files


def _openada(files: Files) -> Files:
    files["evidence/drc.json"] = openada_drc(files, "tiny_counter.gds")
    return files


def _cut(files: Files, name: str, marker: str) -> Files:
    """The file up to the end of ``marker``, as if a copy had stopped there."""
    text = files[name].decode()
    assert marker in text, (name, marker)
    files[name] = text[: text.index(marker) + len(marker)].encode()
    return files


def _counter_layout(hierarchy: str) -> Callable[[Files], Files]:
    return lambda f: with_evidence(
        {**f, "tiny_counter.gds": counter_gds(hierarchy=hierarchy)}, "tiny_counter"
    )


# Nested 70 deep, past the limit of 64 both validators check before parsing.
DEEP = 70


# Broken and edge-case variants: (case, base, directory name, change). Each keeps
# its evidence hashes fresh unless the change is about the evidence.
VARIANTS: list[tuple[str, Callable[[], Files], str, Callable[[Files], Files]]] = [
    (
        "lef-missing-pin",
        ota,
        "tiny_ota.macro",
        lambda f: with_evidence({**f, "tiny_ota.lef": ota_lef(drop="inn").encode()}, "tiny_ota"),
    ),
    (
        "pdk-mismatch",
        ota,
        "tiny_ota.macro",
        lambda f: with_evidence(f, "tiny_ota", pdk_name="sky130A"),
    ),
    (
        "specs-fail",
        ota,
        "tiny_ota.macro",
        lambda f: {**f, "tiny_ota.specs.toml": ota_specs(ss_gain=28.4).encode()},
    ),
    (
        "missing-specs",
        ota,
        "tiny_ota.macro",
        lambda f: {k: v for k, v in f.items() if k != "tiny_ota.specs.toml"},
    ),
    ("stale-evidence", ota, "tiny_ota.macro", _stale),
    (
        "spice-missing-pin",
        ota,
        "tiny_ota.macro",
        lambda f: with_evidence(
            {**f, "tiny_ota.spice": ota_spice("inp inn out vdd vss").encode()}, "tiny_ota"
        ),
    ),
    (
        "manifest-schema",
        ota,
        "tiny_ota.macro",
        lambda f: _replace(
            _replace(f, "macro.toml", 'kind = "analog"\ndomain', 'kind = "analogue"\ndomain'),
            "macro.toml",
            "width_um = 12.0",
            "width_um = 0",
        ),
    ),
    (
        "manifest-syntax",
        ota,
        "tiny_ota.macro",
        lambda f: _replace(f, "macro.toml", 'name = "tiny_ota"', 'name = "tiny_ota'),
    ),
    (
        "wrong-size",
        counter,
        "tiny_counter.macro",
        lambda f: {**f, "macro.toml": counter_toml(width_um="30.0").encode()},
    ),
    (
        "verilog-direction",
        counter,
        "tiny_counter.macro",
        lambda f: {**f, "tiny_counter.v": counter_v(en_dir="output").encode()},
    ),
    (
        # Both sides of an ifdef declare en: Yosys defines SYNTHESIS and reads an output.
        "verilog-ifdef-port",
        counter,
        "tiny_counter.macro",
        lambda f: _replace(
            f,
            "tiny_counter.v",
            "    input en,\n",
            "`ifdef SYNTHESIS\n    output en,\n`else\n    input en,\n`endif\n",
        ),
    ),
    (
        # A second cell of the macro's name: OpenSTA keeps the last one, where en is an
        # output.
        "liberty-duplicate-cell",
        counter,
        "tiny_counter.macro",
        lambda f: _replace(
            f,
            "tiny_counter.lib",
            "\n}\n",
            "\n  cell (tiny_counter) {\n    pin (en) {\n      direction : output ;\n    }\n  }\n}\n",
        ),
    ),
    (
        # A macro written in lower case before the real one: OpenROAD reads LEF keywords
        # in any case and keeps the first macro of a name.
        "lef-duplicate-macro",
        counter,
        "tiny_counter.macro",
        lambda f: _replace(
            f,
            "tiny_counter.lef",
            "MACRO tiny_counter\n",
            "macro tiny_counter\n  class BLOCK ;\n  size 30 by 10 ;\nend tiny_counter\n"
            "MACRO tiny_counter\n",
        ),
    ),
    (
        "bus-width",
        counter,
        "tiny_counter.macro",
        lambda f: {**f, "tiny_counter.v": counter_v(q_range="[2:0]").encode()},
    ),
    (
        "missing-timing",
        counter,
        "tiny_counter.macro",
        lambda f: {
            **f,
            "tiny_counter.lib": counter_lib("tt", "1.2", "27", q_timing=False).encode(),
        },
    ),
    (
        "top-cell",
        counter,
        "tiny_counter.macro",
        lambda f: with_evidence(
            {**f, "tiny_counter.gds": counter_gds(top="counter")}, "tiny_counter"
        ),
    ),
    (
        "lvs-fail",
        counter,
        "tiny_counter.macro",
        lambda f: {
            **f,
            "evidence/lvs.json": evidence(
                f, "lvs", ["tiny_counter.gds", "tiny_counter.spice"], status="fail"
            ),
        },
    ),
    (
        # Cells named in IHP SG13G2's own name spaces: a tap cell called like a standard
        # cell, and a subcircuit whose name only differs in case.
        "pdk-cell-name",
        counter,
        "tiny_counter.macro",
        lambda f: with_evidence(
            {
                **f,
                "tiny_counter.gds": counter_gds(tap_name="sg13g2_tap"),
                "tiny_counter.spice": (
                    COUNTER_SPICE + ".SUBCKT Sg13g2_Dly a y\n.ENDS Sg13g2_Dly\n"
                ).encode(),
            },
            "tiny_counter",
        ),
    ),
    (
        "power-pin-as-signal",
        counter,
        "tiny_counter.macro",
        lambda f: _replace(
            f,
            "tiny_counter.lib",
            "pg_pin (VDD) {\n      pg_type : primary_power ;",
            "pin (VDD) {\n      direction : inout ;",
        ),
    ),
    (
        "lef-use",
        counter,
        "tiny_counter.macro",
        lambda f: {**f, "tiny_counter.lef": counter_lef(vss_use="POWER").encode()},
    ),
    ("openada-evidence", counter, "tiny_counter.macro", _openada),
    ("dirname", counter, "counter_v2", lambda f: f),
    # Files built to stall or crash a validator: each must end in a finding.
    ("gds-self-reference", counter, "tiny_counter.macro", _counter_layout("self-reference")),
    ("gds-cycle", counter, "tiny_counter.macro", _counter_layout("cycle")),
    ("gds-huge-array", counter, "tiny_counter.macro", _counter_layout("huge-array")),
    ("gds-deep", counter, "tiny_counter.macro", _counter_layout("deep")),
    (
        "manifest-nesting",
        counter,
        "tiny_counter.macro",
        lambda f: {
            **f,
            "macro.toml": f["macro.toml"] + f"\n[extra]\nx = {'[' * DEEP}{']' * DEEP}\n".encode(),
        },
    ),
    (
        "manifest-pins",
        counter,
        "tiny_counter.macro",
        lambda f: _replace(f, "macro.toml", "width = 2\n", "width = 1000000000\n"),
    ),
    (
        "liberty-nesting",
        counter,
        "tiny_counter.macro",
        lambda f: _replace(
            f,
            "tiny_counter.lib",
            "  delay_model : table_lookup ;\n",
            "  delay_model : table_lookup ;\n  " + "g () { " * DEEP + "}" * DEEP + "\n",
        ),
    ),
    (
        "evidence-nesting",
        counter,
        "tiny_counter.macro",
        lambda f: _replace(
            f,
            "evidence/drc.json",
            '"inputs": [',
            f'"data": {"[" * DEEP}{"]" * DEEP},\n  "inputs": [',
        ),
    ),
    # Files that end early, one per format.
    (
        "truncated-manifest",
        counter,
        "tiny_counter.macro",
        lambda f: _cut(f, "macro.toml", 'name = "q"\ndirection = "out'),
    ),
    (
        "truncated-gds",
        counter,
        "tiny_counter.macro",
        lambda f: with_evidence(
            {**f, "tiny_counter.gds": f["tiny_counter.gds"][: len(f["tiny_counter.gds"]) // 2]},
            "tiny_counter",
        ),
    ),
    (
        "truncated-lef",
        counter,
        "tiny_counter.macro",
        lambda f: _cut(f, "tiny_counter.lef", "PIN rst_n\n    DIRECTION INPUT ;"),
    ),
    (
        "truncated-liberty",
        counter,
        "tiny_counter.macro",
        lambda f: _cut(f, "tiny_counter.lib", "pin (en) {"),
    ),
    (
        "truncated-verilog",
        counter,
        "tiny_counter.macro",
        lambda f: _cut(f, "tiny_counter.v", "input rst_n,"),
    ),
    (
        "truncated-spice",
        counter,
        "tiny_counter.macro",
        lambda f: with_evidence(
            _cut(f, "tiny_counter.spice", ".SUBCKT tiny_counter clk rst_n en q[0]"),
            "tiny_counter",
        ),
    ),
    (
        "truncated-specs",
        ota,
        "tiny_ota.macro",
        lambda f: _cut(f, "tiny_ota.specs.toml", "measured = { tt = 36.2,"),
    ),
    (
        "truncated-evidence",
        counter,
        "tiny_counter.macro",
        lambda f: _cut(f, "evidence/drc.json", '"operation": "drc",'),
    ),
]


def reader_tests() -> Files:
    """Layouts for the GDS reader's own tests: references rotated, mirrored, arrayed
    and nested, flush and extended paths, and a cell that contains itself."""
    pair = Cell("pair").rect(METAL1, 0, 0, 2000, 1000).rect(METAL1, 400, -200, 600, 1200)
    cells = [
        pair,
        Cell("rot90").sref("pair", 5000, 2000, angle=90),
        Cell("mirror").sref("pair", 7000, 6000, mirror=True),
        Cell("mirror_rot270").sref("pair", 100, 100, angle=270, mirror=True),
        Cell("rot45").sref("pair", 0, 0, angle=45),
        Cell("arr").aref("pair", 3, 2, 1000, 500, 2500, 1500),
        Cell("path0").path(METAL1, 200, 0, 0, 5000, 4000, 5000, 4000, 8000),
        Cell("path2").path(METAL1, 200, 2, 0, 5000, 4000, 5000, 4000, 8000),
        Cell("nested")
        .sref("rot90", 10, 20, angle=180)
        .sref("arr", -3000, 0, mirror=True, angle=90),
    ]
    loop = [Cell("a").sref("b", 0, 0), Cell("b").sref("a", 0, 0)]
    return {"hierarchy.gds": gds("hierarchy", cells), "loop.gds": gds("loop", loop)}


def write(dest: Path, files: Files) -> None:
    for rel, data in sorted(files.items()):
        p = dest / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)


def main() -> None:
    for d in ("tiny_ota.macro", "tiny_counter.macro", "broken", "gds"):
        shutil.rmtree(HERE / d, ignore_errors=True)
    write(HERE / "gds", reader_tests())
    write(HERE / "tiny_ota.macro", ota())
    write(HERE / "tiny_counter.macro", counter())
    for case, base, dirname, change in VARIANTS:
        write(HERE / "broken" / case / dirname, change(base()))
    sums = []
    for p in sorted(HERE.rglob("*")):
        rel = p.relative_to(HERE).as_posix()
        if p.is_file() and rel != "SHA256SUMS" and "__pycache__" not in rel:
            sums.append(f"{_sha(p.read_bytes())}  {rel}\n")
    (HERE / "SHA256SUMS").write_text("".join(sums))


if __name__ == "__main__":
    main()
