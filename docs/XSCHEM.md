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
- **Proof, xschem** runs xschem itself, headless and sandboxed, and compares its netlist with the `.asc`'s. On an export it always runs (the `.sch` is aispice's own, written so nothing in it is evaluated). On an import it runs only when you ask, because the `.sch` is a file aispice did not write and xschem evaluates Tcl from it: pass `--xschem-check`, or start aispice with `AISPICE_XSCHEM_CHECK_IMPORTS=1`. Without that the import says `not checked against xschem` and is still proven by aispice's own netlist. The proof is skipped, with the reason, when xschem is missing, when no sandbox is available (see [Running xschem safely](#running-xschem-safely)), when a symbol is known only from aispice's built-in definitions (xschem would not find it), or when the safety scan refuses the file. A skip is not a failure.
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

xschem evaluates Tcl from the files it opens: `tcleval(...)` property values, `@tcleval` tokens, instance values substituted into a symbol's Tcl, symbols it loads by library path, file names spliced into Tcl commands, and the rc files it sources. A schematic from someone else could use any of these to run commands. aispice never runs Tcl itself. What keeps a machine safe is, in order:

1. **xschem runs on an import only when you ask.** An export is aispice's own file; an import is not, so aispice runs xschem on it only with `--xschem-check` or `AISPICE_XSCHEM_CHECK_IMPORTS=1` in the environment of the process you start. The agent and MCP tools and the content of a schematic cannot turn it on; the model can ask for an import but not for xschem to run on it.
2. **Every xschem run is sandboxed.** aispice uses, in order:
   - **bubblewrap** (`bwrap`) where it can start: new user, PID, IPC, UTS and network namespaces (no network at all), the system folders and xschem's install read-only, the library, model and input files read-only at their own paths, a private `/tmp` and `HOME`, and one writable folder for the netlist;
   - else **Landlock** (Linux 5.13 and later): xschem runs confined to read and execute the system and its own install, read the library, model and input files, write only a private output folder with a private `HOME` and `TMPDIR`, and make no TCP bind or connection;
   - when neither is available, xschem is not run, and the proof is skipped with the reason.

   On Ubuntu 24.04 the default kernel refuses bubblewrap its user namespace (`kernel.apparmor_restrict_unprivileged_userns=1`) unless bwrap has an AppArmor profile, so aispice falls back to Landlock there. To use bubblewrap instead, add a profile that allows the namespace (this needs root), for example in `/etc/apparmor.d/bwrap`:

   ```
   abi <abi/4.0>,
   include <tunables/global>
   profile bwrap /usr/bin/bwrap flags=(unconfined) {
     userns,
     include if exists <local/bwrap>
   }
   ```

   then `sudo apparmor_parser -r /etc/apparmor.d/bwrap`. aispice does not change system configuration; this is yours to decide.
3. **An export writes nothing xschem would evaluate.** Every value in an export comes from the `.asc`, so the export writer lets a value reach the file only where it stays literal: an instance whose values or nets a symbol's Tcl would read gets a plain `format` of its own, or becomes its SPICE line in a code block, and anything holding `tcleval` is left out with a finding. The written file is read back as xschem reads it and the export fails if anything does not round-trip.

A scan of the text (`aispice_core::xschem::safety`) runs before any of this. It is defense in depth, not a security boundary, and refuses before anything runs what is known to be dangerous or not comparable:

- any property value, symbol `format` or text that starts with `tcleval(` and holds `[` or `]`, whose braces do not balance the way Tcl counts them, or that ends in an odd run of backslashes (any of these lets the value leave the Tcl word xschem wraps it in); the reviewer's `value="tcleval(1k} ; <command> ; #{)"` is refused here;
- `tcleval(` together with `[` anywhere in a schematic's or an untrusted symbol's string;
- a value xschem would paste into Tcl that could reshape it: the properties of an instance of a symbol that uses Tcl, label names, and the properties of subcircuit instances must hold no bracket, brace, quote, `;`, backslash or line break (the `value` of xschem's `simulator_commands` symbols, whose Tcl wraps it in braces, may hold brackets as long as it has no brace or backslash);
- bus or vector notation in names and labels, which xschem expands and aispice does not;
- a symbol reference that looks like Tcl, is not a `.sym` or `.sch` file, or leads outside the folders aispice may read;
- a file that cannot be read, or a hierarchy deeper, wider or larger than aispice's netlister follows.

When it does run, xschem gets an argument vector, no window (`-x`), stdin closed, a timeout and capped output, an environment of only `PATH`, `HOME`, `TMPDIR`, `LC_ALL` and the variables the rc reads (nothing inherited), a private `HOME` (so `~/.xschem/xschemrc` is never read), and `--rcfile` pointing at an rc aispice writes that takes every path from environment variables, so no path is ever written into Tcl. Given `--rcfile`, xschem sources no other rc, not even an `xschemrc` in the working folder. Everything an import produces (code-block SPICE, directives, model names, include and lib lines) also passes aispice's netlist policy before it is written or simulated, and an import never opens a file outside the project or a registered PDK because of a path in the `.sch`.
