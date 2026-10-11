<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/lockup-dark.svg">
    <img src="assets/brand/lockup.svg" alt="aispice" width="320">
  </picture>
</p>

<p align="center">An AI agent for LTspice circuits: it edits schematics, simulates them,<br>measures the results and sizes parts until your specs pass.</p>

<p align="center">
  <a href="https://github.com/danieltyukov/aispice/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/danieltyukov/aispice/ci.yml?branch=master&label=CI" alt="CI status"></a>
  <a href="https://github.com/danieltyukov/aispice/releases"><img src="https://img.shields.io/github/v/release/danieltyukov/aispice" alt="Latest release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT license"></a>
  <img src="https://img.shields.io/badge/rust-1.88%2B-orange" alt="Rust 1.88 or newer">
</p>

<p align="center">
  <a href="https://danieltyukov.github.io/aispice/">Website</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="docs/SETUP.md">Setup</a> ·
  <a href="docs/TOOLS.md">Tools</a> ·
  <a href="CHANGELOG.md">Changelog</a>
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="app/e2e/screenshots/hero-dark.png">
    <img src="app/e2e/screenshots/hero-light.png" alt="The aispice desktop app: circuits on the left, an RC low-pass schematic with the agent's changes outlined above its Bode plot, and a chat on the right where the agent changed C1, added a load resistor, shows the diff with an Undo button and reports the specs." width="900">
  </picture>
</p>

## What it does

aispice works on the LTspice schematics (`.asc`) in a folder. You describe a change in plain language; the agent reads the circuit, edits it, runs a simulation and checks the result against your specs before it answers. It comes in three forms that share one engine:

- a **desktop app** with the schematic, waveforms with cursors, a spec table, edit history and the chat side by side
- a **command line tool**, `aispice`, for chat, simulation, netlisting, linting and spec checks in CI
- an **MCP server**, `aispice mcp`, that gives Claude Code, Cursor, Codex, Claude Desktop or any other MCP client the same tools

What the agent can do with your circuits:

- read a schematic as parts, the net on every pin and the directives, so it reasons about circuits and not coordinates
- edit with typed operations (add, remove, move, rotate, set values, connect pins, label nets, change directives); aispice places parts and routes wires so the result stays readable in LTspice
- turn a SPICE netlist, or a circuit it reads from a picture, into a readable LTspice schematic, checked to netlist back to the same circuit
- simulate on **ngspice, LTspice, Xyce or Cadence Spectre**, and measure gain, bandwidth, phase margin, overshoot, settling time, rise time, RMS, noise density and integrated noise, PSRR, CMRR and more
- check a spec table and report pass or fail with margins, at one operating point or across process, voltage and temperature corners, naming the worst corner of each spec
- sweep parameters, run Monte Carlo analysis over part tolerances and device mismatch with yield, and optimise part values to meet several specs at once, at every corner if asked
- check the bias point of every transistor (region, gm, gds, gm/Id), find poles and zeros, start from 17 verified reference circuits, lint for wiring mistakes, and render the schematic to SVG or PNG
- design transistor-level IC blocks on a process design kit you have installed (IHP SG13G2 first): draw with the PDK's MOSFETs, resistors, MIM capacitor and HBT, simulate on ngspice or Xyce with the PDK's own models, size transistors by gm/Id from tables aispice builds and caches, and start from templates (current mirror, OTA, two-stage op-amp, comparator, bandgap, LDO) that meet their specs at every process corner
- lay those blocks out with generators that draw the PDK's own pcells (current mirror, differential pair, five-transistor OTA, two-stage Miller op-amp with its MIM capacitor, StrongARM comparator) with common-centroid or interdigitated units, dummies and guard rings; check them with the PDK's KLayout DRC and LVS decks; extract parasitics with Magic; and report every spec before and after layout
- undo any edit; every change is saved as a snapshot

Things you can ask:

- Move the corner of this filter to 1 kHz using E24 values.
- Why does the output clip? Check the bias point.
- Size R1 and R2 for 20 dB of gain and at least 100 kHz of bandwidth.
- Run a Monte Carlo with 1% resistors and 10% capacitors and tell me the yield.
- Check the specs at the ss, tt and ff corners from -40 C to 125 C and tell me the worst corner.
- Add a 100k load at the output and show me what it does to the response.

## Quick start

Install the command line tool (Rust 1.88 or newer) and a simulator:

```sh
cargo install --locked --git https://github.com/danieltyukov/aispice aispice
sudo apt-get install ngspice        # or: brew install ngspice
```

Add a key for your model provider and check the setup:

```sh
aispice keys set anthropic          # paste the key; it goes into the OS keychain
aispice doctor
```

Then ask about a folder of circuits:

```sh
aispice chat --project ~/circuits "Set the corner of rc.asc to 1 kHz and prove it"
```

To use aispice from Claude Code instead:

```sh
claude mcp add aispice -- aispice mcp --project ~/circuits
```

The desktop app is on the [releases page](https://github.com/danieltyukov/aispice/releases) for Linux, Windows and macOS. [docs/SETUP.md](docs/SETUP.md) covers every simulator, provider and MCP client.

## Command line

```console
$ aispice sim rc_lowpass.asc -m "fc = bandwidth_3db(V(out))"
Simulated rc_lowpass.asc with ngspice in 11 ms (run 1791642901500-0).
  note: wrote LTspice's SINE() source function as SIN()
  AC Analysis (Ac): 81 points; vectors: frequency, V(n001), V(out), I(v1)
Measurements:
  fc = 1.591kHz
```

| Command | What it does |
|---|---|
| `aispice chat` | Talk to the agent about a folder of circuits, with one prompt or interactively |
| `aispice sim <file>` | Simulate and print results; `-m` adds measurements, `--simulator` picks one |
| `aispice check <file>` | Check a circuit against its `.specs` file, across corners with `--process`, `--temp` and `--supply`; exits 1 when a spec fails |
| `aispice netlist <file>` | Print the SPICE netlist of a schematic |
| `aispice lint <file>` | Report wiring problems; exits 1 on errors |
| `aispice show <file>` | Describe a schematic: parts, the net on every pin, directives |
| `aispice render <file> -o <out.svg\|png>` | Draw a schematic |
| `aispice draw <netlist> -o <out.asc>` | Lay a SPICE netlist out as a readable LTspice schematic |
| `aispice eval` | Run the design task suite in `evals/` against one or more models and report pass rates |
| `aispice pdk add\|list\|show\|remove` | Register an installed PDK (IHP-Open-PDK's `ihp-sg13g2`), list and describe them |
| `aispice gmid <device>` | Size a PDK transistor by gm/Id, for example `--gm-id 12 --l 0.5u --id 50u` |
| `aispice macro check <dir>` | Check a macro package (a hardened block shared with ChipLeap); exits 1 when it fails |
| `aispice layout generate <circuit> --template ota5t` | Lay a circuit out with the PDK's pcells, then run DRC and LVS; exits 1 when either fails |
| `aispice layout render <gds> -o <out.png>` | Draw a layout |
| `aispice drc <gds>`, `aispice lvs <gds> <circuit>` | The PDK's KLayout rule and LVS decks; exit 1 on violations or a mismatch |
| `aispice extract <gds>` | Parasitic extraction with Magic (`--resistance` for resistor networks) |
| `aispice check <file> --post-layout <gds>` | Specs before and after layout; exits 1 when a spec fails after layout |
| `aispice mcp` | Serve the tools over MCP on stdio |
| `aispice doctor` | Show simulators, LTspice's library, keys and the config file |
| `aispice keys`, `aispice models` | Manage provider keys, list a provider's models |

A spec table is a plain text file next to the circuit, one spec per line:

```text
# amp.specs
gain = gain_db_at(V(out)/V(in), 1k) >= 20
bw   = bandwidth_3db(V(out)) >= 100k
pm   = phase_margin(V(out)) >= 45
```

`aispice check amp.asc` runs the simulation and fails the build when a spec does not pass, so circuits can be tested in CI like code. `aispice check amp.asc --process tt,ss,ff --temp=-40,27,125 --supply VDD=1.62,1.98` checks every corner and names the worst one for each spec.

## IC design on a PDK

aispice uses a PDK where you installed it and never ships one. Register it once, then name it in the project's `aispice.toml`:

```sh
aispice pdk add ihp-sg13g2 ~/IHP-Open-PDK/ihp-sg13g2
echo 'pdk = "ihp-sg13g2"' > ~/my-ota/aispice.toml
aispice gmid sg13_lv_nmos --gm-id 12 --l 0.5u --id 50u
```

Every netlist in that project then carries the PDK's model header (`.lib .../cornerMOSlv.lib mos_tt` and the like), and simulations load the PDK's compiled device models on ngspice or Xyce. [docs/SETUP.md](docs/SETUP.md#pdks-for-ic-design) covers building the models and what aispice checks.

Six IC templates start a block from a circuit known to work: an NMOS current mirror, a five-transistor OTA, a two-stage Miller op-amp, a StrongARM comparator, a bandgap reference and an LDO on IHP SG13G2. Each meets its spec table at every process corner (tt, ss, ff, sf, fs) and, where it matters, from -40 C to 125 C or across the supply, on ngspice and Xyce, and names the layout generator and role map for its block.

With KLayout and Magic installed, the same project goes on to layout:

```console
$ aispice layout generate ota.asc --template ota5t
Wrote ota.gds and ota.layout.json.
ota5t layout of ota (6 devices, 6 dummies), 14.46 x 26.02 um.
  row tail: nmos fingers 2.0 um wide, L 1.0 um; order d M5 M6 d
  row pair: nmos fingers 1.0 um wide, L 0.5 um; order d M1 M2 d
  row load: pmos fingers 2.0 um wide, L 0.5 um; order d M3 M4 d
  pins: 0, inm, inp, out, vbn, vdd
DRC clean: cell ota, decks main, extra, antenna.
LVS clean: the layout of ota matches the schematic.
$ aispice check ota.asc --post-layout ota.gds
...
  gain: 25.06 dB pass -> 25.37 dB pass (+1.2%)
  gbw: 113.7 MHz pass -> 105.2 MHz pass (-7.5%)
  pm: 84.49 ° pass -> 82.54 ° pass (-2.3%)
```

Layouts come only from generators: when DRC or LVS fails, the agent changes the generator's parameters or role map and draws again, and never edits GDS. [docs/SETUP.md](docs/SETUP.md#layout) lists what layout needs.

## Blocks for mixed-signal chips

Mixed-signal chips need blocks from both sides. aispice and its digital counterpart, [ChipLeap](https://github.com/danieltyukov/chipleap), hand hardened blocks to each other as macro packages: GDS, LEF, liberty, a Verilog model, a SPICE netlist, measured specs and DRC and LVS evidence, described by one `macro.toml`. Both tools check a package with the same rules before placing it:

```sh
aispice macro check tiny_counter.macro --pdk ihp-sg13g2
```

The `check_macro` tool does the same for the agent and MCP clients. [docs/macro-package.md](docs/macro-package.md) is the format, shared word for word with ChipLeap.

## Simulators and models

| Simulator | Notes |
|---|---|
| ngspice | Recommended default. Runs in LTspice compatibility mode |
| LTspice | Native on Windows and macOS, under Wine on Linux, headless with `xvfb-run` |
| Xyce | When `Xyce` is on `PATH`, or set by `xyce_path`. PDK models need a Xyce built with plugin support |
| Spectre | On a remote licensed server over SSH, configured by you |

| Layout tool | Notes |
|---|---|
| KLayout | 0.30 or newer, for DRC, LVS, drawing layouts with the PDK's pcells and pictures. Runs in batch mode only |
| Magic | 8.3.573 or newer, for parasitic extraction with the PDK's tech file. Runs without graphics |

| Provider | Notes |
|---|---|
| Anthropic | Claude models through the Messages API |
| OpenAI | GPT models |
| Google | Gemini models |
| OpenRouter | Any model OpenRouter serves |
| Ollama | Local models, no key |
| Custom | Any OpenAI-compatible endpoint (LM Studio, vLLM, a company gateway) |

## Security

aispice treats circuit files, simulator logs and model output as data. Tools only read and write inside the project folder. Every process runs from an argument vector, never a shell string, and every netlist is checked against an allowlist before a simulator sees it, so a schematic cannot run commands or read files outside the project. A netlist can never load compiled models; aispice loads those itself, only from a PDK folder you registered. KLayout and Magic run only aispice's own scripts, with names and paths passed as checked data, and the GDS and report readers are bounded against hostile files. In "ask before applying" mode every write waits for your approval. Keys stay in the OS keychain. [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#security-model) has the details, and [SECURITY.md](SECURITY.md) explains how to report a problem.

## Development

```sh
git clone https://github.com/danieltyukov/aispice
cd aispice
npm ci
npm run check        # Rust fmt, clippy and tests; interface typecheck, lint and tests
npm run desktop      # the desktop app with hot reload
```

[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) explains how the crates fit together and [CONTRIBUTING.md](CONTRIBUTING.md) how to send a change.

## License

[MIT](LICENSE)
