# Changelog

All notable changes to aispice are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- PDK registry: `aispice pdk add <name> <path>` checks and registers an installed PDK (IHP SG13G2 from IHP-Open-PDK first) in `pdks.toml` beside `config.toml`; `list`, `show` and `remove` manage it, and `aispice doctor` lists it. Only the user registers a PDK; the agent and MCP clients can read the registry but not change it.
- A project uses a registered PDK with `pdk = "<name>"` in `aispice.toml` at its root. Its netlists then end with the PDK's model header, one `.lib <corner library> <section>` line per device class the circuit uses (`mos_tt`, `res_typ`, `cap_typ`, `dio_tt`, `hbt_typ`), and runs load the PDK's compiled device models: OSDI files through the `.spiceinit` aispice writes for ngspice (which then runs in its native mode), plugins through `-plugin` for Xyce. LTspice and Spectre are refused for PDK projects. Board-level projects are unchanged.
- IHP SG13G2 device symbols (`sg13g2\sg13_lv_nmos`, `sg13_lv_pmos`, `sg13_hv_nmos`, `sg13_hv_pmos`, `rsil`, `rppd`, `rhigh`, `cap_cmim`, `npn13G2`, `npn13G2l`, `npn13G2v`) that netlist as the PDK's subcircuits with IHP's default sizes, lint, render and take edits; new instances are named M1, R1, C1, Q1.
- `pdk` tool: list registered PDKs, describe one (compiled models, corner libraries and their sections, the header aispice adds), list its devices, and make the open project use one.
- `gm_id` tool and `aispice gmid`: size a PDK transistor by gm/Id from a lookup table built on ngspice over L, VDS and VGS and cached under the user cache folder by PDK commit, then checked and corrected by simulating the device at the size found.
- `xyce_path` setting for a Xyce built with plugin support.
- Noise analysis (`.noise`) on ngspice, Xyce, LTspice and Spectre, read the same way from each as `V(onoise)` and `V(inoise)`, with the measurements `output_noise_at`, `input_noise_at`, `output_noise_rms` and `input_noise_rms`.
- `psrr_db_at` and `cmrr_db_at` measurements: aispice adds a second AC run with the stimulus moved to the named supply or common-mode source and compares it with the circuit's own.
- Corner sets for `check_specs`, `optimize` and `monte_carlo`: process corners (presets tt, ss, ff, sf, fs or family maps, applied to `.lib <file> <family>_<corner>` lines), temperatures and supply values. Reports give each spec's worst value, margin and corner and the full table; `optimize` minimises the worst case. `aispice check` takes `--process`, `--temp` and `--supply`.
- Mismatch Monte Carlo: `monte_carlo` with `mismatch` set to `local` or `global` switches corner libraries to their `_mismatch` or `_stat` sections, with a simulator seed per run, on ngspice and Xyce, alone or with part tolerances and corners.
- Design tasks `noise-tia` and `temp-corners`; task files take a `corners` set the judge checks every spec at.
- Macro packages (`macro/v1`), the format aispice and ChipLeap use to hand hardened blocks to each other: `macro.toml` plus GDS, LEF, liberty, Verilog model, SPICE netlist, a measured spec table for analog blocks and DRC and LVS evidence. `aispice macro check DIR [--pdk NAME[@COMMIT]]` and the `check_macro` tool check a package with the same rules, rule ids and messages as `chipleap macro check`: the manifest against its JSON schema (`aispice macro schema`), the files each kind needs, every pin across the GDS, LEF, liberty, Verilog and SPICE views, the size, timing arcs of digital outputs, measured specs against their limits, and evidence that passes, matches the package files and names the same PDK. Evidence follows OpenADA's result envelope, and OpenADA results are accepted as they are. `aispice_core::macropkg` reads and writes packages for later steps. The spec is [docs/macro-package.md](docs/macro-package.md); fixtures shared with ChipLeap pin both validators' findings.
- Analog layout on IHP SG13G2 (new crate `aispice-layout`). The `layout` tool and `aispice layout generate` lay a circuit's transistors out with a generator (`current_mirror`, `diff_pair`, `ota5t`, `two_stage` for a Miller op-amp with its MIM capacitor, `comparator` for a StrongARM latch) drawn with IHP's own pcells in KLayout: matched devices in common-centroid or interdigitated two-finger units, dummy fingers, guard rings, Metal3 tracks and Metal4 spines, labelled pins. Roles come from the topology or a role map; the generator checks the topology and says what differs. It writes `<cell>.gds` and `<cell>.layout.json` and runs DRC and LVS, drawing again with a changed parameter when a failed DRC rule points to one. `layout render` and `aispice layout render` draw a layout as a PNG.
- `drc` tool and `aispice drc`: IHP's KLayout decks (main rules, extra rules, antenna; density on request), each violation with its rule, layer, description and box in um.
- `lvs` tool and `aispice lvs`: IHP's KLayout LVS deck against a CDL netlist written from the circuit's PDK devices (only the devices a generated layout drew, plus its dummies), reporting each net, device and pin that does not match by its schematic and layout names.
- `extract` tool and `aispice extract`: Magic with IHP's tech file, devices and coupling capacitance, and with `resistance` a resistor network on every net.
- `check_specs` takes `post_layout` (and `aispice check --post-layout <gds>`): the extracted layout replaces the laid-out devices, joined by net name, and each spec is reported before and after layout.
- `klayout_path` and `magic_path` settings; `aispice doctor` shows KLayout and Magic.

### Security

- `aispice macro check` and the `check_macro` tool read a package from someone else within limits that no real macro comes near ([docs/macro-package.md](docs/macro-package.md), "Limits"), the same as ChipLeap's. A file over its size limit is not read; nesting in TOML, JSON and liberty, pins, tokens, GDS records, elements, cells and reference depth have caps; a GDS cell that contains itself is an error; an array reference is measured from its corner instances, so a 32767 x 32767 array costs no more than one instance; and a report lists at most 1,000 findings and counts the rest. Each case ends in a finding with the same rule id in both tools (`file.too_large`, `file.unreadable`, `gds.cycle`, `check.limit` and a `.limit` rule per file), never in a hang, a stack overflow or a machine out of memory. No reader panics on malformed input, and a seeded random-bytes test per format checks it. TOML that only TOML 1.1 allows is refused, as the spec's TOML 1.0 and Python's `tomllib` require.
- A PDK folder is checked again, symlinks resolved, every time it is used, and a filesystem root, the home folder or a folder above it cannot be registered. A registered folder replaced by a link afterwards is no longer trusted.
- KLayout and Magic run only aispice's own scripts, from an argument vector in a private folder that is also their `HOME` and `TMPDIR` (IHP's pcell library writes and deletes fixed-name files in the temporary folder, which a shared `/tmp` would let another run or user interfere with); paths and names reach them as checked data, never as script text. GDS files, KLayout report databases, LVS results and Magic netlists are read with size and nesting limits and are tested against random and mutated input.
- The netlist policy trusts registered PDK folders as include roots, still checking their files with every rule except the path heuristic their expressions trip (`w/ng`). Includes of compiled libraries (`.osdi`, `.so`, `.dll`, `.dylib`, `.cm`) and `osdi` lines are refused; `pre_osdi`, `codemodel` and `.control` stay refused. Compiled models load only from inside a registered PDK, after a check that rejects paths through symlinks or with quotes, commas or `$`.
- The netlist policy read some include lines differently from the simulators, which could make ngspice or Xyce open a file outside the project that the policy never checked: a `;`, `$` or `//` comment glued to a file name, single quotes or braces around it, an indented include, a `.include` on the title line (ngspice reads it), and `+` continuation lines that Xyce joins across comment, indented and `;` lines. The policy now checks a deck as ngspice and as Xyce join its lines, accepts include lines in one form only (the keyword, the file bare or in double quotes, for `.lib` at most a section name, nothing after it, on one line, no space in a `.lib` path) and refuses lone carriage returns, Unicode line separators and ngspice's `\\` line joining. The deck's own include lines are rewritten to name the checked file by its full path. A test runs every spelling through ngspice and Xyce against a decoy file outside the project.
- Corner, mismatch and section-inlining code read library files only after the policy approved that exact path (regular files, 16 MiB each, 32 files and 64 MiB per call), with the policy's own parser, and list section names only for libraries in the project. Model resolution no longer reads a file an include names before the policy has approved it.
- `aispice.toml` is read only when it is a regular file at the project root, never through a link and at most 64 KiB. A project could ship it as a link to a secret elsewhere, and its TOML error, quoting the line it failed on, reached the model and MCP clients through every tool that builds a netlist; a FIFO there would hang the read. Errors in `aispice.toml` and `pdks.toml` now give the line and column only, and the `pdk` tool's `use` action does not write through a link.

### Fixed

- An `.include` of a file beside the circuit was dropped before ngspice and Xyce ran.
- Xyce ignored `.temp`; it is now written as Xyce's device temperature option.
- ngspice could not read library sections (`.lib <file> <section>`) in the LTspice mode aispice runs it in; aispice now inlines the section for ngspice and LTspice.
- AC measurements could read a noise analysis, which also has a frequency axis.
- The netlist policy read a `+` continuation after a CRLF line end as a separate line instead of part of the include before it.
- A Monte Carlo analysis whose every run failed reported a yield over no runs; it now reports the first failure as an error. Runs or optimizer candidates times corners over the limit are refused with the largest number that fits, rather than cut silently.

## [0.2.0]

A rewrite. aispice is now one Rust engine with three front ends: a desktop app, a command line tool and an MCP server.

### Added

- `aispice` command line tool: `chat`, `sim`, `check`, `netlist`, `lint`, `show`, `render`, `draw`, `eval`, `mcp`, `doctor`, `keys`, `models` and `docs`.
- MCP server (`aispice mcp`) exposing every tool to Claude Code, Cursor, Codex, Claude Desktop and other clients.
- Lossless `.asc` and `.asy` reading and writing in every encoding LTspice uses.
- A netlister that follows LTspice's own, checked against LTspice's `-netlist` output.
- Typed schematic edits with automatic placement and wire routing, a semantic diff after each edit, and undo through snapshots.
- Netlist to schematic: `create_schematic` and `aispice draw` lay a SPICE netlist out as a readable LTspice schematic (signal flow left to right, ground down, rails up, feedback above its stage) and refuse any drawing that does not netlist back to the input.
- `poles_zeros` tool: pole-zero analysis of a transfer function on ngspice's `.pz`, with f0 and Q for each complex pair, the corner of each real root, and a stability verdict.
- `operating_point` tool: the DC operating point of every MOSFET, BJT and diode on ngspice, subcircuit devices included, with region, gm, gds, gm/Id, intrinsic gain, rpi, ro and beta, and a note on devices out of saturation that look like they should be in it, such as a current-mirror output.
- `templates` tool with 17 verified reference circuits (filters, amplifiers, references, power, oscillators, digital, sensors), each a readable LTspice schematic with design equations and a spec table that passes on ngspice.
- `aispice eval` and a 13-task design suite in `evals/`, judged by simulation, reporting pass rate, steps, tokens and time per model.
- Desktop app: import a SPICE netlist as a drawn schematic.
- Linux launcher integration: the dock and app grid show the aispice icon on X11 and Wayland, from the packages and from an AppImage.
- Simulation on ngspice, Xyce, LTspice (native or under Wine, headless with `xvfb-run`) and Cadence Spectre over SSH.
- Measurements (gain, bandwidth, phase and gain margin, overshoot, settling, rise and fall time, RMS and more), spec tables with margins, parameter sweeps, Monte Carlo with yield, and an optimizer.
- `peaking_db` and `q_lowpass` measurements, so a filter's shape (a Butterworth claim) can be checked as well as its corner.
- Lint rule `unknown-subckt`: a part calls a subcircuit that nothing defines, such as an op-amp whose value was replaced by a parameter. A library aispice cannot read suppresses it.
- `embedded_models_only` setting: simulate with aispice's embedded models and the project's own files only, never with LTspice's library. `aispice eval` always runs this way, so results compare across machines.
- Edits that cannot do what they were asked fail as a whole with nothing saved, naming the edit and its op; schema mistakes name the edit and the fields its op takes, and common aliases (`to` for `net` in connect_to_net, and others listed in the schema) are accepted.
- `disconnect` and `remove` take along the stubs and labels that served only that pin or part, and `disconnect` keeps the other pins on their net; `connect_to_net` refuses a pin reference as a net name and refuses to short two named nets.
- `read_schematic` shows an op-amp's subcircuit name as its value and its SpiceLine parameters apart, and `set_value` refuses parameters on a subcircuit call, pointing to `set_attr`.
- `measure`, `plot` and `read_waveform` simulate again, with the same analysis, when the circuit has changed since the run they would read.
- Schematic diffs report only the connections that changed.
- `aispice check` for testing circuits against their specs in CI.
- Providers: Anthropic, OpenAI, Google Gemini, OpenRouter, Ollama and any OpenAI-compatible endpoint.
- Lint rules, SVG and PNG rendering, and plots.
- Desktop app rebuilt: schematic view, waveforms with cursors, netlist, log, specs and history tabs, and a chat that shows each tool call as a card with its diff and an Undo button. Optional approval before each edit.
- New name style (`aispice`), logo, website and documentation.

### Changed

- The desktop app's backend now calls the shared engine instead of its own copy of the format, AI and simulation code.
- API keys move from the plain config file to the OS keychain. An existing `config.json` with keys is migrated on first use and the keys are removed from it.
- Licensed under MIT.

### Security

- Every tool is confined to the project folder, including through symbolic links.
- Netlists and everything they include are checked against an allowlist before any simulator runs them.
- Every process is started with an argument vector; there are no shell strings.
- In ask mode, every write (edits, spec files, optimizer results, undo) waits for approval.
- Strict content security policy and minimal capabilities in the desktop app; SVG output is sanitised before display.
- Chat sessions are kept in the user's data folder, never in a project, so a cloned repository cannot plant a conversation.

## [0.1.0]

The first version: a Tauri desktop chat beside LTspice that edited `.asc` files and triggered LTspice to reload.

[Unreleased]: https://github.com/danieltyukov/aispice/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/danieltyukov/aispice/releases/tag/v0.2.0
[0.1.0]: https://github.com/danieltyukov/aispice/tree/808c0de
