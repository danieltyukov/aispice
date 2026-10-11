# Setup

This page covers installing aispice, connecting simulators and model providers, the config file, and using aispice from an MCP client. Run `aispice doctor` at any point to see what aispice found.

## Install

**Desktop app.** Download the installer for your platform from the [releases page](https://github.com/danieltyukov/aispice/releases): `aispice_amd64.deb` or `aispice_x86_64.AppImage` on Linux, `aispice_x64-setup.exe` or `aispice_x64.msi` on Windows, `aispice_universal.dmg` on macOS. The desktop app includes the same agent and tools as the command line.

**Command line.** Prebuilt archives are on the same page (`aispice-cli-x86_64-linux.tar.gz`, `aispice-cli-x86_64-windows.zip`, `aispice-cli-universal-macos.tar.gz`). To build from source instead, with Rust 1.88 or newer:

```sh
cargo install --locked --git https://github.com/danieltyukov/aispice aispice
```

**From a clone**, for development:

```sh
git clone https://github.com/danieltyukov/aispice
cd aispice
cargo build -p aispice          # the CLI, at target/debug/aispice
npm ci                          # the desktop interface and the site
npm run desktop                 # the desktop app, with hot reload
```

The desktop app on Linux needs the WebKitGTK development packages. On Debian and Ubuntu:

```sh
sudo apt-get install libwebkit2gtk-4.1-dev libxdo-dev libssl-dev \
  libayatana-appindicator3-dev librsvg2-dev
```

## Simulators

aispice needs at least one simulator. With `simulator = "auto"` (the default) it prefers ngspice, then LTspice, then Xyce. Every tool and command also takes an explicit simulator.

### ngspice

The recommended default: free, fast, and on every platform.

| Platform | Install |
|---|---|
| Debian, Ubuntu | `sudo apt-get install ngspice` |
| Fedora | `sudo dnf install ngspice` |
| macOS | `brew install ngspice` |
| Windows | Download from [ngspice.sourceforge.io](https://ngspice.sourceforge.io/download.html), unpack, and put the folder with `ngspice_con.exe` on `PATH` |

aispice runs ngspice with `ngbehavior=ltpsa`, so LTspice-style expressions, behavioural sources and model cards work. Each run happens in a private temporary folder.

### LTspice

aispice looks in the standard install locations:

| Platform | Locations checked |
|---|---|
| Windows | `%LOCALAPPDATA%\Programs\ADI\LTspice\LTspice.exe`, `C:\Program Files\ADI\LTspice\LTspice.exe`, `C:\Program Files\LTC\LTspiceXVII\XVIIx64.exe` |
| macOS | `/Applications/LTspice.app` |
| Linux | LTspice installed under Wine, in `$WINEPREFIX` or `~/.wine` |

For anything else, set `ltspice_path` in the config file. On Linux, aispice runs LTspice through `wine`, and through `xvfb-run` when there is no display, so batch simulation works on a headless server.

When LTspice is installed, aispice also reads its symbol library, so schematics that use LTspice's own parts (op-amps, regulators, comparators) netlist correctly. Point `AISPICE_LTSPICE_LIB` at a `lib/sym` folder to use a different library.

After an edit, the desktop app opens the schematic in LTspice again, which brings LTspice forward with the saved version, so you can keep LTspice open next to aispice. Turn this off in Settings.

### Xyce

Install Xyce from [xyce.sandia.gov](https://xyce.sandia.gov/) and make sure `Xyce` is on `PATH`, or set `xyce_path` in the config file. aispice translates LTspice-specific syntax where Xyce needs it. Simulating a PDK on Xyce needs a Xyce built with plugin support (see [PDKs for IC design](#pdks-for-ic-design)).

### Spectre

Cadence Spectre usually lives on a licensed server, so aispice runs it over SSH: it copies the netlist to the server with `scp`, runs Spectre with `ssh`, and copies the results back. Add this to the config file:

```toml
[spectre]
ssh_host = "user@cadence.example.edu"
remote_dir = "/home/user/aispice-runs"
# Optional: a command that sets up the Cadence environment first.
setup_command = "source /opt/cadence/setup.sh"
```

SSH runs with `BatchMode=yes`, so the host must accept your SSH key without a password prompt. Check with `ssh user@cadence.example.edu true`. aispice creates a private folder under `remote_dir` for every run.

These settings come only from your config file. A model or a netlist can never change the host or the command. Keep process design kits and anything under an NDA out of your project folders if you share them.

Spectre 19 and older do not run `.meas` statements in SPICE syntax. aispice's own measurements, spec checks, sweeps and the optimizer work on Spectre's waveforms the same as on any other simulator, so use those. Tested with Spectre 19.1 on a remote CentOS 7 server, through an SSH jump host configured in `~/.ssh/config`.

## PDKs for IC design

aispice designs transistor-level circuits on a process design kit you install. It never ships or copies a PDK: it reads the models where they are. IHP SG13G2 (130 nm SiGe BiCMOS, from [IHP-Open-PDK](https://github.com/IHP-GmbH/IHP-Open-PDK)) is the first one it knows. PDK projects run on Linux and macOS; on Windows the netlist policy refuses the absolute paths a PDK's model header needs.

### Install IHP SG13G2

```sh
git clone https://github.com/IHP-GmbH/IHP-Open-PDK ~/IHP-Open-PDK
cd ~/IHP-Open-PDK/ihp-sg13g2/libs.tech/verilog-a
./openvaf-compile-va.sh          # needs OpenVAF 23.5 or OpenVAF-Reloaded on PATH
```

The last step compiles the PDK's Verilog-A transistor models (PSP 103) into OSDI files in `libs.tech/ngspice/osdi`, which ngspice 42 or newer loads. They are not in the PDK's repository. aispice's tests run against commit `22f2a25f1734796de3debbbf29cf697cbbc54081`.

For Xyce, the same models must be built as Xyce plugins into `libs.tech/xyce/plugins`, with IHP's `adms-compile-va.sh` (or `buildxyceplugin.sh` from your Xyce installation, run in a copy of each model's folder). That needs ADMS and a Xyce built with shared libraries and plugin support (`-DBUILD_SHARED_LIBS=ON -DXyce_PLUGIN_SUPPORT=ON`); a statically linked Xyce cannot load plugins. Point `xyce_path` at that Xyce. Without plugins, PDK circuits still run on ngspice.

### Register it

```sh
aispice pdk add ihp-sg13g2 ~/IHP-Open-PDK/ihp-sg13g2
aispice pdk show ihp-sg13g2      # compiled models, corner libraries, devices
```

`add` checks the folder (every corner library, and at least `psp103.osdi`), resolves symlinks, and records it in `pdks.toml` beside `config.toml`. The path to the checkout itself works too. `aispice pdk list` and `aispice pdk remove` do the rest, and `aispice doctor` shows what is registered. Set `AISPICE_PDK_REGISTRY` to use another registry file.

Only you register a PDK. A registered folder is trusted to supply compiled models that run inside the simulator, so neither a project nor the agent can add one: the `pdk` tool reads the registry and can opt a project in, nothing more.

### Use it in a project

Put an `aispice.toml` at the project's root:

```toml
pdk = "ihp-sg13g2"
```

It names a registered PDK and nothing else (never a path), so a project you clone cannot point aispice at code of its choosing. The agent can write it for you with the `pdk` tool's `use` action, through the usual approval.

In such a project:

- Place devices from the `sg13g2` symbol folder: `sg13g2\sg13_lv_nmos`, `sg13_lv_pmos` (1.2 V), `sg13_hv_nmos`, `sg13_hv_pmos` (3.3 V), the resistors `rsil`, `rppd`, `rhigh`, the MIM capacitor `cap_cmim`, and the HBTs `npn13G2`, `npn13G2l`, `npn13G2v`. Each netlists as the PDK's subcircuit with IHP's default sizes; set sizes in SpiceLine (`w=2u l=0.5u ng=2 m=1`). Resistors have a body pin (BN) and HBTs a substrate pin (BN): wire them, usually to ground.
- Every netlist ends with the PDK's model header, one line per device class the circuit uses, as in IHP's own test benches:

  ```text
  .lib /home/me/IHP-Open-PDK/ihp-sg13g2/libs.tech/ngspice/models/cornerMOSlv.lib mos_tt
  .lib /home/me/IHP-Open-PDK/ihp-sg13g2/libs.tech/ngspice/models/cornerRES.lib res_typ
  ```

  A netlist that already includes a corner library keeps its own line.
- Simulations run on ngspice (the default) or Xyce. On ngspice aispice loads the PDK's OSDI files through the start-up file it writes for each run and runs ngspice in its native mode, because ngspice's LTspice and PSpice compatibility modes break the PDK's `.lib` sections and its PSP models. On Xyce it passes the PDK's plugins with `-plugin` and reads `libs.tech/xyce/models` (the HBT from the ngspice library, as IHP's Xyce test benches do). LTspice cannot load the models, so it is refused.
- `gm_id` (and `aispice gmid`) sizes a transistor by gm/Id:

  ```console
  $ aispice gmid sg13_lv_nmos --gm-id 12 --l 0.5u --id 50u
  sg13_lv_nmos at L = 500nm, VDS = 600mV, gm/Id = 12 S/A:
    W = 8.185um, Id = 50.01uA
    SpiceLine: w=8.185u l=500n ng=2 m=1
    VGS = 393.1mV (Vth 275.7mV), current density 6.11uA per um of width
    ...
  ```

  The first use of a device sweeps it on ngspice over L, VDS and VGS (a few seconds) and caches the table in `~/.cache/aispice/gmid/<pdk>/<device>.json`, rebuilt when the PDK's commit or model files change. The answer is then simulated at the size found and corrected, since the models do not scale exactly with finger width.

## Layout

aispice lays out, checks and extracts analog blocks on the PDK the project uses. It needs:

- **KLayout 0.30 or newer** (`klayout` on PATH, or `klayout_path`). It runs IHP's DRC and LVS decks, draws layouts with IHP's pcells and renders pictures, always in batch mode (`-b`), so no window opens. The klayout.org packages work; aispice's tests use 0.30.4.
- **Magic 8.3.573 or newer** (`magic` on PATH, or `magic_path`), for extraction with IHP's tech file, run without graphics (`-dnull`). Tested with 8.3.677.
- **The pcell support submodules of IHP-Open-PDK**, which the pcells import:

  ```sh
  cd ~/IHP-Open-PDK
  git submodule update --init ihp-sg13g2/libs.tech/klayout/python/pycell4klayout-api \
      ihp-sg13g2/libs.tech/klayout/python/pypreprocessor
  ```

`aispice doctor` shows whether KLayout and Magic were found.

### Generate a layout

```sh
aispice layout generate ota.asc --template ota5t
aispice layout render ota.gds -o ota.png
```

A generator is a deterministic planner: it reads the circuit, checks the topology it expects, splits each device into two-finger units, orders matched devices in a common-centroid or interdigitated row with dummy fingers at both ends and a guard ring, routes every net on Metal3 tracks (joined across rows on Metal4) and labels the pins. One KLayout script draws the plan with IHP's own MOS pcells. Templates and their roles:

| Template | Roles |
|---|---|
| `current_mirror` | `ref` (diode-connected), `out` (one or more) |
| `diff_pair` | `pair` |
| `ota5t` | `pair`, `load`, `tail`, `bias` (optional) |
| `two_stage` | the `ota5t` roles plus `cs` (second stage), `sink` (its current source) and `cc` (a `cap_cmim` of at least 2 um a side, drawn above the transistors with both plates brought down to the routing) |
| `comparator` | `tail`, `pair`, `latch_n`, `latch_p`, `reset` (StrongARM latch) |

Roles are found from the topology when you give none; `--roles '{"pair": ["M1", "M2"], "tail": "M5"}'` names them. Parameters (`--params` as JSON): `pattern` (`common_centroid` or `interdigitated`), `dummies` (0 to 4 per row end), `guard_ring`, `fingers` (of the smallest device in a row; even), `track_pitch` and `row_gap` in um. Transistors must be `sg13_lv_nmos` or `sg13_lv_pmos`, sized on the 5 nm grid, with fingers at least 0.3 um wide.

`generate` writes `<cell>.gds` and `<cell>.layout.json` (the template, roles, parameters, the devices drawn and the dummies) and runs DRC and LVS. When DRC fails on a rule a parameter addresses (spacing on the routing layers, wells, latch-up), it changes that parameter and draws again, up to three times. It overwrites only a layout it made itself.

### Check and extract

```sh
aispice drc ota.gds                  # main rules, IHP's extra rules and antenna
aispice lvs ota.gds ota.asc          # against the circuit's devices (and the layout's dummies)
aispice extract ota.gds --resistance # Magic: devices, capacitors, resistor networks
aispice check ota.asc --post-layout ota.gds
```

LVS writes the circuit's PDK devices in the CDL form IHP's deck reads. For a layout from a generator, only the devices it drew are compared and its dummy transistors are added to the schematic side (KLayout drops a dummy whose terminals are all one net, so those are left out). Pins are the nets the devices share with the rest of the circuit, and ground.

Extraction runs Magic's `extract` and `ext2spice` with coupling capacitance (capacitors under 0.01 fF dropped) and, with resistance, `extresist` on every net. Magic cannot split a substrate net that carries a label, so with resistance the substrate net (ground) stays one ideal node and the report says so. `check --post-layout` puts the extracted subcircuit in place of the laid-out devices, joined by net name, keeps the PDK's model header, and reports each spec before and after layout. A net the test bench uses that the layout does not label is an error that names the net.

Reports, logs, KLayout's extracted netlist and Magic's netlists are kept in `.aispice/layout/<run>/`.

## Model providers

aispice works with Anthropic, OpenAI, Google Gemini, OpenRouter, Ollama, and any OpenAI-compatible endpoint (LM Studio, vLLM, a company gateway).

Store a key once:

```sh
aispice keys set anthropic      # reads the key from stdin
aispice keys list               # which providers have a key, and from where
aispice keys rm anthropic
```

Keys go into the OS keychain (Secret Service on Linux, Keychain on macOS, Credential Manager on Windows). Where no keychain is available, as on a headless Linux server, they go into `keys.json` in the config folder, readable only by you. Environment variables win over both, which suits CI and one-off runs:

| Provider | Variable |
|---|---|
| `anthropic` | `ANTHROPIC_API_KEY` |
| `openai` | `OPENAI_API_KEY` |
| `google` | `GOOGLE_API_KEY` or `GEMINI_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` |

Ollama needs no key and is reached at `http://localhost:11434/v1`. To list the models a provider offers: `aispice models google`.

The desktop app manages the same keys in Settings.

## Config file

| Platform | Path |
|---|---|
| Linux | `~/.config/aispice/config.toml` |
| macOS | `~/Library/Application Support/aispice/config.toml` |
| Windows | `%APPDATA%\aispice\config.toml` |

Every field is optional:

```toml
provider = "anthropic"          # default provider
model = "claude-opus-5-5"       # default model for that provider
simulator = "auto"              # auto, ngspice, ltspice, xyce or spectre
edit_mode = "apply"             # apply (with undo) or ask (approve each edit)
# ltspice_path = "/home/me/.wine/drive_c/Program Files/ADI/LTspice/LTspice.exe"
# xyce_path = "/home/me/.local/xyce/bin/Xyce"   # a Xyce other than the one on PATH
# klayout_path = "/opt/klayout/klayout"         # a KLayout other than the one on PATH
# magic_path = "/opt/magic/bin/magic"           # a Magic other than the one on PATH
# embedded_models_only = true   # never use models or symbols from LTspice's library

[agent]
max_steps = 40                  # tool calls per request
thinking = true

# A local OpenAI-compatible server under its own name.
[providers.lmstudio]
base_url = "http://localhost:1234/v1"
```

With `provider = "lmstudio"` (or `--provider lmstudio`) aispice talks to that endpoint.

`embedded_models_only = true` makes results independent of the machine. Normally, when LTspice is installed, aispice offers its symbols and pulls the models a circuit names (a `2N3904`, a vendor op-amp) from LTspice's library when it runs ngspice, Xyce or Spectre, and switches to LTspice for a circuit ngspice cannot run. With this setting it uses only its own embedded models (the generic library behind `opamp`, `1N4148`, `2N3904` and the others) and the files in the project folder, never LTspice's symbols or models, and it does not switch simulators by itself. A run on LTspice chosen by name still reads LTspice's library, as LTspice always does. `aispice eval` always works this way.

## Using aispice from an MCP client

`aispice mcp` serves every tool over the Model Context Protocol on stdio. It works in the folder you give it and refuses paths outside it.

Claude Code:

```sh
claude mcp add aispice -- aispice mcp --project /path/to/circuits
```

Cursor, Claude Desktop, and other clients that take a JSON server list:

```json
{
  "mcpServers": {
    "aispice": {
      "command": "aispice",
      "args": ["mcp", "--project", "/path/to/circuits"]
    }
  }
}
```

## What aispice writes in a project

| Path | Contents |
|---|---|
| `<circuit>.specs` | The spec table for a circuit, one spec per line. Commit it with the circuit. |
| `aispice.toml` | Project settings: `pdk = "<name>"` for an IC project. Written only when you (or the agent, with approval) choose a PDK. |
| `.aispice/history/` | A snapshot before every edit, for undo |
| `.aispice/runs/` | Recent simulation output |
| `<cell>.gds`, `<cell>.layout.json` | A generated layout and how it was made (template, roles, parameters, devices, dummies). Commit both. |
| `.aispice/layout/` | DRC and LVS reports, logs and extracted netlists |

Add `.aispice/` to `.gitignore` unless you want history in version control. Chat sessions from the desktop app are kept in your user data folder, not in the project.

A specs file looks like this:

```text
# rc.specs
fc   = bandwidth_3db(V(out)) in 950..1050 Hz
gain = gain_db_at(V(out)/V(in), 10) >= -0.5
```

## Troubleshooting

Start with `aispice doctor`. It lists every simulator it found with its version, the LTspice library, which providers have a key, and the config file in use.

**"No simulator found."** Install ngspice (see above) and check that `ngspice` runs in a terminal. On Windows, make sure the folder with `ngspice_con.exe` is on `PATH`, then open a new terminal.

**LTspice under Wine is not found.** aispice looks in `$WINEPREFIX`, then `~/.wine`. If LTspice is in another prefix, set `WINEPREFIX` or `ltspice_path`. Run LTspice once by hand so Wine finishes creating its prefix.

**LTspice hangs or fails on a headless machine.** Install `xvfb` (`sudo apt-get install xvfb`). aispice uses `xvfb-run` when neither `DISPLAY` nor `WAYLAND_DISPLAY` is set.

**A part is reported as an unknown symbol.** The schematic uses a symbol from LTspice's library or a custom `.asy` file. Install LTspice, set `AISPICE_LTSPICE_LIB`, or pass the folder with `--symbols <dir>`. Symbols next to the schematic are always found.

**Keys are not saved on a Linux server.** Without a Secret Service (GNOME Keyring or KWallet), keys go to `keys.json` in the config folder. You can also use the environment variables above.

**"this project uses PDK ... but no PDK is registered".** Register it with `aispice pdk add`, using the name in `aispice.toml`. The desktop app reads the registry when it starts and when settings are saved.

**ngspice says "Unknown model type psp103va".** The PDK's OSDI files are missing or were not loaded. Run `aispice pdk show <name>` to see which were found, and compile them with IHP's `openvaf-compile-va.sh`. A netlist that includes the PDK's libraries by hand only works in a project whose `aispice.toml` names the PDK.

**HTTP 429 from the provider.** Free tiers allow only a few requests per minute. aispice retries with backoff; for long agent runs, use a paid tier or a local model through Ollama.

**The agent cannot reach a local model.** Check the server is running and that `base_url` ends in `/v1` for OpenAI-compatible servers. Pick a model that supports tool calling.

**Spectre runs fail.** Run `ssh <host> true` in a terminal; it must succeed without a prompt. Check that `spectre` is on the remote `PATH` after your `setup_command`.

**The desktop app shows a blank window on Linux.** Some GPU drivers break WebKitGTK's DMA-BUF renderer. Start the app with `WEBKIT_DISABLE_DMABUF_RENDERER=1`.

If none of this helps, [open an issue](https://github.com/danieltyukov/aispice/issues/new/choose) with the output of `aispice doctor`.
