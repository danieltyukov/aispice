# xschem import and export (A2): design

Date: 2026-10-11. Status: accepted for implementation. Roadmap milestone A2. Builds on the PDK foundation (A1 part 1, PR #36): the IHP SG13G2 device catalog in `aispice_core::pdk`, the `sg13g2\` device symbols and the PDK registry.

## Goal

A designer moves a circuit between aispice and xschem in either direction, and every move is proven: both sides are netlisted and compared, and the result says "same circuit" or lists the differences.

- **Import:** an xschem `.sch` (IHP `sg13g2_pr` devices, xschem's generic devices, labels, pins, code blocks) becomes a readable `.asc`, drawn by aispice's own schematic layout engine.
- **Export:** an `.asc` becomes an xschem `.sch` that opens cleanly in xschem, uses IHP's symbols for PDK devices and xschem's `devices/` symbols for the rest, and netlists to the same circuit.

aispice keeps `.asc` as its editable format (roadmap decision 6). xschem is a format it reads and writes, not a second editor.

## Facts this design rests on

Checked on this machine on 2026-10-11 with xschem 3.4.4 (`/usr/bin/xschem`, Ubuntu package) and IHP-Open-PDK at `22f2a25`, and against xschem's 3.4.4 source.

1. **File format.** A file is a sequence of records, each starting with a tag character: `v` (version), `G K V S E` (global property strings; `K` holds a symbol's own properties, `G` in older files), `N x1 y1 x2 y2 {props}` (wire), `C {symbol} x y rot flip {props}` (instance), `T {text} x y rot flip xscale yscale {props}`, `L B P A` (graphics; `B` on layer 5 is a symbol pin), `[ ... ]` after a `C` record (an embedded symbol), `F` and `#` (skipped). A `{...}` string ends at the first unescaped `}`; `\` escapes the next character. Coordinates are floats; y grows downward, as in LTspice.
2. **Properties** are `key=value` tokens separated by space, tab, newline or `;`. A value may be quoted with `"`; outside escapes, `\` and `"` are dropped and `\x` keeps `x`.
3. **Connectivity** (`netlist.c`): a pin is the centre of its layer-5 box, transformed by the instance's rotation (quarter turns) and flip (applied first). A pin joins a wire when it lies anywhere on the segment; two wires join when an endpoint of one lies on the other; two pins join when they coincide. Labels (`type=label`: `lab_pin`, `lab_wire`, `gnd`, `vdd`, IHP's `sub`) and ports (`ipin`, `opin`, `iopin`) name the net at their pin. Unnamed nets are numbered `net1`, `net2`, ... in a fixed order: wires in file order first, then instance pins in instance and pin order, skipping names already used. A label of a symbol with `global=true` adds a `.GLOBAL` line.
4. **Netlist lines** (`token.c`, `print_spice_element`): the instance's `format` (instance property first, then the symbol's) is expanded: `@name`, `@pinlist` (pins in symbol order, or by `sim_pinnumber` when every pin has one), `@@PIN`, `@#n`, `@symname`, `@spiceprefix` (the instance's `spiceprefix`, normally `X`, prefixed to the element name by `spice.awk`), and `@attr` (instance value, else the symbol template's). A value that starts with `tcleval(` is evaluated as Tcl `subst`, and so is a whole expanded line that starts with `tcleval(`.
5. **Tcl in practice.** IHP's test benches use `tcleval( @value )` on their model code block (`$::MODELS_NGSPICE` substitution only). IHP's `ptap1`, `ntap1` and `pnpMPA` symbols compute parameters with `[ev {arithmetic}]`; xschem's `vsource` and `ammeter` use `[expr {@savecurrent ? "..." : "..."}]`; `simulator_commands` uses `[if {[sim_is_ngspice]} {return {...}}]`. Nothing else in IHP's 48 test benches needs Tcl.
6. **xschem evaluates Tcl from the file while netlisting, and puts paths into Tcl commands.** Instance values are substituted into symbols' `tcleval` strings before evaluation (a `vsource` value of `[exec ...]` runs), and the schematic's own path is spliced into `get_directory [list <path>]`. xschem's own netlisting also runs `exec awk`, so xschem cannot run with `exec` disabled.
7. **Headless netlisting** works without a display: `xschem -n -s -q -x -r --rcfile <rc> -o <dir> -N <name> <file.sch>` takes about 0.1 s. Every IHP test bench in `sg13g2_tests` netlists this way.
8. **IHP test benches** put the model header (`.lib cornerMOSlv.lib mos_tt`) and a `.control ... .endc` block in code blocks, and use ammeters (`Vd D net1 0` plus `.save i(vd)`). Several use `.include <bench>.save`, a file xschem's launcher writes.
9. xschem's `res`, `capa` and `ind` append `m=@m` (template `m=1`); `diode` and `npn` append `area=@area`.

## Decisions

### 1. Where the code lives

| Crate | Module | What |
|---|---|---|
| aispice-core | `xschem::file` | `.sch`/`.sym` reader and writer |
| aispice-core | `xschem::props` | property strings: read, quote, write |
| aispice-core | `xschem::symbol` | symbol definitions, resolution through library folders, built-in definitions |
| aispice-core | `xschem::tcl` | the safe Tcl subset (below) |
| aispice-core | `xschem::netlist` | aispice's own xschem netlister: connectivity, net names, format expansion, hierarchy |
| aispice-core | `xschem::safety` | the scan that decides whether xschem may be run on a file |
| aispice-core | `xschem::import`, `xschem::export` | the two conversions |
| aispice-core | `netlist::compare` | `compare_with` and `CompareOptions` |
| aispice-core | `layout` | PDK devices drawn with the `sg13g2\` symbols; layout with a caller's symbol set |
| aispice-sim | `xschem` | running xschem headless as a netlister |
| aispice-tools | `xschem` | conversion plus proof, shared by the tools and the CLI |
| aispice-tools | `tools::xschem_tools` | `xschem_import`, `xschem_export` |
| aispice-cli | `cmd_xschem` | `aispice xschem import`, `aispice xschem export` |

aispice-core stays free of processes.

### 2. Import

1. Read the `.sch` and resolve every symbol: an embedded definition, then the library folders in xschem's order (absolute path; `./` and `../` relative to the schematic; each library folder; the schematic's folder), then aispice's built-in definitions. Library folders are xschem's `xschem_library/devices` and `xschem_library` (found from the `xschem` executable or `AISPICE_XSCHEM_SHAREDIR`) and the PDK's `libs.tech/xschem` (the project's PDK, else the only registered `ihp-sg13g2` PDK). Built-ins are short definitions written for aispice (pins, pin positions, `type`, `format`, `template`) of xschem's generic devices and IHP's `sg13g2_pr` devices, so import works with neither installed. A gated test checks every built-in against the installed file.
2. Netlist it with aispice's xschem netlister (fact 3 and 4), which reproduces xschem's output line for line for everything in scope. A subcircuit symbol with a schematic beside it becomes a `.subckt` built from that schematic (depth limit 16).
3. Tcl: aispice never runs Tcl. `xschem::tcl` evaluates a fixed subset: backslash, `$var` and `$::var` from a fixed table (`MODELS_NGSPICE`, `MODELS_XYCE`, `SG13G2_MODELS`, `PDK_ROOT`, `PDK`), braces and quotes, and the commands `expr`, `ev` (`expr` then `%.4g`, as xschem does), `if`/`elseif`/`else`, `return`, `sim_is_ngspice` (1) and `sim_is_xyce` (0). `expr` covers numbers, `+ - * / % **`, comparisons, `&& || !`, `?:`, parentheses, double-quoted strings and the math functions `abs sqrt exp log log10 pow min max int double round floor ceil`. Anything else (another command, an unknown variable) is a finding naming the instance, and the element is left out; never a silent drop.
4. The text is then treated as any SPICE deck:
   - `.control ... .endc` blocks are dropped with a finding: they are ngspice scripts, which the netlist policy refuses, and aispice runs its own analyses.
   - The PDK model header (a `.lib` of one of the kit's corner libraries) is dropped with a note: in aispice the project's PDK supplies it. A non-typical section (`mos_ff`) is named in the note.
   - Every other line goes through `netlist::policy::check_lexical`; a refused line is dropped with a finding that quotes the policy's reason.
   - `.GLOBAL` lines are dropped (a flat deck has no use for them).
   - A relative `.include` whose file is not beside the output is kept with a finding.
5. Net names: `GND` (any case) becomes `0`; xschem's automatic names (`netN`) become LTspice-style automatic names (`N001`...), so the drawing labels only the nets the designer named. `m=1` is dropped from R, C and L lines (SPICE's default), which keeps resistor values readable.
6. `layout::from_netlist` draws it. The layout now maps a PDK subcircuit call (`XM1 d g s b sg13_lv_nmos w=.. l=..`) to its `sg13g2\` symbol (MOS as transistors, HBTs as bipolars, the MIM capacitor as a capacitor, poly resistors as three-pin resistors) with the parameters in `SpiceLine`, instead of keeping it as a SPICE line. This also improves `create_schematic` for PDK netlists.
7. Symbols aispice cannot resolve, devices outside the kit's symbol set (for example `npn13G2_5t`) and anything else the layout keeps as a SPICE line are findings.

Out of scope, reported as findings: bus and vector notation (`lab=D[3:0]`, `name=R1[3:0]`), pass-through symbols, `spice_ignore=short`, generator symbols, `lvs_format`, Verilog and VHDL. `device_model` attributes are supported (printed at the end as xschem does).

### 3. Export

1. Netlist the `.asc` with aispice's netlister (in a PDK project, without the absolute-path model header).
2. **Redraw** it with the layout engine using a symbol set shaped like xschem's: for each supported LTspice symbol, a definition with the same attributes, pin names and pin order, but xschem's pin positions (scaled by 1.6, so xschem's 10-unit grid lands on LTspice's 16-unit grid) and a body box. The layout then routes wires that end exactly on xschem pins, and its usual round-trip check proves the drawing netlists back. Coordinates are scaled by 0.625 into the `.sch`; orientation `R k` becomes `rot=k flip=0` and `M k` becomes `rot=(4-k) mod 4 flip=1`.
   - Rejected alternative: keep the `.asc` coordinates and patch each pin with a stub wire. LTspice and xschem symbols differ in proportions, so stubs land mid-wire or on other parts; a redraw is clean by construction and reuses the tested router. The cost is that the xschem drawing does not keep the `.asc` arrangement; this is documented.
3. Symbol mapping:

   | aispice | xschem |
   |---|---|
   | `res`, `cap`, `ind` | `devices/res.sym`, `capa.sym`, `ind.sym` |
   | `voltage`, `Misc\signal`, `current` | `devices/vsource.sym`, `isource.sym` |
   | `diode`, `zener`, `schottky`, `LED` | `devices/diode.sym` |
   | `npn`, `pnp` | `devices/npn.sym`, `pnp.sym` |
   | `nmos4`, `pmos4`; `nmos`, `pmos` | `devices/nmos4.sym`, `pmos4.sym` (three-pin parts with a wire tying bulk to source) |
   | `e`, `g`, `f`, `h`, `bv`, `bi` | `devices/vcvs.sym`, `vccs.sym`, `cccs.sym`, `ccvs.sym`, `bsource.sym` |
   | `sg13g2\<device>` | `sg13g2_pr/<device>.sym` (the body terminal of the poly resistors goes in `body=`, with a label on the sheet) |
   | ground flag, other flags | `devices/gnd.sym`, `devices/lab_pin.sym` |
   | directives, `.model` lines | `devices/code_shown.sym` blocks |
   | PDK model header | a `MODEL` code block with `.lib <corner library> <section>`, as IHP's test benches write it |

   Anything else (op-amp macro models, switches, transmission lines, LTspice library parts, unknown subcircuits) is kept as SPICE text in a code block, with a finding; the layout labels every net that text refers to, so it stays connected.
4. Every instance is checked with aispice's own xschem netlister as it is written. When the symbol's own `format` cannot reproduce aispice's line (a MOSFET with no `w` and `l`, where xschem's template would add them), the instance gets a `format` override that can (`@name @pinlist @value`). xschem honours instance overrides.

### 4. Proof

Every conversion returns a proof: equivalent, or the list of differences.

- **aispice side, always:** import compares aispice's xschem netlist of the `.sch` with aispice's netlist of the new `.asc`; export compares the `.asc`'s netlist with aispice's xschem netlist of the new `.sch`.
- **xschem side, when xschem is installed:** xschem's own netlist of the `.sch` (headless, section 5) is compared with aispice's netlist of the `.asc`. It is skipped, with the reason, when xschem is missing, when a symbol is only known from aispice's built-ins (xschem would not find it), or when the safety scan refuses the file.
- **Comparison** (`compare_with`, options for xschem): elements by name; `GND` and `gnd` equal `0`; automatic names on either side (`N001`, `netN`) may differ as long as they correspond one to one; `name=value` parameters compared as a set with numbers by value (`0.5u` equals `0.5e-6`); `m=1` and `area=1` are SPICE defaults and may be absent on one side; subcircuit bodies compared the same way; directives compared as sets after removing what both sides treat as setup (`.lib`, `.include`, `.global`, `.end`, control blocks, and the lines the import reported as dropped).

The existing `compare` keeps its exact behaviour (LTspice golden tests rely on it).

### 5. Running xschem

`aispice_sim::xschem` finds the executable (`AISPICE_XSCHEM`, else `xschem` on `PATH`) and netlists one file:

- Argument vector only; stdin closed; 30 s timeout; output capped (the existing process runner).
- The environment is cleared. `HOME` is a fresh private temporary folder, so `~/.xschem/xschemrc` is not read; `PATH` is kept for `awk`.
- `--rcfile` points at a fixed rc file aispice writes into that folder. It reads the library path, output folder and model folders from environment variables (`set XSCHEM_LIBRARY_PATH $env(AISPICE_XSCHEM_LIBRARY_PATH)`), so no path is ever spliced into Tcl text.
- Before running, the schematic path, the library folders and every file the schematic pulls in must consist of letters, digits and `/ . _ - + , = @`: xschem splices paths into Tcl commands (fact 6).
- **Safety scan** (`xschem::safety`), before running: refused when the schematic or a sub-schematic contains `tcleval(` with a `[` (Tcl command substitution) anywhere in a property or text; when a symbol outside the trusted folders (xschem's library and the registered PDK) does; when any instance of a symbol that uses `tcleval` has a `[` or `]` in a property value (instance values are substituted into the symbol's Tcl before evaluation); or when a symbol reference contains `[`, `$`, `;` or a backslash. A refusal is reported as "xschem was not run", never as a failure of the conversion. IHP's test benches and aispice's own exports pass the scan.

### 6. Tools, CLI and data

- `xschem_import { schematic: "x.sch", circuit?: "x.asc", effort? }`: writes the `.asc` (refuses to overwrite; through the approval hook), returns the findings, the layout quality and the proof. Data: `{kind: "schematic", circuit, summary}` plus `xschem` (findings and proof), so the existing card shows it.
- `xschem_export { circuit: "x.asc", schematic?: "x.sch" }`: writes the `.sch` (refuses to overwrite; approval hook), returns findings and proof. Data: `{kind: "generic", value: {...}}`.
- `aispice xschem import <file.sch> [-o out.asc] [--force] [--effort N]` and `aispice xschem export <file.asc> [-o out.sch] [--force]`. Exit code 1 when the proof finds a difference, for CI.
- `docs/TOOLS.md` regenerated; README feature list; a short guide in `docs/XSCHEM.md`; CHANGELOG under Unreleased.

### 7. Limits

A `.sch` or `.sym` over 4 MiB is refused; a `{...}` string over 1 MiB, more than 20 000 records, hierarchy deeper than 16, or Tcl nesting deeper than 32 or output over 256 KiB is an error. Every reader loop advances or stops at end of input.

## Tests

Offline, everywhere (fixtures written for these tests, no IHP files): file and property round trips, connectivity cases (pin mid-wire, T junction, crossing wires, labels, shorts, numbering order), format expansion, the Tcl subset (and its refusals), the safety scan, import of small schematics through the built-ins (generic devices, IHP devices, code blocks, hierarchy), export of netlists and of every template, and compare options.

Gated on `AISPICE_PDK_IHP` and `xschem` on `PATH`:

- every built-in symbol definition matches the installed file;
- every `sg13g2_tests` schematic imports, aispice's xschem netlist matches xschem's own, and xschem's netlist matches the imported `.asc`'s netlist;
- every template and a set of IHP circuits export, and xschem's netlist of each `.sch` matches the `.asc`'s.
