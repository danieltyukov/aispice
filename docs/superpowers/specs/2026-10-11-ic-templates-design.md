# IC templates, IC evals and Spectre end to end (A1 part 3): design

Date: 2026-10-11. Status: accepted for implementation. Roadmap milestone A1, part 3. Builds on the PDK foundation (#36: registry, trusted OSDI, IHP device symbols, gm/Id), corner sets, noise and mismatch (#34) and the analog layout generators (#38).

## Goal

A designer, or the agent, starts an IC block from a template that is known to work on IHP SG13G2 at every corner of a stated set, sizes it further with the gm/Id tool and corner-aware `optimize`, and hands it straight to the layout generator for its block. The eval suite gains IC tasks that only a correctly sized circuit passes. Spectre, the one simulator whose end-to-end path was still untested, is run against two real releases.

## Facts this design rests on

Measured on 2026-10-11 with IHP-Open-PDK `22f2a25`, ngspice 42 and the plugin-capable Xyce 7.10.

1. The process presets of #34 move every device family: `ss` is `mos_ss` with resistors, capacitors and HBTs at `wcs`, `ff` is `mos_ff` with `bcs`, and the skewed corners leave the passives typical. A template's corner set therefore covers its resistors and HBTs too.
2. IHP's MOS models are valid up to 10 um per finger, and current density changes with finger width (5.1 A/m at 2 um, 10.2 A/m at 30 um for the NMOS at a fixed VGS). Template devices stay under 10 um per finger.
3. The low-voltage NMOS has a low intrinsic gain (gm/gds about 37 at L = 2 um and gm/Id = 15 S/A, from the gm/Id tool), so a five-transistor OTA reaches about 27 dB and a two-stage op-amp about 53 dB at 1.2 V.
4. Xyce's IHP model folder has no HBT model (`sg13g2_hbt_mod.lib` is empty), so a circuit with an `npn13G2` runs on ngspice only.
5. The layout generators of #38 take a role map with fixed role names per block (`current_mirror`: ref, out; `ota5t`: pair, load, tail, bias; `two_stage`: those plus cs, sink, cc; `comparator`: tail, pair, latch_n, latch_p, reset) and draw only the low-voltage MOS devices and `cap_cmim`. `generate` plans a block from a netlist without running KLayout, so a template's role map can be checked without the PDK.
6. A check at 15 corners of an AC run takes about one second on ngspice; the LDO's 60 us transient at 45 corners about five seconds.

## Decisions

### Template metadata

An IC template is a template in category `ic` with four more fields in its TOML:

- `pdk`: the kind of PDK the circuit is drawn in (`ihp-sg13g2`), which must be a kit aispice knows.
- `corners`: the corner set its spec table holds at, in the same form `check_specs` takes. Process presets at the template's nominal temperature at least; temperatures and supply values where they matter to the block.
- `simulators`: the simulators the specs are verified on (`ngspice`, `xyce`); a template with an HBT lists ngspice only (fact 4).
- `layout` (optional): `generator` and `roles`, deserialized into the layout crate's own `Template` and `Roles` types, so a misspelt generator fails to load.

`templates show` and `templates use` print the PDK, the corner set as JSON ready to pass to `check_specs`, `optimize` and `monte_carlo`, and the layout generator with its role map. `templates use` refuses an IC template, before anything is written, unless the open project uses a registered PDK of that kind, and names the registered PDKs it could switch to.

### The templates

| id | block | analysis | corners | layout |
|---|---|---|---|---|
| `current_mirror` | NMOS mirror, 10 uA, L = 4 um | `.dc` of the output voltage | 5 process x 3 temperatures | `current_mirror` |
| `ota5t` | NMOS-input 5T OTA, 20 uA, 1 pF | open-loop `.ac` | 5 x 3 | `ota5t` |
| `two_stage` | Miller op-amp, MIM compensation, 1 pF | open-loop `.ac`, DC loop closed through a large inductor | 5 x 3 | `two_stage` |
| `comparator` | StrongARM latch, 500 MHz clock | `.tran` with two decisions | 5 x 3 | `comparator` |
| `bandgap` | current-mode (Banba) reference, 0.6 V | `.dc temp` sweep | 5 process x 3 supplies | none (HBTs) |
| `ldo` | 3.3 V to 1.2 V on thick-oxide devices, 10 mA step | `.tran` | 5 x 3 x 3 input voltages | none (HV devices) |

Each spec limit leaves room at its worst corner: a one-sided spec passes by at least 5% of its limit, a two-sided one stays inside the middle 70% of its window (the rule the existing template test uses). Sizing starts from gm/Id tables (the `gm_id` tool) and is finished with `check_specs` and `optimize` over the corner set; the design equations go into each template's parameters.

### Tests

- Unit tests (no PDK): every IC template names a known kit, a corner set that expands, known simulators, parameters that name real parts, and, with a layout hint, a role map that the generator's planner accepts on the template's own netlist. Non-IC templates carry none of these fields.
- `templates_ihp.rs`, gated on `AISPICE_PDK_IHP`: each IC template is copied into a project that uses the PDK through `templates use`, then checked with the `check_specs` tool at its corner set on ngspice, and on Xyce where it lists Xyce and a plugin-capable Xyce is found. Every spec must pass at every corner with the margin above. `templates use` without the PDK is refused and writes nothing.
- The existing all-templates simulation test skips IC templates; they need the PDK.
- CI's PDK job runs `templates_ihp`.

### IC evals

- A task may name `pdk = "ihp-sg13g2"`. `EvalOptions` carries the PDK registry (the CLI passes the user's). A task whose PDK kind is not registered is skipped with the reason, not failed. Otherwise the runner writes `aispice.toml` naming the registered PDK before the run starts, and both the agent's runner and the judge's have the registry.
- Tasks: `ic-opamp-corners` (size the two-stage op-amp for 50 dB, 20 MHz and 60 degrees at all 15 corners), `ic-comparator-ss` (a comparator that decides in time at tt but not at ss), `ic-opamp-noise` (report the input-referred noise of the two-stage at 1 kHz without changing it). Reference solutions are checked by `eval_suite.rs` when `AISPICE_PDK_IHP` is set.

### Spectre end to end

- Configuration stays outside the repository: `AISPICE_SPECTRE_HOST`, `AISPICE_SPECTRE_DIR` and `AISPICE_SPECTRE_SETUP` for the tests, and the user's own `config.toml` for the CLI. No host names, paths or results from the server are committed.
- The dialect tests and the noise, PSRR and corner features run against two Spectre releases with generic models. Failures are fixed in the backend or the dialect translation; differences between the releases are noted in the PR in general terms.

## Out of scope

- IC templates on Spectre: IHP ships no Spectre models.
- Layout generators for the bandgap and LDO (HBTs, resistors and thick-oxide devices).

## As built

Status 2026-10-11: implemented as designed, with these results and changes.

Worst case of each spec over the template's corner set (15 corners; 45 for the LDO), the same on ngspice 42 and Xyce 7.10 to the digits shown except where noted:

| Template | Worst case | Limit |
|---|---|---|
| `current_mirror` | iout 10.42 uA (ff, -40 C); ilow 9.21 uA (ss, 125 C); flat 1.65 uA (ff, -40 C) | 9 to 11.5 uA; 8.5 uA; 2 uA |
| `ota5t` | 25.9 dB (fs, 125 C); 16.9 MHz (ss, 125 C); 83 degrees (ff, -40 C) | 24 dB; 15 MHz; 70 degrees |
| `two_stage` | 51.1 dB (fs, 125 C); 15.2 MHz (ss, 125 C); 57 degrees (ff, -40 C) | 48 dB; 13 MHz; 52 degrees |
| `comparator` | 194 ps on ngspice, 195 ps on Xyce (ss, -40 C); full swing and reset everywhere | 250 ps |
| `bandgap` (ngspice) | 601.7 mV (ff, 1.08 V); 3.8 mV drift (ss, 1.08 V) | 580 to 620 mV; 6 mV |
| `ldo` | 1.202 V light, 1.192 V heavy, 1.187 V minimum, 1.203 V peak, 155 uV ripple on ngspice and 280 uV on Xyce | 1.17 to 1.23 V; 1.12 V; 1.27 V; 2 mV |

- The OTA's input pair was one 11 um finger, past the models' 10 um; it is two fingers now, which moved no spec by more than 0.2 dB or 0.2 MHz. The gm/Id tool gives W = 10.7 um (two fingers) for gm/Id = 15 S/A at L = 2 um and 10 uA, the size the template uses.
- The two-stage op-amp spreads its unity-gain frequency by two to one over the corners (15 MHz at ss and 125 C, 31 MHz at ff and -40 C), mostly through temperature, so meeting 20 MHz and 60 degrees everywhere is the hard task `ic-opamp-corners`: a 216-point sweep over bias current, pair, mirror, second stage and Miller capacitor found 13 sizings that pass, and the reference solution (IB 18 uA, mirror 2/0.5 um, M6 eight fingers, M7 four) has 52.8 dB, 21.3 MHz and 66 degrees at its worst corners.
- Spectre end to end, with generic models only, on Spectre 19.1, 23.1 and 25.1: the dialect, noise, PSRR, temperature and process corner tests pass on all three, and the three releases give identical numbers. Three faults were found and fixed: a circuit that includes a file failed because the server has none of the project's files (the project files a deck includes are now inlined for Spectre; after a security review, nothing outside the project is: a PDK or LTspice library file names the server's copy from `remote_folders` in the user's config, or the run is refused by file name, LTspice library models are not added, and the backend refuses a deck that names a local file or exceeds 16 MiB); Spectre's SPICE reader refuses `{expression}` values on every release (now written in single quotes); and `.meas` results, which every release writes to a `.measure` file rather than the log, were never read (now fetched and parsed, which also corrects the documented claim that Spectre 19 ignores `.meas` in SPICE mode). Spectre 24.1 and 25.1 need glibc 2.22 or newer and do not start on CentOS 7.
