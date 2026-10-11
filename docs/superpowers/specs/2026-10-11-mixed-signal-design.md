# Mixed-signal: behavioural models, spec budgets, trim checks, co-simulation

Date: 2026-10-11. Status: accepted for implementation. Milestone A4, part 1 of 2, in `chip-design-roadmap.md`. Part 2 (macro export to `macro/v1` from aispice layouts, analog-on-top assembly) follows the analog layout work (A3).

## Why

aispice designs and verifies one analog block at a time. A mixed-signal chip needs four more things before its blocks can be exported and assembled: blocks described by behaviour rather than transistors, a way to turn a chip-level target into the specs each block must meet, proof that a trimmed block can always be trimmed into range, and simulation of analog blocks together with the digital logic around them. This part adds those, on the simulators and tools already on the machine.

## What the simulators can do

Checked on this machine (ngspice 42, OpenVAF 23.5.0, Verilator 5.020):

| Need | How | Notes |
|---|---|---|
| Verilog-A behavioural models | OpenVAF compiles a `.va` file to an OSDI library; ngspice loads it with `osdi '<file>'` | Works in ngspice's native mode and in `ngbehavior=ltpsa`. A comparator compiles in 0.08 s. OpenVAF itself refuses `$fopen`, `$table_model` and `$system`. |
| XSPICE digital and mixed-signal devices | `A` instances with code models (`adc_bridge`, `dac_bridge`, `d_and`, `d_dff`, ...) | ngspice loads its own code models from its install (the system `spinit`), never from a deck. |
| Verilog co-simulation | XSPICE `d_cosim`, which loads a shared library built from Verilog with Verilator | **ngspice 42 already has it**: its release notes (`/usr/share/doc/ngspice/NEWS.gz`, "Ngspice-42, Dec 24th, 2023") list "Add XSPICE code model d_cosim" and the `vlnggen` script, and the system `digital.cm` contains `d_cosim`. No newer ngspice is needed. |
| Running a program | XSPICE `d_process`, also new in ngspice 42 | Starts the program its `process_file` names. Never allowed. |

A comparator driving a 4-bit Verilog counter through `adc_bridge`, `d_cosim` and `dac_bridge` counts five edges of a 1 kHz input in 5.5 ms, in both ngspice modes.

`d_cosim` takes three port groups, `A<name> [inputs] [outputs] [inouts] <model>`, with `null` for an empty group. Pins follow the Verilog module's ports in declaration order, each bus most significant bit first. ngspice 42's Verilator shim (`scripts/src/verilator_shim.cpp`) has two bugs on multi-bit inputs (`&=` without `~` when clearing a bit, and `|` without `=` for inouts); aispice builds with a private copy that fixes both when it finds them.

Xyce has no XSPICE and its mixed-signal coupling needs an embedding program, and LTspice and Spectre cannot load either kind of library, so behavioural models and co-simulation run on ngspice only.

## Compiled code stays aispice's to load

Both new model kinds are native code that the simulator runs. The rule from the PDK work holds: a deck can never load compiled code. aispice compiles it, from sources it has checked, and loads it itself.

1. **Declared, not named by the deck.** A project lists its behavioural sources in `aispice.toml`:

   ```toml
   veriloga = ["models/comparator.va"]

   [[cosim]]
   model = "counter"            # the name decks use: A1 [..] [..] null counter
   sources = ["rtl/counter.v"]
   top = "counter"              # default: the model name
   ```

   Paths go through `Project::resolve`, so they stay inside the project and outside `.aispice/`. A deck uses a Verilog-A module with an ordinary `.model <name> <module>` card and an `N` instance, and a co-simulated module by its `model` name on an `A` instance. The deck never names a library file.

2. **Checked sources.** Before compiling, aispice reads each source within a size limit and refuses what could reach outside the model:
   - Verilog-A: at most 1 MiB a file. `` `include `` only of `disciplines.vams` and `constants.vams`, which OpenVAF provides. No file input and output (`$fopen`, `$fclose`, `$fdisplay`, `$fwrite`, `$fstrobe`, `$fmonitor`, `$fgets`, `$fscanf`, `$fread`, `$readmem*`, `$writemem*`), `$table_model` or `$system`, even though OpenVAF 23.5 refuses most of them too: the check does not depend on the compiler's version.
   - Verilog: at most 16 files, 1 MiB each, 4 MiB together. Verilator compiles more than Verilog: `$c(...)` and the `` `systemc_* `` and `` `verilog `` blocks put C++ into the build, DPI imports call any C function (`import "DPI-C" function int system(...)`), `$system` runs a shell, and file tasks read and write anywhere. So system tasks and functions come from an allowlist of pure ones (`$display`, `$clog2`, `$signed`, `$time`, `$finish`, ...), and `import`, `export`, `` `include ``, `` `systemc_* ``, `` `verilog ``, `` `verilator_config `` and `$c` are refused. Ports are at most 32 bits wide, the widest the shim handles.

3. **Built privately.** OpenVAF and Verilator run from argument vectors, never a shell, in a private temporary folder holding copies of the checked sources, with a timeout and `HOME` set to that folder. The Verilator build is ngspice's `vlnggen` done step by step: `verilator --cc`, the port headers read out of the generated class, `verilator --cc --build --exe` with ngspice's two shim sources, then `c++ --shared` linking the result. aispice reads compiler messages back with the temporary folder's path replaced by the source's project path and terminal colours removed.

4. **Loaded from bytes aispice produced in this process.** The runner keeps every library it built in memory, keyed by a SHA-256 of the sources, the compiler and its version, with the library's own SHA-256. A copy goes to `.aispice/build/veriloga/` or `.aispice/build/cosim/` for the record (through `Project::state_subdir`, which never writes through a link), but nothing is ever loaded from there: a library found on disk, from an earlier session or a cloned repository, is rebuilt. For a run, the backend writes the bytes into the run's private folder, checks their SHA-256 against the record, and loads them: Verilog-A through the `osdi` lines of the `.spiceinit` it writes, co-simulation through `.model <model> d_cosim simulation="<that file>"` lines it adds to the deck after the policy check. A run loads only the modules its deck uses.

5. **The policy refuses the rest.** A `.model` card of type `d_cosim` or `d_process` is refused wherever it appears, deck or included file. The policy already refused `osdi`, `pre_osdi`, `codemodel` and includes of compiled files. Today no `A` device reaches ngspice at all (the dialect drops LTspice's special-function devices), so XSPICE is enabled only together with these refusals.

**XSPICE devices.** For ngspice, an `A` instance whose model card has a code-model type on an allowlist (bridges, gates, flip-flops, latches, oscillators, sources, analog blocks such as `gain` and `limit`), or whose model is a declared co-simulation model, passes through the dialect unchanged. Every other `A` instance is what it was before: an LTspice special-function device ngspice cannot run. Code models that read files (`d_source`, `d_state`, `file_source`, `table2D`, `table3D`) keep the policy's path rule: only files inside the project.

## Spec budgeting

`aispice_sim::budget` turns a chip-level target into block specs with first-order equations, each shown with its numbers so a designer can follow and override it. The `budget` tool returns the derivation and writes each block's `.specs` file (with approval), in the measurement names `check_specs` evaluates, for a test bench convention the file states in a comment.

**ADC front end** (a switched-capacitor sample or residue stage around an OTA, the block an ADC budget is usually most sensitive to). Inputs: resolution N, target ENOB (or SNR), full scale, sample rate, closed-loop feedback factor β, the share of noise given to kT/C, the share of the half period given to settling and to slewing, temperature.

| Quantity | Equation |
|---|---|
| SNR | 6.02 ENOB + 1.76 dB |
| Total noise allowed | P_n = (V_fs²/8) / 10^(SNR/10) for a full-scale sine |
| Quantization noise | P_q = LSB²/12, LSB = V_fs/2^N; thermal budget P_th = P_n - P_q (refused if not positive: ENOB too close to N) |
| Sampling capacitor | C ≥ 2kT / (α P_th) differential (kT/C per side) |
| OTA input noise | e_n ≤ sqrt((1 - α) P_th / ((π/2) β GBW)), the closed loop's noise bandwidth |
| DC gain | A0 ≥ 2^(N+1)/β, a static error below half an LSB |
| Gain-bandwidth | GBW ≥ (N+1) ln2 / (2π β t_settle), linear settling to half an LSB in t_settle |
| Slew rate | SR ≥ V_step / t_slew |
| Phase margin | at least 60°, so single-pole settling holds |

**LDO.** Inputs: input range, output voltage, maximum load, reference voltage, load and line regulation, PSRR at a frequency, output noise, quiescent current. Derived: divider ratio and resistors from the quiescent budget, pass device on-resistance from the dropout, loop gain from regulation, error amplifier gain and loop unity-gain frequency from PSRR, input-referred noise of reference plus amplifier from the output noise. The block specs are the error amplifier's (open loop) and the closed loop's.

Budgets are pure computation in Rust, so they need no Python, as the roadmap's "Python system models" meant: the system model is the equation set.

## Trim range check

A block with trim bits (a bandgap with a trim DAC, an oscillator with a capacitor bank) passes if, at every corner and for every mismatch sample, some code brings the spec into range. The `trim_check` tool takes the circuit, the `.param` the trim DAC reads, the code range (bits, or first and last code), the specs, a corner set and mismatch or tolerance Monte Carlo, all as the `monte_carlo` tool takes them. For each corner and sample it simulates every code (exhaustive, so the answer is a proof for those samples rather than a search that assumes monotonicity), and reports:

- whether every sample can be trimmed, and the samples that cannot, with their best margin;
- the code each sample needs (the one with the largest margin), as a histogram;
- the headroom: how many codes remain at each end of the range, so a range that only just covers the spread is visible;
- the untrimmed (centre code) yield, for comparison.

Codes times samples times corners are capped at 2000 runs, counted before anything is built, like the Monte Carlo limits.

## Doctor and settings

`aispice doctor` lists OpenVAF, Verilator, a C++ compiler and `make`, and whether the ngspice in use can co-simulate (its `d_cosim` shim sources next to it). `config.toml` gains `openvaf_path` and `verilator_path`. The ngspice path setting already exists, so a newer ngspice can be used without replacing the system one.

## Demo

`examples/mixed_signal/`: a comparator (a Verilog-A model, and a generic behavioural one) drives a 4-bit Verilog counter co-simulated in ngspice; the test checks the count. The IHP StrongARM comparator joins it when the IC templates land.

## How part 2 plugs in

- An analog macro's Verilog model (`macro/v1` needs one) can be the Verilog-A module's port list written as a blackbox or `wreal` module; `aispice_sim::behavioral` exposes each module's ports for that.
- A hardened digital macro from ChipLeap carries a Verilog model; listing it under `[[cosim]]` co-simulates the macro inside an analog-on-top test bench unchanged.
- Budgets write block `.specs` files, the same files `check_specs` and corner runs evaluate, whose results become the macro's measured spec table.

## Limits

| What | Limit |
|---|---|
| Verilog-A source | 1 MiB a file |
| Verilog sources for one model | 16 files, 1 MiB each, 4 MiB together |
| Behavioural models a project declares | 32 |
| OpenVAF compile | 120 s |
| Verilator build | 600 s |
| Built library kept | 64 MiB |
| Trim check | 2000 runs, 64 codes |

## Alternatives considered

- **Load the `.osdi` or `.so` from the project's build folder.** Rejected: any file there could have been planted, and checking a path then loading it leaves a gap. Bytes held in memory and written into the private run folder close both.
- **Let decks name a Verilog-A file with an aispice-specific directive.** Rejected: declaring sources in `aispice.toml` keeps the deck valid SPICE and keeps compile decisions out of text a model writes.
- **Build a newer ngspice.** Not needed: ngspice 42 has `d_cosim`. A configured `ngspice_path` covers a newer one when wanted.
- **Icarus Verilog through `ivlng`.** ngspice 42 ships only the Verilator path; Verilator also refuses less Verilog at run time, since it compiles to C++.
- **Xyce mixed signal.** Needs a host program embedding Xyce; out of scope.
