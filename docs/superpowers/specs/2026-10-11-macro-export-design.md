# Macro export and analog-on-top assembly (A4 part 2): design

Date: 2026-10-11. Status: accepted for implementation. Roadmap milestone A4, second half: export an aispice block as a `macro/v1` package (the hand-off format of `docs/macro-package.md`), and assemble an analog-on-top chip from aispice blocks and hardened digital packages, with a pad ring, seal ring and fill, checked with IHP's own DRC and LVS decks. Part 1 of A4 (behavioral models, mixed-signal simulation) is separate work.

Builds on the PDK registry (#36, #37), corner sets (#34), the macro package contract and validator (#35) and the layout stack (#38: generators, DRC, LVS, Magic extraction, post-layout specs, tool access classes).

## Goal

1. `macro_export` (tool) and `aispice macro export` (CLI): turn a template circuit with a generated, DRC and LVS clean layout into `<name>.macro/` that passes aispice's `check_macro` and ChipLeap's `chipleap macro check`.
2. `chip` (tool) and `aispice chip assemble` (CLI): place aispice blocks and digital `macro/v1` packages in a core, surround it with IHP's `sg13g2_io` pad ring, route the few top-level nets, add bond pads, a seal ring and fill, and run full-chip DRC and LVS with IHP's KLayout decks.

## Facts this design rests on

Checked on 2026-10-11 against IHP-Open-PDK `22f2a25`, KLayout 0.30.7 and Magic 8.3.624 (the offload server) and KLayout 0.30.4 and Magic 8.3.677 (this machine).

1. The shared digital fixture `tiny_counter.macro` is synthetic: its GDS is a few rectangles. IHP's DRC deck flags it (`LU.b` in the main deck, `Act.d` in the extra rules), and its SPICE netlist instantiates standard cells its GDS does not contain. It is good for planning and routing tests, and can never be part of a DRC or LVS clean chip.
2. The IO cells pass IHP's main DRC deck on their own (`sg13g2_IOPadAnalog` 11 s, `sg13g2_Corner` 31 s). So do the `sealring` pcell (400 x 400 um core, 25 um edge box) and the `bondpad` pcell (70 um square, Metal3 to TopMetal2 with a passivation opening), on the main and extra decks.
3. IHP's KLayout LVS deck cannot compare the IO cells. Its reader turns the `X` device lines of `libs.ref/sg13g2_io/spice/sg13g2_io.spi` into empty subcircuits, rejects the CDL's resistor lines (`$[res_rppd]`), and the deck's own extraction of `sg13g2_IOPadAnalog` merges `pad` and `padres` (the secondary protection resistor is not extracted). IHP's LVS regression covers devices, standard cells and SRAM, not IO cells.
4. A cell with no devices disappears from both sides of a KLayout comparison (purged), and nets keep the names of top-level labels. A layout whose two pad labels are swapped fails LVS even when the devices match, so top-level labels identify pads reliably.
5. The IO ring rails run through every IO cell at fixed heights from the outer edge: on Metal3, `vdd` is the band 160 to 178 um and `vss` 140 to 158 um; on Metal4, `vss` is 162.2 to 178 um and `vdd` 140 to 155.8 um. A strap from the core reaches `vdd` on Metal3 and `vss` on Metal4 without crossing the other. Supply pads and fillers have nothing above 178 um on Metal3 and Metal4.
6. Core-side signal pins sit on the IO cell's inner edge (`y` 179.71 to 180 um on Metal3): `pad` of `sg13g2_IOPadAnalog` (x 26.105 to 50.875), `p2c` of `sg13g2_IOPadIn` (x 39.725 to 40.225), `c2p` of `sg13g2_IOPadOut4mA` (x 38.145 to 41.855). The bond side is a 70 um wide strip at `y` 0 to 3 on Metal2 to TopMetal2; a 70 um bond pad abuts it outside the cell (as Croc places them).
7. Corner cells are 180 x 180 um, pad cells 80 x 180 um, fillers 1, 2, 5, 10, 20 and 50 um wide; LEF `SYMMETRY X Y R90`.
8. LibreLane (3.0.0.dev44, local container only) writes per-corner liberty files and a Magic-extracted cell-level netlist; on IHP its default power grid uses TopMetal1 and TopMetal2 straps and its IO pins are 0.4 um squares on Metal2 (north and south) and Metal3 (east and west). Its own PDK is at commit `cb7daaa`, not `22f2a25`.
9. IHP's standard-cell CDL (`libs.ref/sg13g2_stdcell/cdl/sg13g2_stdcell.cdl`) uses the `M ... sg13_lv_nmos` form the LVS deck reads.
10. A generated block labels its pins on Metal3 tracks at 0.6 um pitch, inside the block; ground is net `0`, which is not a valid macro pin name.

## Decisions

### Engine: KLayout, Magic and an aispice router

The chip is assembled by one KLayout script from a plan aispice computes in Rust: pad ring, bond pads, macro placement and top-level routes; the seal ring is IHP's `sealring` pcell; fill is Magic's pattern fill with IHP's GDS tech file, as `generate_fill.py` does it; checks are IHP's KLayout decks.

Why not OpenROAD, as Croc does: OpenROAD here is a container wrapper that runs only on this machine, not on the offload server or in CI; the tests could never run where the others do. An analog-on-top chip has a handful of top-level nets that a small deterministic router handles, and owning the router keeps the aispice pattern: the agent changes parameters (placement, pad sides, pitch), the engine produces the geometry, and every run replays. KLayout, Magic and IHP's pcells are already the engines A3 uses, so no new tool, no new process sandbox and no new failure modes. D4 (ChipLeap, digital-on-top) keeps OpenROAD, where hundreds of nets and standard-cell rows make it the right engine; the two tools share the macro package, not code.

Why the pcell seal ring instead of `generate_seal.py`: both come from IHP; the pcell is drawn in the same KLayout run as everything else, while the Magic script builds its Tcl by string concatenation of its arguments, which aispice does not run. Fill keeps Magic, since it is the PDK's documented fill flow, but through aispice's own Tcl (the logic of `generate_fill.py`'s single-process mode) with values passed as environment variables.

### Macro export

Input: a circuit, its generated layout (the GDS with its `<cell>.layout.json` record), a macro name, the ground pin name (default `vss`), optional pin overrides (direction, kind, description), the specs and a corner set, the supply domain (`core`, voltage from the circuit's supply source or 1.2 V), and the output folder (default `macros/<name>.macro`).

Steps, all in a private run folder; nothing reaches the project until every check has passed:

1. **Rebuild the layout for the package.** The record names the generator, roles and parameters, so the block is drawn again, deterministically, with two changes a macro needs: ground net `0` is renamed to the ground pin name (fact 10), and `pin_edge` is on (below). The cell is the macro name. A layout without a record (drawn by hand) is used as it is when its top cell and labels already make valid pin names.
2. **Edge pins.** A new generator parameter `pin_edge`: every port gets a Metal4 column at the block's left side, `0.3 um` wide, `1.2 um` apart on a grid fixed to the block origin, running down to the bottom edge; its net's first Metal3 track is extended left to the column with a Via3. The pin (label, pin shape) is the column's bottom end. Columns are vertical on Metal4 and extensions horizontal on Metal3 at distinct heights, so nothing crosses on one layer. A chip router reaches every pin from below, on a grid, without entering the block. Default off, so existing layouts are unchanged.
3. **DRC** (main, extra, antenna) and **LVS** (the circuit's devices with the renamed ground, the generator's dummies) on the rebuilt GDS; both must be clean.
4. **Extraction** with Magic (capacitance; resistance optional). The extracted netlist is kept in the package as `<name>.pex.spice` for mixed-signal and post-layout simulation.
5. **Characterization.** The specs run at every corner on the schematic and on the post-layout netlist made from this extraction (the ground pin renamed back to `0` for the test bench). Every post-layout value must meet its limits, or the export stops and names the spec: a package never claims a spec it does not meet.
6. **Views.**
   - `<name>.gds`: the rebuilt layout.
   - `<name>.lef`: Magic `lef write -hide` (pins on their layers, the rest of the block as obstructions), read back and checked; `macro.toml`'s `size` is the LEF `SIZE`.
   - `<name>.lib`: written by aispice. `pg_pin`s for supplies with `voltage_map`, `pin` groups with direction, `capacitance` (pF) from the extraction (every capacitor touching the net, ground and coupling), `related_power_pin`/`related_ground_pin`, `is_analog : true` on analog pins, no timing arcs, no leakage (an analog block's quiescent current is not leakage, so it is left out rather than misreported). Characterized at the first corner, as `macro/v1` says.
   - `<name>.v`: a blackbox module (`(* blackbox *)`), supplies behind `ifdef USE_POWER_PINS`; with `model = "wreal"`, analog ports are declared `wreal` behind `ifdef AISPICE_WREAL` so tools without Verilog-AMS still read it.
   - `<name>.spice`: the LVS netlist (CDL form IHP's deck reads) as `.subckt <name> <pins>` with a `*.PININFO` line.
   - `<name>.specs.toml`: per spec, the post-layout values per corner with the spec's limits, and a second entry `<spec>_schematic` with the pre-layout values and no limits, so both are in the table without claiming limits twice.
   - `macro.toml`: pins (directions inferred, below), the supply domain, the corners (`tt_27`, `ss_125`, `ff_m40`, names made valid identifiers), the PDK name and commit (from the registered checkout; export refuses a PDK whose commit is unknown), producer `aispice` and version, `[extra.aispice]` with the generator, parameters and pin pitch.
   - `evidence/`: `drc.json` and `lvs.json` (`macro.evidence/v1`, inputs with SHA-256 of the package files they checked: GDS, and for LVS the SPICE netlist), `pex.json` and `sim.json` (status pass).
7. `check_package` runs on the folder; any error stops the export.
8. The files are written into the project through `Workspace::write_files`: one approval, recorded in the history, undoable. An existing folder is replaced only if it is an aispice package (its `macro.toml` names producer `aispice`).

Pin directions: a port that reaches only gates (or a diode-connected device's gate and drain) is an `input`, one that reaches a drain of a device that is not diode-connected is an `output`, anything else `inout`; supplies are `inout`. Kinds: the ground pin and nets named like grounds are `ground`, nets named like supplies (`vdd`, `vcc`, `avdd`, ...) `power`, others `analog`. Overrides replace any of this per pin.

### Chip assembly

A chip is described by `<name>.chip.toml` in the project:

```toml
name = "demo"
pad_pitch_um = 100            # optional, default 100

[[macros]]
instance = "ota"
package = "macros/ota.macro"  # a folder in the project
at_um = [40, 60]              # optional: lower left in the core; a row by default
nets = { inp = "ota_inp", out = "ota_out" }   # pins not listed join the net of their own name
lvs = "full"                  # or "abstract" (below)

[[pads]]
net = "ota_inp"
side = "west"                 # north, east, south, west
kind = "analog"               # analog, input, output, vdd, vss, iovdd, iovss
```

- **Packages are untrusted input.** Each is resolved inside the project (no `..`, no link out of it), checked with `check_package` against the project's PDK (`name@commit`; another PDK name is an error, another commit a warning), and only then read. File names come from the validated manifest name, never from the package. The GDS is copied into the run folder before KLayout reads it. Its SPICE is reduced to the macro's own `.subckt` block (and subcircuits it defines), refusing any include or control line, before it joins the chip netlist.
- **Pad ring.** Pads of each side in order, evenly spaced at `pad_pitch_um` or more; corners; fillers decompose every gap exactly. Cells by kind: `sg13g2_IOPadAnalog` (core pin `pad`, with `resistor = true` `padres`), `sg13g2_IOPadIn` (`p2c`), `sg13g2_IOPadOut4mA` (`c2p`; `drive = 16` or `30` picks the stronger cells), `sg13g2_IOPadVdd`, `sg13g2_IOPadVss`, `sg13g2_IOPadIOVdd`, `sg13g2_IOPadIOVss`. At least one pad of each supply kind is required. ESD is the IO cells' own: clamps, the DCN and DCP diodes and, on analog pads, the secondary protection. The 2 kV and 4 kV diodes (`esd` pcell: `diodevdd_2kv`, `diodevss_2kv`, `_4kv`) can be added per analog pad with `esd = "2kv"` or `"4kv"`: a pair beside the pad, between its net and the core supplies, which LVS sees as devices.
- **Geometry**, from the die edge inward: seal ring (pcell, 25 um edge box) and 35 um clearance, 70 um bond-pad band, 180 um IO ring, core. Die size follows from the pads per side and the core the macros need.
- **Core power.** Supply pads get a strap into the ring rail (fact 5): Metal3 into `vdd` over each `vdd` pad, Metal4 into `vss` over each `vss` pad. `vdd` and `vss` are then ordinary nets for the router, whose terminals are these straps and every macro's supply pins.
- **Router.** A grid of 0.6 um over the core, two layers (Metal3 horizontal, Metal4 vertical, the other direction at a cost), wires 0.3 um, vias with 0.3 um pads. On that grid, wires and vias of different nets are DRC clean by construction (spacing 0.3 um against a 0.21 um rule; via spacing 0.41 um against 0.22 um). Macros and their 1.2 um halo are blocked on both layers. Each terminal has an access stub: from its pin on the macro or pad edge straight out to the first grid node, with a via up to Metal3 when the pin is on Metal2 (LibreLane pins). Stubs closer than the spacing rule to each other are an error that names both pins. Each net grows as a tree, terminal by terminal, by A* to the nearest tree node; nets route shortest first, and a net that finds no path is moved to the front and routing restarts, up to 8 times. Same input, same routes.
- **Assembly** (one KLayout script, `chip.py`): copies the IO cells and each macro's top cell tree into the chip, places instances, bond pads (pcell) and the seal ring (pcell), draws the routes and labels, and writes `chip_nofill.gds` plus `lvs_view.gds`. Then Magic fills `chip_nofill.gds` and a second KLayout run merges the fill into the final `<name>.gds`.
- **DRC**: main, extra, antenna and density decks on the final GDS.
- **LVS**: on `lvs_view.gds`, the final chip without the IO cells, bond pads, seal ring and fill, against a chip netlist aispice writes: the macros' netlists (with IHP's standard-cell CDL appended when a macro instantiates standard cells) and a top subcircuit whose ports are the pad nets. Each pad's core pin carries a top-level label with its net, so a pad connected to the wrong net fails (fact 4). IO cells are left out because the deck cannot compare them (fact 3); their DRC is checked in full. A macro with `lvs = "abstract"` is left out the same way, its pin nets labelled `<instance>.<pin>`; its producer's LVS evidence covers its inside.
- **Results** go into the project through `write_files` (one approval): the final GDS, the LVS netlist, `<name>.chip.json` (floorplan, routes, decks and counts) and `evidence/drc.json` and `evidence/lvs.json` with the GDS hash. Reports and intermediate files stay in `.aispice/layout/<run>/`.

### Digital macros for the demo

The shared fixture cannot be in a clean chip (fact 1), and ChipLeap does not export packages yet. `crates/aispice-tools/tests/support/harden_counter.py` hardens a two-bit counter with the fixture's interface on LibreLane (local container; the PDK's IO pin and power settings chosen so its pins sit on Metal2 and Metal3 at the boundary and its power on a Metal4 ring) and packages the result as `macro/v1` the way ChipLeap will: liberty from LibreLane's corners, the RTL as the model, a cell-level netlist with the standard cells' port order, evidence from LibreLane's DRC and LVS counts. The chip acceptance test adds it when `AISPICE_DIGITAL_MACRO` names such a package. The package holds PDK cell layouts, so it is a test output, never committed.

### Security

- Every tool declares its class: `macro_export` and `chip` are `Edit`; they write the user's files only through `write_files`. Everything else they produce stays in run folders under `.aispice/layout/`.
- Reads are confined: circuits and layouts through the project's resolvers, packages inside the project and checked first, PDK files only under the registered root.
- Output folders a package or chip needs are made by the project, never by a tool, through one check that `resolve_output`, `missing_dirs` and `create_missing_dirs` all use (`Project::output_walk`). Containment is decided on real paths (symlinks resolved) against the root canonicalized once at open. Whether a path is in `.aispice/` is decided by file identity, not by name, because no name rule matches every file system's folding (APFS normalization and case folding, NTFS's upcase table, 8.3 short names, streams, trailing dots): the state folder is made first if missing (never through a link; a link or a file there stops every write), its identity taken (device and inode from a stat on Unix, so a planted FIFO cannot block it; volume serial and file index through `same-file` on Windows, where std's accessors are unstable), and a path is refused when it or any folder above it up to the root has that identity. Folders are made one level at a time and each is checked again once made; one that resolves outside or into `.aispice/` is removed and stops the write. Name rules stay only as early refusals with a plain message: the first name compared with `.aispice` case-insensitively where the file system folds case (a test-only constructor forces that on Linux), and names Windows opens as another file refused on every platform (a trailing dot or space, a colon, `~` before a digit, a device name such as `CON` or `NUL`). Reads (`resolve`, `open_gds`, package entries) use the same check.
- KLayout and Magic run aispice's scripts with values as `-rd` arguments, environment variables or JSON files in the run folder; every name is checked against the layout tools' name rule, numbers are integers in range, paths absolute without control characters, and each script checks them again.
- Parsers are bounded: `chip.toml` (1 MiB, 64 levels, at most 64 macros, 200 pads and 1000 nets), LEF through the macro package reader's limits, SPICE through the netlist parser and policy, the router's grid (at most 4 million nodes per layer).

## Acceptance

- The 5T OTA and the StrongARM comparator export to packages that pass `check_macro` and `chipleap macro check` (ChipLeap installed in a private virtual environment outside both repositories; the test is skipped without it).
- A demo chip (the two analog packages, a pad ring with supply, analog, input and output pads) assembles to a GDS that is clean on IHP's main, extra, antenna and density DRC decks and on its LVS deck. With `AISPICE_DIGITAL_MACRO`, the same holds with the hardened counter, the comparator's output driving the counter's enable.
- Unit tests without a PDK cover the writers, pin inference, the floorplan, the router (connectivity and spacing on random problems), the chip netlist and the refusal of bad packages, names and paths.
- `npm run check` passes and `docs/TOOLS.md` is regenerated.

## Out of scope, follow-ups

- Mixed-signal simulation of the comparator driving the counter (A4 part 1).
- The shuttle precheck shared with D4.
- Wide supply straps and a core power ring; supply nets route at the signal width, which is enough for the demo's currents and is reported.
- Comparing the IO cells in LVS, once IHP's deck reads their netlists.

## As built

Status 2026-10-12: implemented; the acceptance chip is DRC and LVS clean. Deviations from the decisions above, and what was found on the way:

- **Fill is KLayout, not Magic.** Magic's pattern fill (the logic of `generate_fill.py`, run on the first demo chip) put Metal and Activ filler within the filler spacing rules of the IO cells' own shapes (several thousand `MxFil.c`, `AFil.c`, `GFil.d` and `TMxFil.c` errors) and filled the small chip past the 60 % global maximum on Metal1, Metal3, Metal4 and Metal5: a pad ring is about 80 % metal on those layers and takes up most of a 1 mm die. `klayout/fill.drc` is aispice's own fill with the keep-outs of IHP's KLayout filler macros; Metal1 to Metal5 are filled with 2 um squares at the pitch that brings each layer to 47.5 % of the chip (measured from what is drawn), TopMetal1 and TopMetal2 with IHP's pattern, Activ and GatPoly with IHP's interleaved pattern but GatPoly bars 7.5 um apart instead of 10 (the demo ended at 14.5 % GatPoly against a 15 % minimum). Measured on the demo with the counter: Activ 49 %, GatPoly 19.5 %, Metal1 49 %, Metal2 53 %, Metal3 47 %, TopMetal1 52 %, TopMetal2 51 %.
- **Chip DRC leaves out IHP's recommended rules** (`DrcChecks::recommended`, passed as `no_recommended`): the pad cells with their bond pads abutting, as Croc places them, break `Pad.d1R` and `Pad.fR_*` (recommendations, not rules). Blocks keep them.
- **Output drive is 4 mA only.** `sg13g2_IOPadOut16mA` and `30mA` contain `sg13g2_Clamp_N15N15D` and `P15N15D`, whose Metal1 pins are off grid (`metal1_pin_Offgrid` on IHP's main deck), so a chip using them cannot be DRC clean.
- **Pins on Metal1 and Metal2 are reached through via stacks** whose landing pads reach back along the stub (0.3 x 0.5 um) for the minimum area: a route that leaves a Metal2 pin's stack on Metal4 at once left a 0.09 um2 square of Metal3 (`M3.d`, three errors on the first counter chip). The router's own check now flags any metal piece under the minimum area, so such a case fails at plan time.
- **Block placement** advances by each block's snapped outline, not its nominal width (the first plan put two blocks 0.4 um closer than the gap allows).
- **`lvs = "abstract"` was not built.** A block without devices disappears from KLayout's comparison on both sides, so leaving one out and checking its pins by label (the way pads are checked) needs a label per pin net at the block; until a block needs it, every block is compared in full. The shared digital fixture, whose GDS is a few rectangles, can be planned and routed (`chip_tools.rs`) but never be in a clean chip.
- **The hardened counter** comes from LibreLane's own PDK (commit `cb7daaa`), so the chip reports a `pdk.expected` warning for it; its standard cells compare cleanly against the 22f2a25 cell netlists. Its power grid is one layer of Metal4 straps to the block's edges (`PDN_MULTILAYER` off, `PDN_EXTEND_TO = boundary`), its pins are 0.4 um squares on Metal2 at the edges, and LibreLane's liberty gets `pg_pin` groups and a missing semicolon before packaging. ChipLeap's validator accepts the package once the clock's LEF `USE` says `CLOCK`.
- **Run times** on the offload server (KLayout 0.30.7, Magic 8.3.624, 32 cores shared): macro export 7 to 10 s per block; chip planning under 1 s; drawing, fill and merge about 65 s; DRC (four decks in parallel) and LVS about 125 s. Locally the same tests use KLayout 0.30.4 and Magic 8.3.677.
- Measured results: OTA after layout (tt, ss, ff at 27 C) gain 25.3 dB, GBW 16.7 to 17.8 MHz, phase margin 91 degrees; comparator decision 1.2 V at 2.9 ns. Demo chip: 1040 x 950 um with the two analog blocks and 13 pads, 1040 x 1040 um with the counter and 16 pads.
