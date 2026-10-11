# xschem import and export

[xschem](https://xschem.sourceforge.io) is the schematic editor the open PDKs ship symbols and test benches for. aispice keeps LTspice's `.asc` as its own format and moves circuits to and from xschem's `.sch`, proving every move by netlisting both sides and comparing them.

```sh
aispice xschem import dc_lv_nmos.sch            # writes dc_lv_nmos.asc
aispice xschem export amp.asc -o amp_x.sch      # writes amp_x.sch
```

In the agent, the same conversions are the `xschem_import` and `xschem_export` tools. Both write only after approval and never replace an existing file; the CLI replaces one with `--force`.

## What a conversion reports

Each conversion prints its proofs, its findings and its notes:

```text
wrote dc_lv_nmos.asc (layout quality 100/100)
Proof, aispice: same circuit (aispice's xschem netlist of the .sch against the .asc's netlist)
Proof, xschem: same circuit (xschem's own netlist of the .sch against the .asc's netlist)
2 finding(s):
  .control: a .control block of 8 lines is dropped: ... it ran `.op`, `.dc Vds 0 1.2 0.01 Vgs 0.3 1.0 0.1`, which can be added as directives
  .include: `.include dc_lv_nmos.save` is kept, but dc_lv_nmos.save is not beside the new schematic ...
  note: the IHP SG13G2 130 nm SiGe BiCMOS model header `.lib cornerMOSlv.lib mos_tt` is dropped: in aispice the project's PDK supplies it
```

- **Proof, aispice** always runs. aispice netlists the `.sch` with its own xschem netlister, which follows xschem's netlister step by step, and compares it with the `.asc`'s netlist: the same elements with the same values, connected the same way. Ground is `0` on one side and `GND` on the other, numbered nets (`net3`, `N001`) may be numbered differently as long as they connect the same pins, parameters compare by value (`0.5u` is `0.5e-6`), and `m=1` may be left out.
- **Proof, xschem** runs xschem itself, headless, when it is installed, and compares its netlist with the `.asc`'s. It is skipped, with the reason, when xschem is missing, when a symbol is known only from aispice's built-in definitions (xschem would not find it), or when the safety check below refuses the file. A skip is not a failure.
- **Findings** are what could not be carried over, each naming what it is about. **Notes** are what changed on purpose.

The CLI exits with code 1 when a proof finds a difference, so conversions can run in CI. A conversion aispice cannot prove is not written.

## Import

The `.sch` is netlisted, made into a deck aispice may run, and drawn by aispice's schematic layout engine:

- IHP SG13G2 devices become aispice's `sg13g2\` symbols (MOSFETs and HBTs as transistors, the MIM capacitor and the poly resistors, with their body pin, as passive parts). xschem's generic devices become LTspice's.
- `.control ... .endc` blocks are dropped: they are ngspice scripts, which aispice's netlist policy refuses, and aispice runs its own analyses. The finding names the analyses the block ran.
- The PDK's model header is dropped; in aispice the project's PDK supplies it. A corner other than the typical one is named in the note.
- `.GLOBAL` lines are dropped, `GND` becomes `0`, xschem's numbered nets are numbered the LTspice way, and `m=1` is dropped from R, C and L lines.
- A relative `.include` whose file is not beside the new `.asc` is kept with a finding. Any other line the netlist policy refuses is dropped with a finding quoting the reason.
- Parts aispice has no symbol for (for example `npn13G2_5t`) stay SPICE lines on the sheet, with a finding.

Symbols are found the way xschem finds them: an embedded definition, then an absolute path, a path relative to the schematic (`./`, `../`), each library folder, and the schematic's folder. The library folders are xschem's own (found beside the `xschem` executable, or `AISPICE_XSCHEM_SHAREDIR`) and the PDK's `libs.tech/xschem` (the project's PDK, else the only registered one). When nothing on disk matches, aispice's built-in definitions of xschem's generic devices and IHP's devices are used, so import works with neither installed. Files are read only inside the library folders and the project (for the CLI, the schematic's folder and the current folder).

Subcircuit symbols with a schematic beside them become `.subckt` definitions, up to 16 levels deep. Bus and vector notation, `lvs_format`, Verilog and VHDL are out of scope and reported as findings.

## Export

The `.asc` is netlisted and redrawn with xschem's symbols: `devices/` for generic parts and IHP's `sg13g2_pr` symbols for PDK devices. The drawing does not keep the `.asc` arrangement: LTspice and xschem symbols differ in proportions, so aispice redraws the circuit with xschem's pin positions rather than patching wires onto the old one.

- Directives become code blocks. In a PDK circuit, the model header becomes a `MODEL` block with one `.lib <corner library> <section>` per device class, as IHP's test benches write it.
- A poly resistor's body terminal goes in its `body=` property, with a label on the sheet.
- A three-pin LTspice MOSFET becomes a four-pin one with its bulk wired to its source.
- Every instance is checked with aispice's xschem netlister as it is written. One whose symbol's `format` would write a different line (a MOSFET without `w` and `l`, which xschem's template would add; a BJT, which xschem draws without a substrate pin) gets a `format` of its own that writes the same line. xschem honours it.
- Parts xschem has no symbol for (op-amp macro models, switches, transmission lines, library parts) stay SPICE lines in a code block, with a finding. Every net those lines use is labelled, so they stay connected.

## Running xschem safely

xschem evaluates Tcl from the files it opens: `tcleval(...)` properties, `@tcleval` tokens, instance values substituted into a symbol's Tcl, and file names spliced into Tcl commands. A schematic from someone else could use any of these to run commands. aispice never runs Tcl itself: it evaluates a small side-effect-free subset (`expr`, IHP's `ev`, `if`, `return`, `sim_is_*`, and the model folder variables), and anything else is a finding. Before it asks xschem for a netlist, it reads every file xschem would read and refuses to run xschem when:

- a schematic, or a symbol outside xschem's library and the registered PDK, has Tcl command substitution (`[...]`) in a `tcleval` property or text;
- an instance of a symbol whose properties use Tcl has `[` or `]` in its own properties (the `value` of xschem's `simulator_commands` symbols, which their Tcl wraps in braces, may hold brackets as long as it has no braces or backslashes);
- a symbol reference looks like Tcl, is not a `.sym` or `.sch` file, or leads outside the folders aispice may read;
- a path xschem would see has characters other than letters, digits and `/ . _ - + , = @`.

When it does run, xschem gets an argument vector, no window (`-x`), stdin closed, a timeout, an empty environment except `PATH`, a fresh private `HOME` (so `~/.xschem/xschemrc` is never read), and an rc file that takes every path from environment variables, so no path is ever written into Tcl.
