# Analysis depth: noise, PSRR and CMRR, corners, mismatch Monte Carlo

Date: 2026-10-11. Status: accepted for implementation. Milestone A1, part 2 of 3, in `chip-design-roadmap.md`.

## Why

aispice measures gain, bandwidth and margins on one simulation of one circuit. Transistor-level design needs more: the noise a block adds, how well it rejects its supply and its common mode, and whether it still meets its specs at every process, voltage and temperature corner and under device mismatch. This part adds those analyses without depending on any PDK. Part 1 (another branch) adds the PDK registry, OSDI loading and device symbols; part 3 adds the CMOS templates and PDK evals.

## Contract with the PDK work

A corner-capable model include is a netlist line `.lib <path> <family>_<corner>[_mismatch|_stat]`. IHP SG13G2 has `mos_{tt,ss,ff,sf,fs}[_mismatch]`, `mos_tt_stat`, `res|cap|hbt_{typ,bcs,wcs}[_mismatch]`, `res|cap|hbt_typ_stat`, `dio_{tt,ss,ff}` and `dio_tt_stat`. Everything here works on any netlist with such lines, so it is tested with small libraries written for the tests (level 1 models and plain resistors), never with PDK files.

## Simulators

| | ngspice 42 | Xyce 7.10 | LTspice XVII | Spectre 19.1 (SPICE mode) |
|---|---|---|---|---|
| `.noise` | yes, raw plot `Noise Spectral Density Curves` (`onoise_spectrum`, `inoise_spectrum` in V/√Hz) and `Integrated Noise` | yes, but spectra only in `<deck>_noise.dat` (Tecplot text, V²/Hz); the raw file holds the small-signal node solution | yes, raw `V(onoise)`, `V(inoise)`, `gain` and one vector per noisy part, V/√Hz | yes, raw `out` and `in` in V/√Hz, `gain`, and per-device contributions in V²/Hz |
| `.temp` | yes | ignored with a warning; `.options device temp=` works | yes | yes |
| `.lib file section` | not under `ngbehavior=ltpsa` (see below) | yes | no (one-argument include only) | yes |
| random `agauss`/`gauss` per run | yes, seeded with `.options seed=N` | only inside a sampling analysis: `.sampling useExpr=true` with `.options samples numsamples=1 seed=N` gives one seeded draw per run | not used (no PDK runs on LTspice) | no: returns the nominal value outside Spectre's own `montecarlo` analysis |

These were checked on this machine (ngspice, Xyce, LTspice under Wine) and on a remote Spectre with generic decks.

## Reading library files safely

Corners, mismatch and section inlining read files a netlist names, so they sit on the security boundary. They share the netlist policy's own reading of include lines (`netlist::logical_lines`, `netlist::parse_include`) and its resolver (`netlist::resolve_include`: the base folder, `..`, symbolic links, allowed folders). `aispice_sim::library::LibraryFiles` opens a file only after the policy has approved that exact resolved path, reads only regular files of at most 16 MiB, at most 32 files and 64 MiB in one call, each file once, and answers each library's sections once. A `.lib` line whose file the policy refuses stops the corner or the Monte Carlo with the policy's reason. Section names are listed in a message only for libraries inside the project. Every rewritten netlist goes through `Runner::run_netlist_with`, which checks the policy on the netlist's include lines before model resolution reads any file, and on the final deck before and after inlining.

**The policy reads as the simulators do.** A check is only as good as its reading of the text, and the policy and the simulators disagreed. Read against ngspice 42's `inpcom.c` and Xyce 7.10's `N_IO_SpiceSeparatedFieldTool.C`, and confirmed with a decoy file outside the project:

| Spelling | ngspice | Xyce |
|---|---|---|
| `.include link.lib;x`, `.include a.lib $x` | strips `;` comments, and `$` after a blank (not in `ps` mode) | strips `;` comments |
| `.include dir/link//x` | strips from `//` | reads the whole name |
| `.include 'a.lib'`, `{a.lib}` | single quotes quote | single quotes and braces make an expression, stripped from a file name |
| `.include a(b).lib`, `a,b`, `a=b` | one name | splits the name there |
| `.lib "my lib.lib" sec` (native mode) | reads `my` as the file | reads the quoted name |
| `  .include a.lib` (indented) | reads it | a comment |
| `+` after a `*`, `$`, `#`, `;` or indented line | joins the last line not starting with `*`, `$`, `#` | joins the last line not starting with `*`, `;` or a blank |
| `.include` on the first line | includes it | the title |
| lone CR, `\\` at a line end | drops the CR; joins the next line | ends the line; nothing |

So the policy checks a text under both joining rules, accepts an include line in one form only (the keyword, the file bare or in double quotes, for `.lib` at most a section name; nothing after it, on one physical line, no `"`, `'`, `;`, `$`, `` ` ``, brackets, braces, `,`, `=`, `%` or `//` in the name, no space in a `.lib` name, no `\` outside Windows), refuses lone carriage returns, the Unicode line separators, a `+` line with nothing to continue and a line ending in `\\`, and checks a title that starts with a dot. `netlist::canonical_includes` then rewrites the deck's own include lines to name the resolved file in that form, so the simulator opens exactly what was checked; library files are not rewritten, which is why the grammar is strict for them too. `tests/include_differential.rs` runs about fifty spellings, in an included file and as a deck line through the runner, on ngspice in native and LTspice mode and on Xyce: each is refused, or the simulators never open the decoy (by the value they print, their log and, where installed, `strace`). It also shows the refused spellings that do reach the decoy when run unchecked.

A registered PDK's files get every rule but the path heuristic on parameter expressions (`w/ng` is division), and only files whose resolved path lies under a registered root.

**ngspice and library sections.** In its LTspice and PSpice modes (`lt`, `ltps`, `ltpsa`, `ps`) ngspice treats a one-argument `.lib name` as an include, including the section headers inside a library file, so `.lib corner.lib res_typ` fails with "Could not find include file res_typ". aispice runs ngspice in `ltpsa` mode, which its LTspice-flavoured netlists need. So for ngspice (and LTspice) aispice expands each two-argument `.lib <file> <section>` itself before the run (`library::expand_lib_sections`): the line is replaced by that section's lines, with relative `.include` and `.lib` paths inside it resolved against the library's folder as the policy resolves them and written in full, recursively up to the policy's include depth. The expanded deck goes through the netlist policy again. Xyce and Spectre read sections natively and get the line as written. A PDK deck runs ngspice in its native mode, where sections work, so it is not inlined.

## Limits

Nothing is built before its size is known. A corner set lists at most 64 process corners, 64 temperatures and 8 supplies of at most 64 values each; longer lists are refused while they are read. `CornerSet::count` gives the number of corners from the list lengths alone and refuses more than 200. Monte Carlo runs times corners are capped at 500 and optimizer candidates times corners at 2000, both from the count, before any netlist is made: a request over the limit is refused with the largest number that fits, and the defaults (50 runs, 80 candidates) shrink to fit with a note.

## Noise

**Reading.** Noise datasets are normalised so every simulator gives the same vectors: `V(onoise)` (output noise density) and `V(inoise)` (input-referred density) over a `frequency` axis, both in V/√Hz, or A/√Hz for the input when the input source is a current source. ngspice's names are renamed in the raw reader; Spectre's `out` and `in` (units `V/sqrt(Hz)`) too. The Xyce backend reads `<stem>.cir_noise.dat`, takes square roots, and appends the two vectors to the raw file's noise plot (or adds a noise dataset when there is none). The backends mark `V(inoise)` as a current when the deck's `.noise` line names an `I` source.

**Measurements.**

| Kind | Value |
|---|---|
| `output_noise_at(freq)` | output noise density at a frequency, V/√Hz |
| `input_noise_at(freq)` | input-referred noise density at a frequency |
| `output_noise_rms([from, to])` | integrated output noise, the square root of the integral of the density squared over the band (default: the whole sweep), V rms |
| `input_noise_rms([from, to])` | the same for the input-referred noise |

Integration assumes the density squared follows a power law between two points (exact for white and 1/f segments, and what ngspice does for its own totals), falling back to the trapezoid when a point is zero.

**Analysis kinds.** AC measurements no longer accept a noise dataset, which also has a frequency axis, and noise measurements accept only noise datasets.

**Verification.** A 1k/1k divider driven by a voltage source: the output density equals sqrt(4kT x 500 ohm) at 27 C within 2 percent on ngspice, Xyce and LTspice, the input-referred density is twice that, and the integrated noise over 1 Hz to 1 MHz equals the density times sqrt(1 MHz - 1 Hz). A common-source stage with a level 1 NMOS (no flicker noise, lambda 0) and a 10k drain resistor: the input-referred density matches sqrt(8kT/(3 gm) + 4kT/(gm² Rd)) within 10 percent with gm from the square law.

## PSRR and CMRR

**Convention.** The circuit's own AC analysis gives the signal gain, so its input is driven with `AC 1` (a differential input with `AC 0.5` and `AC 0.5 180` on its two halves). For a PSRR or CMRR measurement aispice derives one extra AC run per named source: every `AC` stimulus is removed from every independent source and `AC 1` is placed on the named one, the supply for PSRR or the common-mode source for CMRR. Then

`psrr_db_at(expr, source, freq)` = `cmrr_db_at(expr, source, freq)` = 20 log10(|expr| in the circuit's AC run / |expr| in the derived run) at `freq`.

`expr` is the output (`V(out)` or `V(outp,outn)`), not a ratio with the input, because the input carries no stimulus in the derived run. A CMRR test bench feeds both inputs from one common-mode source (`Vcm`) with the differential sources in series. The two kinds compute the same thing; their names say which test the spec is. The derived datasets are appended to the run's datasets with the title `aispice: AC 1 on <source>`, and only these two measurements read them.

Every place that evaluates measurements on a simulation (simulate, measure, check_specs, sweep, monte_carlo, optimize, corners, the eval judge) runs the derived AC runs its measurements need, on the same netlist variant.

**Verification.** Controlled sources with known gains: `out = 100 V(inp) + 0.01 V(vdd)` gives a PSRR of exactly 80 dB; `out = 1000 (V(inp) - V(inn)) + 0.1 V(cm)` with `cm` the average of the two inputs gives a CMRR of exactly 80 dB.

## Corner sets

A corner set is three lists, all optional, crossed with each other:

```json
{
  "process": ["tt", "ss", "ff", "sf", "fs"],
  "temperatures": [-40, 27, 125],
  "supplies": [{"target": "VDD", "values": [1.62, 1.8, 1.98]}]
}
```

- **Process.** A preset or a map from family to corner. Presets: `tt` (mos tt, res/cap/hbt typ, dio tt), `ss` (mos ss, res/cap/hbt wcs, dio ss), `ff` (mos ff, res/cap/hbt bcs, dio ff), `sf` and `fs` (MOS skewed, everything else typical). A map is written `mos=ss res=wcs`; families it leaves out stay as the netlist has them. The rewrite changes the corner part of each top-level `.lib <file> <family>_<corner>[_suffix]` section name and keeps the suffix. Families the netlist does not use are ignored. A process corner on a netlist with no corner-capable `.lib` line is an error, and so is a section the library does not define, when the library can be read (the error lists the sections it has).
- **Temperatures** in Celsius: every `.temp` line and `temp=` option is removed and `.temp T` added. The Xyce dialect now writes `.temp T` as `.options device temp=T`, since Xyce ignores `.temp`.
- **Supplies**: a source's DC value or a `.param`, set as `sweep` does.

Corners are named from their parts, `ss/125C/VDD=1.62`, and ordered process first, supplies last varying fastest. A set has at most 200 corners. Each corner is one netlist variant run through the existing batch runner, six at a time.

**Reports.** For each spec: the worst value over the corners (the smallest margin, or a corner with no value), its margin and the corner it came from; then the full table of every spec at every corner. A spec passes only when it passes at every corner. The worst-case rows form an ordinary spec report, so the desktop app's spec card shows them unchanged.

**Tools.** `check_specs`, `optimize` and `monte_carlo` take `corners`. `optimize` minimises the worst case: each candidate is simulated at every corner and scored on the worst-case report (for a goal without limits, the worst value for its sense). The simulation budget counts candidates; candidates times corners is capped at 2000 simulations. `aispice check` takes `--process tt,ss`, `--temp -40,27,125` and `--supply VDD=1.62,1.8,1.98`.

## Mismatch Monte Carlo

`monte_carlo` takes `mismatch`: `local` switches each corner-capable `.lib` section to its `_mismatch` variant (device mismatch around the process corner), `global` to its `_stat` variant (global process spread plus mismatch, defined around the typical corner only, so a non-typical process corner with `global` is an error). A family whose library lacks the variant keeps its section, with a note (IHP's diodes have no `_mismatch` section). Part tolerances still work as before and combine with mismatch; `tolerances` may now be empty when `mismatch` is set.

Each run gets its own simulator seed, derived from the Monte Carlo seed and the run number, so a seed reproduces the same circuits and more runs keep the first ones. The seed goes into the netlist as `.options seed=N`; ngspice reads it directly, and the Xyce dialect turns it into `.sampling useExpr=true` and `.options samples numsamples=1 seed=N`, which makes Xyce draw `agauss` and its relatives once, seeded. `.sampling` joins the policy's allowlist. Mismatch Monte Carlo refuses LTspice and Spectre, which would silently return nominal values. Part tolerances keep their per-target streams, untouched by the simulator seed.

With `corners`, every corner runs the same `runs` samples (the same seeds, so corners compare like for like), and the report gives the yield over all runs and per corner. Runs times corners is capped at 500.

**Verification.** A toy library whose `res_typ_mismatch` section gives each instance of a resistor subcircuit an independent `agauss` deviation of 1 percent sigma, and whose `res_typ_stat` section adds a shared global deviation. A divider of two such resistors: with local mismatch the output's relative spread is 0.71 percent (1/sqrt(2) of the resistor sigma) within the sampling error of 200 runs on ngspice and on Xyce, its mean stays at nominal; with global variation the divider ratio is unchanged by the shared part while a single resistor's current spreads by the global sigma. The same seed gives the same values twice, and the first 50 of 100 runs equal a 50-run analysis.

## Agent surface

- Tool schemas: the new arguments above, with short descriptions; `MEASURE_SYNTAX` and `SPEC_SYNTAX` list the new kinds and the PSRR convention.
- `docs/TOOLS.md` regenerated, README features, CHANGELOG under Unreleased, ARCHITECTURE module table.
- Evals: `noise-tia` (size a front-end resistor so the input noise current and the output noise voltage at 1 kHz both meet a budget, judged with `.noise`; only one E24 value does) and `temp-corners` (choose C1 so an RC corner stays in band from -40 C to 125 C with a resistor that drifts, judged at every temperature; the value a room-temperature design picks fails at 125 C). The judge runs the derived AC runs and an optional `corners` set from `task.toml`.

## Public API for later parts

- `aispice_sim::corner::{CornerSet, Supply, Corner, Process, CornerError, CornerReport, CornerRow, apply, variants, set_temperature, summarize, worst_case, preset, parse_section, LibSection, SectionKind, SectionUse, section_uses, set_section, SectionLookup, no_lookup, Found, find_section, FAMILIES, MAX_CORNERS, MAX_LIST, MAX_SUPPLIES}` and `CornerSet::count`.
- `aispice_sim::library::{LibraryFiles, LibraryInfo, read_library, lib_sections, expand_lib_sections, MAX_LIBRARY_BYTES, MAX_LIBRARY_FILES, MAX_TOTAL_BYTES}`.
- `aispice_core::netlist::{logical_lines, parse_include, Include, resolve_include, canonical_includes}`: the policy's own line reader (`logical_lines(text, has_title)`), include parser (`parse_include` gives `None` for another line and the reason for an include in any other form), resolver, and the rewrite of a deck's include lines into the checked form, for anything else that reads include lines.
- `aispice_tools::runner::RunnerConfig::ngspice_exe`.
- `aispice_sim::montecarlo::{Mismatch, variants_with, sim_seed, statistical}`, and `Variant::seed`.
- `aispice_sim::noise::{ONOISE, INOISE, integrate_rms, read_xyce_noise_dat, merge_xyce_noise, input_source, mark_input_quantity}`.
- `aispice_sim::stimulus::{ac_on, needed_sources, mark, marked_source, StimulusError}` and `Measure::stimulus_source`.
- `aispice_sim::optimize::Objective::score_worst`.
- `aispice_tools::runner::Runner::{policy_for, base_dir}`: the policy every deck is checked against, which also decides which libraries a corner lookup may read. PDK roots belong there.
- Measurement kinds `output_noise_at`, `input_noise_at`, `output_noise_rms`, `input_noise_rms`, `psrr_db_at`, `cmrr_db_at`.
- Tool arguments: `check_specs.corners`, `optimize.corners`, `monte_carlo.corners`, `monte_carlo.mismatch`.

## Not in this part

- Noise figure and SNR measurements, `.noise` with a stepped sweep.
- Corner sets stored beside a circuit (a `.corners` file) and corners for `sweep`.
- Mismatch Monte Carlo on Spectre (it needs Spectre's own `montecarlo` analysis) and on LTspice. On Xyce with IHP SG13G2: its Xyce corner libraries have no `_mismatch` sections, and their `_stat` sections leave `mc_ok` and the `*_vfbo_mm` multipliers undefined, so only ngspice runs IHP mismatch.
- Expanding `.lib` sections inside included files for ngspice: only the deck's own lines and what they pull in are expanded.
