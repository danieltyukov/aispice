# Analog layout, verification and post-layout (A3): design

Date: 2026-10-11. Status: accepted for implementation. Roadmap milestone A3. Builds on the PDK foundation (#36): a project names a registered PDK in `aispice.toml`, its circuits use the `sg13g2` device symbols, and simulations load the PDK's models. Corner sets (#34) plug into post-layout checks when they land.

## Goal

The layout loop: schematic to layout, DRC, LVS, antenna, extraction, post-layout simulation, with spec values before and after layout side by side. Layouts come from deterministic generators driven by parameters; the agent changes parameters when a check fails and never edits GDS.

## Facts this design rests on

Measured on this machine on 2026-10-11 (KLayout 0.30.4, Magic 8.3.677, IHP-Open-PDK `22f2a25`, load average around 60 from other work, so times are pessimistic).

1. IHP's `run_drc.py` and `run_lvs.py` only assemble `-rd` switches and call `klayout -b -r <deck>` through a shell string (`check_call(..., shell=True)` with paths in single quotes). aispice calls KLayout itself with an argument vector and the same switches, so no path ever reaches a shell and no Python packages are needed.
2. DRC on a single transistor: the main deck (`ihp-sg13g2.drc`, tables `main`) 13 s, the extra rules (`rule_decks/sg13g2_maximal.drc`) 11 s, antenna (`rule_decks/antenna.drc`) 3.5 s; density is skipped for blocks. A lone transistor fails `LU.b` (no substrate tie within 20 um), so every generated block needs taps.
3. Results come as a KLayout report database (`.lyrdb`, XML): categories with rule names and descriptions, items with the category, cell and values such as `polygon: (x,y;...)`, `box: (...)` or `edge-pair: (...)|(...)` in microns.
4. LVS (`sg13g2.lvs`) on a single transistor takes 7 s. Its schematic reader wants CDL-style device lines with the PDK's model names (`M1 d g s b sg13_lv_nmos w=1u l=0.13u ng=1 m=1`, `R1 a b sub rppd w= l= b= m=`, `C1 a b cap_cmim w= l= m=`, `Q1 c b e s npn13G2 ...`), not `X` subcircuit calls; xschem's `lvs_format` writes exactly these. It upper-cases names. The result database (`.lvsdb`) is read back through KLayout's API (`LayoutVsSchematic.read`, `xref()`), which gives each net, device and pin pair with its status and both names.
5. IHP's KLayout pcells (`SG13_dev`: nmos, pmos, ptap1, ntap1, via_stack, cmim, rppd, ...) load in `klayout -b` once `libs.tech/klayout/python` and the `pycell4klayout-api` submodule are on `sys.path` (the PDK's `pycell4klayout-api` and `pypreprocessor` submodules must be checked out). Loading takes about 5 s. MOS pcells draw Activ, GatPoly, Cont, Metal1 source and drain stripes and (PMOS) pSD and NWell, but no gate contact. Their geometry is regular: finger pitch `l + 0.38 um`, source/drain Metal1 stripes 0.16 um wide starting at x = 0.07, gates from x = 0.34 extending 0.18 um past Activ.
6. Magic 8.3.677 loads IHP's tech file (`ihp-sg13g2.magicrc`, which needs Magic 8.3.573 or newer) and `ext2spice` writes `X0 d g s b sg13_lv_nmos ad= pd= as= ps= w= l=` plus parasitic capacitors, which the PDK's subcircuits simulate as they are. Internal node names contain `#`.
7. The design rule values are in `libs.tech/klayout/python/sg13g2_pycell_lib/sg13g2_tech_mod.json` (`M1_b` 0.18, `V1_a` 0.19, ...).

## Decisions

### Crate

A new crate `aispice-layout` (processes and files, like `aispice-sim`, which it uses for process handling): `gds` (a small reader), `klayout` (running KLayout scripts), `drc`, `lvs`, `extract` (Magic), `render`, `gen` (generators). Tools live in `aispice-tools`, commands in the CLI.

### Running external tools

- KLayout and Magic are found on `PATH`, or set with `klayout_path` and `magic_path` in `config.toml`.
- Every run is an argument vector in a private temporary folder that is also `HOME` (and `KLAYOUT_HOME`), so neither tool reads the user's or a project's start-up files. Timeouts and cancellation as for simulators.
- Scripts are aispice's own files, embedded in the binary and written into the run folder. Parameters reach them as `-rd name=value` arguments (KLayout) or environment variables (Magic) and JSON files they read; no script text is ever built from names or paths. Cell and net names are checked against `[A-Za-z0-9_$.<>-]` before they reach a tool.
- Inputs: a GDS path must resolve inside the project. The GDS reader refuses files over 256 MiB, a file that does not start with a GDS header, and structures nested deeper than 64. Report readers cap file size (DRC 64 MiB, LVS 16 MiB JSON) and keep at most 1000 markers per rule, counting the rest.
- Outputs (reports, extracted netlists, logs) go to `.aispice/layout/<run id>/` in the project. GDS that aispice generates is written into the project through the approval hook, with a `<cell>.layout.json` beside it recording the generator and its parameters.

### DRC

- `drc(gds, cell?, checks)`: checks are `main` (always), `extra` (the maximal set, default on), `antenna` (default on), `density` (default off; it is a chip-level rule). The top cell defaults to the single top cell of the file.
- Each violation: rule, description, layer (from the rule prefix: `M1.b` is Metal1, `V1.a` Via1, `Gat.a` GatPoly, ...), cell, bounding box in um, and the marker count for the rule. A clean run says which decks ran.

### LVS

- `lvs(gds, circuit, cell?)`: the schematic side is the circuit's netlist with only the PDK devices kept, written in the CDL form of fact 4 as `.subckt <cell> <ports>`. Ports are the nets that connect the devices to anything else in the schematic (sources, other parts, labels to the outside). The layout must label them (text on the metal pin layer).
- The report: matched or not, and every net, device and pin that did not match, with layout and schematic names, plus KLayout's comparison log. The layout's extracted netlist is kept in the run folder.

### Extraction and post-layout simulation

- `extract(gds, cell?, resistance?)` runs Magic with IHP's tech: `extract all`, `ext2spice` with capacitance (threshold 0.01 fF, coupling included) and, with `resistance`, `extresist`. The result is a flat `.subckt <cell> <labelled nets>`; internal names are made SPICE-safe (`#` to `_`).
- A post-layout netlist is the circuit's netlist with its PDK devices removed and an instance of the extracted subcircuit in their place, joined by net name; the PDK model header stays. A net that the test bench uses but the layout does not label is an error naming the net.
- `check_specs` gets `post_layout: {layout, cell?, resistance?}`. It runs the specs on the schematic and on the post-layout netlist and reports both values per spec with the difference. The post-layout netlist replaces the base netlist before variants are made, so when corner sets land each corner runs on it with the same `.lib` section rewrite (the header lines are untouched by the substitution).

### Generators

- Generators are deterministic Rust planners that turn a schematic plus parameters into a drawing plan (pcell instances with parameters and positions, rectangles, vias, labels), and one embedded KLayout script draws any plan with IHP's pcells. Geometry rules come from a table of IHP rule values (fact 7) in the planner.
- Input: the circuit and a role map naming which instances play which part (`{"pair": ["M1","M2"], "load": ["M3","M4"], "tail": "M5", "bias": "M6"}`); device sizes and net names come from the schematic. A generator checks the topology it expects (the pair's sources share a net with the tail's drain, ...) and says what differs.
- Order: `current_mirror`, `diff_pair`, `ota5t`; then `two_stage` and `comparator` if time allows.
- Building blocks: each transistor unit is a pcell instance with a gate strap (poly bar and contact) on one side, a source bus below and a drain bus above in Metal1; units of matched devices are interdigitated (`ABAB`) or common-centroid (`ABBA`) in a row, with dummy units at both ends (gates to the off rail, all terminals to the bulk); NMOS rows get a substrate guard ring, PMOS rows an n-well guard ring in their NWell.
- Router: nets run on Metal3 horizontal tracks above or between rows; every terminal reaches its net's track with a Metal2 vertical stub and stacked vias. Track order is fixed by net order, so the same input always gives the same layout.
- Parameters the agent may change: `pattern` (interdigitated, common_centroid), `dummies`, `guard_ring`, `fingers` (per unit), `track_pitch`, `row_gap`, `tap_spacing`. A `<cell>.layout.json` records them.

### Agent loop

`layout` tool, action `generate`, runs DRC and LVS after drawing (default) and returns the structured violations with hints that name the parameter that usually fixes each rule family (metal spacing: `track_pitch`; well and latch-up rules: `guard_ring`, `row_gap`; LVS net mismatches: the role map). The system prompt and the tool description say GDS is never edited by hand; there is no tool that edits GDS.

### Rendering

`layout` action `render` draws a GDS to PNG with KLayout (`LayoutView`, IHP's layer properties file `sg13g2.lyp`), returned as an image like `render_schematic`.

## Tests

| What | Needs |
|---|---|
| GDS reader: top cells, refusal of non-GDS, oversize and deep files (synthetic GDS written by the test) | nothing |
| lyrdb parser: rules, values to boxes, layer from rule, caps (synthetic reports) | nothing |
| LVS netlist writer: PDK devices to CDL, ports, test-bench parts left out | nothing |
| Post-layout netlist assembly: substitution, unlabelled net error, header kept | nothing |
| Planner: geometry invariants (no two shapes of different nets on one layer closer than the rule), determinism | nothing |
| DRC on a synthetic GDS with a Metal1 spacing violation: rule `M1.b` and its location | KLayout and `AISPICE_PDK_IHP` |
| Generated current mirror, diff pair and 5T OTA: DRC (main, extra, antenna) and LVS clean | KLayout, PDK |
| Removed contact: LVS fails and names the net; added Metal1 sliver: DRC fails with rule and location | KLayout, PDK |
| 5T OTA extracted with Magic simulates; GBW and phase margin before and after layout | KLayout, Magic, PDK |

## CI

The PDK job gains KLayout 0.30.4 (the klayout.org Ubuntu 24.04 package, checksum pinned) and runs the DRC plumbing test on the synthetic GDS (one deck run, about 15 s). Generator, LVS and extraction tests need the pcell submodules and Magic and run locally; whether a generator test fits CI is decided by its measured time.

## As built

Where the implementation differs from the decisions above, and what was measured.

- The generators module is `generator` (`gen` is reserved in Rust 2024). Shapes are drawn flat: the drawing script copies the pcell variants into the top cell, so DRC, LVS and Magic read one cell.
- Dummies share the island's end diffusion (gate to the bulk rail, both sides on the island's source net), so they are real transistors in the layout. `<cell>.layout.json` lists them and LVS adds them to the schematic side, except a dummy whose four terminals are one net, which KLayout drops when it extracts the layout. Magic extracts them too, so post-layout runs include their junction capacitance.
- `tap_spacing` was dropped: `guard_ring` false draws a tap bar under the row. A `comparator` template (StrongARM latch) was added; its cross-coupled NMOS pair is a row of two one-device islands. `two_stage` puts the second stage on its own island in the load row and the compensation capacitor in a row of its own: IHP's `cmim` pcell, the bottom plate through a Metal5 tab and the top plate through a TopMetal1 tab, a TopVia1 and a Metal5 island, each with a via stack down to Metal2. Islands of a row are 0.8 um apart, for `pSD.b` between PMOS islands.
- The `layout` tool checks each try in a run folder and, when DRC fails on a rule a parameter addresses, draws again with that parameter changed (`generator::adjust`, up to `attempts`), before writing the result into the project.
- Magic's `extresist` crashes (Magic 8.3.677, a null node in `ResReadDevice`) when the substrate net carries a label, as ground does in every generated block. Extraction with resistance therefore runs Magic twice: once to learn the substrate's name, then with that net's labels erased; the substrate's pieces are joined back under its name and made a pin, so ground stays one ideal node. `extresist threshold 0` and `mindelay 0` give every other net its network.
- KLayout runs with `QT_QPA_PLATFORM=offscreen` as well as `-b`.
- The report readers gained a pre-parse depth check: a fuzz test found that `roxmltree` recurses into deeply nested XML.

Measured on 2026-10-11 (machine load 4 to 60): a generated block draws in 2 to 6 s; DRC with the main, extra and antenna decks takes 6 to 22 s; LVS 4 to 12 s; Magic extraction 1 s. The 5T OTA (6 devices, 6 dummies, 14.5 x 26 um) is DRC and LVS clean; with its 150 fF MIM load the schematic gives GBW 113.7 MHz and phase margin 84.5 degrees, after layout 105.2 MHz and 82.5 degrees (capacitance only) or 105.9 MHz and 83.4 degrees (with resistor networks). The two-stage op-amp (9 devices, 8 dummies, a 15 x 15 um MIM capacitor, 0.5 pF load, DC loop closed through 1 GH) goes from 51.9 dB, 46.2 MHz and 51.8 degrees to 52.9 dB, 44.2 MHz and 47.4 degrees.

## Out of scope

Chip-level assembly, pads, seal ring, fill and density (A4); xschem import (A2); layouts of passives other than what the generators need; parasitic-aware sizing loops.
