# PDK foundation (A1, part 1 of 3): design

Date: 2026-10-11. Status: accepted for implementation. Roadmap milestone A1 ("IC foundations"), first of three parts. Corners, mismatch, noise and PSRR/CMRR are part 2, built in parallel on `feat/analysis-corners-noise`; CMOS templates and IC evals are part 3.

## Goal

A user registers an installed PDK once, a project opts into it, and from then on aispice draws, lints, netlists and simulates transistor-level circuits in that PDK on ngspice and Xyce, with no weakening of the netlist security policy. The first PDK is IHP SG13G2 (IHP-Open-PDK at commit `22f2a25f1734796de3debbbf29cf697cbbc54081`). A gm/Id tool sizes MOS devices from cached lookup tables.

Board-level projects (no PDK) behave exactly as before.

## Facts this design rests on

Checked on this machine on 2026-10-11 against the pinned PDK commit, ngspice 42 and Xyce 7.10.

1. IHP's MOS models are PSP 103.6 in Verilog-A, compiled to OSDI (`libs.tech/ngspice/osdi/{psp103,psp103_nqs,r3_cmc,mosvar}.osdi`). The `.osdi` files are not in the PDK's git tree; `libs.tech/verilog-a/openvaf-compile-va.sh` builds them with OpenVAF. IHP's own `.spiceinit` loads them with `osdi '<path>'`.
2. The model header in IHP's xschem test benches is one `.lib cornerXXX.lib <section>` line per device class: `cornerMOSlv.lib mos_tt`, `cornerMOShv.lib mos_tt`, `cornerRES.lib res_typ`, `cornerCAP.lib cap_typ`, `cornerDIO.lib dio_tt`, `cornerHBT.lib hbt_typ` (xschem netlists these with bare file names and relies on ngspice's `sourcepath`).
3. **ngspice's LTspice and PSpice compatibility modes break IHP's models.** With `ngbehavior` containing `lt` or `ps` (aispice's default is `ltpsa`), ngspice 42 rewrites `.lib file section` to `.include file section` and fails ("Could not find include file mos_tt"), in the deck and in included files alike. With the section inlined by hand the deck runs but the answer is wrong: an inverter at Vin = 0 gives 0.75 V instead of 1.2 V. Without those flags the same deck gives the right curve. So PDK runs use ngspice's native mode.
4. Xyce's IHP models (`libs.tech/xyce/models`) call `YPSP103_VA` and `Yr3_cmc` devices, which exist only as Xyce plugins built from IHP's Verilog-A with ADMS (`libs.tech/verilog-a/adms-compile-va.sh`, output `libs.tech/xyce/plugins/*.so`, loaded with `Xyce -plugin`). A statically linked Xyce, like `/usr/local/bin/Xyce` here, cannot load plugins. Xyce's `sg13g2_hbt_mod.lib` is empty; IHP's Xyce test benches take the HBT from `libs.tech/ngspice/models/cornerHBT.lib`. Xyce's corner libraries have `_stat` sections but no `_mismatch` sections.
5. IHP's model files use expressions such as `as='max(w/ng,wmin)*...'`. The policy's "anything with a `/` is a path" heuristic refuses those lines.
6. PSP exposes operating-point values to ngspice as `@n.<instance>.n<subckt>[gm]` (also `gds`, `ids`, `cgg`, `vth`, `vdss`), and they can be saved across a nested `.dc` sweep.

## Decisions

### 1. PDK registry

- A user-level file `<config dir>/aispice/pdks.toml` (on Linux `~/.config/aispice/pdks.toml`), separate from `config.toml`, because the desktop app rewrites `config.toml` from its in-memory copy and would drop an entry added by the CLI while it is open.

  ```toml
  [pdks.ihp-sg13g2]
  path = "/home/me/.local/share/pdk/IHP-Open-PDK/ihp-sg13g2"
  ```

- Names match `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`, so a name is safe as a folder name in the gm/Id cache.
- CLI: `aispice pdk add <name> <path>` validates and stores the canonical path (a path to the IHP-Open-PDK checkout is accepted and narrowed to its `ihp-sg13g2` folder), `aispice pdk list`, `aispice pdk show <name>`, `aispice pdk remove <name>`.
- Registering is a user action only. The MCP server and the agent can read the registry (the `pdk` tool's `list` and `show`) but cannot add or remove entries, because registering a folder makes aispice load native code from it.
- Validation of the IHP layout (kind `ihp-sg13g2`), relative to the root:
  - required: `libs.tech/ngspice/models/cornerMOSlv.lib`, `cornerMOShv.lib`, `cornerRES.lib`, `cornerCAP.lib`, `cornerDIO.lib`, `cornerHBT.lib`; `libs.tech/ngspice/osdi/psp103.osdi` (with a hint to run IHP's `openvaf-compile-va.sh` when the folder has no `.osdi` files);
  - reported, not required: the other `.osdi` files, `libs.tech/xyce/models/corner*.lib`, `libs.tech/xyce/plugins/*.so`.
- The PDK commit is read from the checkout's git metadata without running git (`.git` folder or file, `HEAD`, loose ref or `packed-refs`). Without git it is `unknown`.
- Types: `aispice_sim::pdk::{Pdk, PdkKind, PdkError, PdkRun}` (layout knowledge, validation, header lines, module lists); `aispice_tools::pdk::{Registry, RegistryEntry, ProjectConfig}` (the file, project opt-in).

### 2. Project opt-in

- A committed file `aispice.toml` at the project root:

  ```toml
  pdk = "ihp-sg13g2"
  ```

- It can only name a registered PDK. It never holds a path, so a cloned project cannot point aispice at code of its choosing. Unknown keys are ignored so later parts can add their own. The file is read on every run (it is small; limit 64 KiB) so edits take effect at once.
- No file, or no `pdk` key: board-level behaviour, unchanged.
- The `pdk` tool's `use` action writes `aispice.toml` through the approval hook, so the agent can opt a project in.

### 3. Trusted OSDI and plugin loading

- `SimJob` gets `pdk: Option<PdkRun>` with `osdi: Vec<PathBuf>` and `xyce_plugins: Vec<PathBuf>`. Only the runner fills it, from the registry, never from deck text.
- Each path passes `aispice_core::netlist::policy::check_module(path, roots, extensions)`: a regular file, canonical path inside a registered PDK root (symlinks resolved first), extension `.osdi` (ngspice) or `.so`/`.dylib`/`.dll` (Xyce), and no quote, comma or control character (the path is written into a start-up file and a comma-separated argument).
- ngspice: the `.spiceinit` aispice already writes into the private run folder (which is `HOME`, so neither the user's nor a project's `.spiceinit` runs) gets one `osdi '<path>'` line per file, and no `ngbehavior` line (fact 3). Nothing about this is in the deck, so the deck policy is unchanged.
- Xyce: `-plugin <a.so>,<b.so>` on the argument vector. A PDK run on Xyce with no plugins in the PDK is refused before Xyce starts, with the build command in the message. The Xyce executable can be configured (`xyce_path` in `config.toml`), because plugin support needs a shared-library build of Xyce.

### 4. Netlist policy

- `Policy` gets `trusted_dirs: Vec<PathBuf>`: the registered PDK roots. They are also allowed include roots.
- Files inside a trusted dir are still checked line by line with every rule (allowlisted directives, no control blocks, no foreign statements, `cmd=`, `file=`-style keys, includes must resolve inside allowed folders), except the heuristic that treats any token with a `/` as a path (fact 5). Outside trusted dirs nothing changes.
- New refusals everywhere: an include whose target is a compiled library (`.osdi`, `.so`, `.dll`, `.dylib`, `.cm`), and a line starting with `osdi`. `pre_osdi`, `codemodel`, `.osdi`, `.control` stay refused, inside the PDK too.
- The runner allows and trusts every registered root, not only the project's PDK, so a `.cir` that includes PDK files directly passes the policy; it then fails in the simulator unless the project opts in, and the runner says so.

### 5. Model header

- Contract with part 2: every corner-capable include appears in the netlist as `.lib <absolute path to cornerXXX.lib> <family>_<corner>`, family in `mos`, `res`, `cap`, `dio`, `hbt`. The path is quoted only when it contains whitespace. Defaults: `mos_tt`, `res_typ`, `cap_typ`, `dio_tt`, `hbt_typ`. Part 2 swaps corners by rewriting the section word and switches to `_mismatch` or `_stat` the same way.
- `Runner::netlist_for` adds the header to the netlist of every circuit in a PDK project, so every flow (simulate, sweep, Monte Carlo, optimize, check_specs, operating point, poles and zeros) gets it and part 2 can rewrite it before the run. Paths point at `libs.tech/ngspice/models`.
- Only the corner libraries the circuit uses are included, found from the subcircuits its X instances (and subcircuit bodies) call, using the device catalog below. Order: MOSlv, MOShv, RES, CAP, DIO, HBT. A library the netlist already includes (same file name inside the PDK) is not added again, so hand-written decks keep their own header.
- For Xyce the runner retargets each header line to `libs.tech/xyce/models/` with the same section, except `cornerHBT.lib`, which stays on the ngspice file (fact 4).
- The model resolver treats PDK device names as defined, so it does not look them up in an LTspice library or report them missing.

### 6. Simulator choice in a PDK project

- `auto` picks ngspice, then Xyce. LTspice and Spectre are refused with a reason (LTspice cannot load OSDI; IHP's open PDK has no Spectre models), and the automatic switch to LTspice when ngspice cannot run a deck is off.
- A netlist that calls PDK devices in a project without `pdk` fails before simulation with the fix in the message.

### 7. Device catalog and symbols

- `aispice_core::pdk` holds a static catalog per kit: subcircuit name, family, corner library, pin order, default parameters (from IHP's xschem `sg13g2_pr` templates), the letter for new instance names, and a description. Lint, the netlister's callers, the runner and gm/Id share it.
- Built-in LTspice symbols (original drawings) under `sg13g2/`:

  | Symbol | Pins (SPICE order) | Default SpiceLine |
  |---|---|---|
  | `sg13_lv_nmos`, `sg13_lv_pmos` | D G S B | `w=0.15u l=0.13u ng=1 m=1` |
  | `sg13_hv_nmos` | D G S B | `w=0.3u l=0.45u ng=1 m=1` |
  | `sg13_hv_pmos` | D G S B | `w=0.3u l=0.4u ng=1 m=1` |
  | `rsil`, `rppd` | P M BN | `w=0.5u l=0.5u b=0 m=1` |
  | `rhigh` | P M BN | `w=0.5u l=0.96u b=0 m=1` |
  | `cap_cmim` | PLUS MINUS | `w=7u l=7u m=1` |
  | `npn13G2` | C B E BN | `Nx=1` |
  | `npn13G2l`, `npn13G2v` | C B E BN | `Nx=1 El=1.0` |

  Each has `Prefix X` and `Value` = the subcircuit name, so the existing netlister writes `XM1 d g s b sg13_lv_nmos w=... l=... ng=1 m=1`, the line xschem writes. The resistor body and the HBT substrate are real pins (xschem hides the body in a `sub!` property), so lint can see them and the user wires them.
- `edit_schematic add_component` names new PDK parts with the catalog letter (`M1`, `R1`, `C1`, `Q1`) instead of `U1`.
- Lint treats catalog subcircuits as defined (no `unknown-subckt`).

### 8. gm/Id

- `aispice_sim::gmid`. One ngspice run per device: one instance per channel length on shared nodes, a nested `.dc` of VGS inside VDS, VBS = 0, saving `ids`, `gm`, `gds`, `cgg`, `vth` per instance (fact 6); fT = gm / (2 pi cgg).
- Default grid, reference width 5 um, `ng=1`:
  - low voltage: L in {0.13, 0.18, 0.25, 0.35, 0.5, 0.75, 1, 1.5, 2, 3, 5, 10} um, VDS 0.1 to 1.2 V by 0.1, VGS 0 to 1.2 V by 10 mV;
  - high voltage: L in {0.45, 0.6, 0.8, 1, 1.5, 2, 3, 5, 10} um, VDS 0.2 to 3.2 V by 0.2, VGS 0 to 3.3 V by 25 mV.
  PMOS tables store magnitudes (VSG, VSD, |Id|).
- Cache: `<cache dir>/aispice/gmid/<pdk name>/<device>.json` with the PDK commit, the grid and a format version; any mismatch rebuilds. `refresh` forces a rebuild.
- Query: target gm/Id, L, VDS (default half the nominal supply), and one of Id or W. Bilinear interpolation over L and VDS; on each curve the VGS where gm/Id crosses the target on the falling (moderate to strong inversion) branch. Current density is interpolated in log. Answer: W, Id, VGS, Vth, gm, gds, intrinsic gain gm/gds, cgg, fT, plus the reference width. Out-of-grid L or VDS, and a gm/Id above the curve's maximum, are errors that state the valid range.
- Surfaces: the `gm_id` tool (MCP and agent) and `aispice gmid`.

### 9. Environment variables for tests

- `AISPICE_PDK_IHP`: path to `ihp-sg13g2`. PDK tests skip (pass, with a note on stderr) without it.
- `AISPICE_XYCE`: optional Xyce executable for the Xyce half of those tests; otherwise `Xyce` on `PATH`. The Xyce half skips when the PDK has no plugins.

## Tests

| What | Where | Needs |
|---|---|---|
| Registry file, names, validation, commit reading | `aispice-sim` and `aispice-tools` unit tests with a fake PDK tree | nothing |
| Policy: `pre_osdi`, `osdi`, `.osdi` include, `codemodel` still refused inside a PDK; modules outside a root, through a symlink, with quotes or a wrong extension refused; PDK includes accepted inside the root only, `..` and symlinks out refused; `/` expressions accepted only in trusted files; `.control` in a trusted file refused | `policy.rs` tests | nothing |
| `.spiceinit` and Xyce argv carry exactly the checked modules; plain runs unchanged | backend unit tests | nothing |
| Header: selection by device, order, no duplicates, Xyce retargeting, section names match the contract | `pdk.rs` tests with a fake tree | nothing |
| Symbols: every IHP symbol resolves, netlists to the xschem line, lints clean, renders | core tests | nothing |
| gm/Id interpolation and the crossing search on synthetic tables | `gmid.rs` tests | nothing |
| Inverter DC transfer and 5T OTA AC, each from an `.asc` drawn with the new symbols, on ngspice and Xyce | `aispice-tools/tests/pdk_ihp.rs` | `AISPICE_PDK_IHP` (Xyce half: plugins and a plugin-capable Xyce) |
| gm/Id table build and a sizing query, checked by simulating the sized device | `aispice-tools/tests/pdk_ihp.rs` | `AISPICE_PDK_IHP` |
| `aispice pdk add/list/show/remove` against a temporary config dir | `aispice-cli/tests` | nothing (validation of a fake tree) |

Tolerances between ngspice and Xyce (same PSP 103.6 Verilog-A, two compilers and two solvers):

- Inverter DC transfer, VDD 1.2 V: |V(out)| difference at most 10 mV at every point of a 10 mV sweep, and switching threshold (V(out) = VDD/2) within 2 mV.
- 5T OTA AC: DC gain within 0.2 dB and unity-gain frequency within 2 %.

These are set from the agreement measured when the tests were written, with margin; the measured numbers are in the plan.

## CI

A Linux job `PDK (IHP SG13G2)` sparse-clones IHP-Open-PDK at the pinned commit (only `ihp-sg13g2/libs.tech/ngspice` and `ihp-sg13g2/libs.tech/verilog-a`), installs ngspice from apt and OpenVAF from its release archive, compiles the OSDI files with IHP's script, sets `AISPICE_PDK_IHP`, and runs the gated tests. Xyce is not built in CI (a plugin-capable Xyce needs Trilinos and a shared build, well over the job's budget), so the Xyce half skips there and runs locally. The job is not a required check; Engine x3, Desktop shell and Interface stay the required ones.

## Out of scope here

Corners, mismatch, noise, PSRR/CMRR (part 2); CMOS templates and IC evals (part 3); xschem import and export (A2); layout (A3); standard cells and IO cells; other PDKs.
