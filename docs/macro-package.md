# Macro package, version macro/v1

This file is the contract between ChipLeap and aispice. The canonical copy is `docs/macro-package.md` in [chipleap](https://github.com/danieltyukov/chipleap); [aispice](https://github.com/danieltyukov/aispice) keeps an identical copy at the same path. The two copies, the JSON schema and the shared test fixtures must stay byte for byte identical: change them together, in both repositories, in the same release cycle. A test in each repository pins their SHA-256 hashes so a one-sided change fails CI.

## Purpose

A macro package is one hardened block, handed from the tool that built it to the tool that places it:

- aispice exports analog blocks (a bandgap, a comparator, an OTA) that ChipLeap places in a digital-on-top chip.
- ChipLeap exports hardened digital blocks (a counter, a controller) that aispice places in an analog-on-top chip.

The package is the only coupling between the two tools. Neither tool reads the other's projects, databases or configuration; each reads and writes packages. Both tools ship a validator with the same rules, the same rule ids and the same messages:

```sh
chipleap macro check path/to/tiny_ota.macro
aispice macro check path/to/tiny_ota.macro
```

## Layout

```text
<name>.macro/
  macro.toml            what the block is: name, kind, PDK, size, pins, supplies, corners
  <name>.gds            layout
  <name>.lef            abstract for placement and routing
  <name>.lib            liberty: pins, capacitance, supplies, timing arcs, leakage
  <name>.v              Verilog model: behavioral (digital) or blackbox or wreal (analog)
  <name>.spice          netlist for LVS and mixed-signal simulation
  <name>.specs.toml     measured spec table with corners (analog macros)
  evidence/
    drc.json            DRC summary
    lvs.json            LVS summary
    <operation>.json    more summaries: antenna, sta, density and so on
  README.md             optional notes for people
```

`<name>` is the macro name from `macro.toml`. It is the GDS top cell, the LEF macro, the liberty cell, the Verilog module and the SPICE subcircuit, so it must be a valid identifier in all five: letters, digits and underscores, starting with a letter or underscore, at most 64 characters. The directory should be called `<name>.macro`; a different directory name is a warning.

Required files per kind:

| File | analog | digital |
|---|---|---|
| `macro.toml` | required | required |
| `<name>.gds` | required | required |
| `<name>.lef` | required | required |
| `<name>.lib` | required, timing arcs optional | required, every output signal pin has a timing arc |
| `<name>.v` | required, blackbox or wreal model allowed | required, behavioral model |
| `<name>.spice` | required, transistor level | required, cell level allowed |
| `<name>.specs.toml` | required | optional, checked when present |
| `evidence/drc.json`, `evidence/lvs.json` | required, status pass | required, status pass |
| liberty file of each corner that names one | required | required |

Other files may sit in the package (reports, logs, a README). Validators ignore them.

Why a separate `.specs.toml` instead of aispice's `.specs` files: in aispice a `.specs` file holds spec definitions (a measurement expression and limits, in one line each) that refer to a testbench's nodes. A macro package carries no testbench, so it carries measured results instead, keyed by corner.

## macro.toml

```toml
#:schema https://raw.githubusercontent.com/danieltyukov/chipleap/main/src/chipleap/macro/macro-v1.schema.json
schema = "macro/v1"
name = "tiny_ota"
kind = "analog"                       # analog | digital
description = "Five-transistor OTA"   # optional

[pdk]
name = "ihp-sg13g2"
commit = "22f2a25f1734796de3debbbf29cf697cbbc54081"   # 7 to 40 lowercase hex digits

[producer]
tool = "aispice"
version = "0.2.0"
source = "examples/ota"               # optional: where the block came from

[size]
width_um = 12.0                       # the placement boundary, also the LEF SIZE
height_um = 8.0

[[domains]]
name = "core"
voltage = 1.2                         # nominal supply in volts
power = ["vdd"]
ground = ["vss"]

[[pins]]
name = "inp"
direction = "input"                   # input | output | inout
kind = "analog"                       # signal | clock | analog | power | ground
domain = "core"                       # required for signal, clock and analog pins
layer = "Metal2"                      # optional: the LEF pin must have a shape on it
description = "Non-inverting input"   # optional

[[pins]]
name = "vdd"
direction = "inout"                   # supply pins are always inout
kind = "power"                        # no domain: [[domains]] lists supply pins

[[corners]]
name = "tt"
process = "tt"
temperature = 27                      # degrees Celsius

[[corners]]
name = "ss"
process = "ss"
temperature = 125
voltages = { core = 1.08 }            # optional: supply per domain, default nominal
liberty = "tiny_ota__ss.lib"          # optional: a liberty file for this corner

[extra]                               # optional: tool-specific data, never validated
```

Fields:

- `schema` is the string `macro/v1`. A validator that sees a different `macro/` version reports `manifest.version` and stops.
- `kind` is `analog` or `digital`. It decides the required files and two content rules (timing arcs, measured specs).
- `pdk.name` names the process, for example `ihp-sg13g2`. `pdk.commit` is the git commit of the PDK the block was built and signed off with. PDKs change often, so the commit is required.
- `producer` says which tool and version wrote the package.
- `size` is the placement boundary in micrometres. The LEF `SIZE` must match it, and every GDS shape must lie inside it.
- `domains` lists the supply domains. Each names its nominal voltage and its power and ground pins. A ground pin may belong to several domains.
- `pins` lists every pin of the block in one order that all views follow.
  - `width` (an integer of at least 1) makes a pin a bus with bits `name[0]` to `name[width-1]`. LEF, liberty and SPICE name the bits that way; Verilog declares the bus with a range of the same width. Without `width` the pin is a scalar. Supply pins cannot be buses.
  - `kind` `clock` marks a clock input or output, `analog` a pin that carries an analog value, `signal` any other digital pin.
- `corners` lists the conditions the block was characterized at. Analog macros need at least one, since the measured specs refer to them. `<name>.lib` is characterized at the first corner. A corner with a `liberty` file has a liberty of its own, checked like `<name>.lib`.
- `macro.toml` and the spec table are TOML 1.0, which every TOML reader understands (Python's `tomllib` reads nothing newer). What TOML 1.1 added (the `\e` and `\xHH` escapes, line breaks and trailing commas in inline tables, times without seconds) is a syntax error, and so are an integer that does not fit in 64 bits and a float too large for a double. Text files are UTF-8; the view formats are read as ASCII, so names, keywords and numbers in them must be ASCII.
- File names in `macro.toml` are bare names inside the package; paths are not allowed.
- Unknown fields are errors, so a typo does not pass silently. Put tool-specific data under `[extra]`.

The JSON schema `macro-v1.schema.json` (in ChipLeap at `src/chipleap/macro/`, in aispice at `crates/aispice-core/src/macropkg/`) describes `macro.toml`, and under `$defs` the evidence summary (`#/$defs/evidence`) and the spec table (`#/$defs/specs_file`). Editors that understand JSON schemas for TOML (Taplo, Even Better TOML) use it through the `#:schema` comment.

## Views

All names are case-sensitive except in SPICE, which is case-insensitive. Each view must declare exactly the pins of `macro.toml`: a missing pin and an extra pin are both errors.

**GDS.** A cell named `<name>` must be a top cell. Other top cells are a warning, since they would be merged into the chip. Subcells should carry the macro name as a prefix (`tiny_ota_pair`), so two macros merged into one chip never share a cell name; the validator does not check this. No cell may contain itself, directly or through other cells. The bounding box of the top cell, through its whole hierarchy and ignoring text, must lie inside the boundary from `macro.toml`, placed by the LEF `ORIGIN`. Each pin bit should have a text label of the same name in the top cell, as LVS finds pins by their labels; a missing label is a warning.

**LEF.** The file must contain `MACRO <name>` with `CLASS BLOCK` (another class is a warning) and a `SIZE` equal to `macro.toml` within 0.0005 um. Every pin bit has a `PIN` with at least one `PORT` shape. Signal, clock and analog pins have a `DIRECTION` equal to `macro.toml`'s (`OUTPUT TRISTATE` counts as output). Power pins have `USE POWER`, ground pins `USE GROUND`, and no other pin may use either; a `USE` that does not match a signal, clock or analog pin is a warning. If `macro.toml` gives a pin `layer`, the LEF pin has a shape on that layer. Bus bits use square brackets (`BUSBITCHARS "[]"`).

**Liberty.** The file must contain `cell(<name>)`. Signal, clock and analog pins are `pin` groups, directly or as bits of a `bus` whose `bus_type` names a `type` group in the library, with the same `direction`. Power and ground pins are `pg_pin` groups with a `pg_type` ending in `_power` or `_ground`. In a digital macro, every output signal or clock pin has at least one `timing` group. Analog macros may have none.

**Verilog.** The file must contain `module <name>`, with ANSI or non-ANSI port declarations. Signal, clock and analog pins are ports with the same direction; a bus has a range of the same width, a scalar has none. Power and ground ports may be left out (they are often behind `ifdef USE_POWER_PINS`); when present they are `inout` or `input`. Preprocessor lines are ignored and both sides of an `ifdef` are read.

**SPICE.** The file must contain `.subckt <name>` whose ports are exactly the pin bits, in any order. Parameters after the ports (`params:` or `name=value`) are ignored. When `*.PININFO` comment lines follow (the CDL convention, `name:I`, `name:O`, `name:B`), the directions of signal, clock and analog pins must agree: `I` is input, `O` output, `B` inout. Digital netlists may instantiate the PDK's standard cells without defining them.

## Spec table: `<name>.specs.toml`

```toml
schema = "macro.specs/v1"

[[specs]]
name = "dc_gain"
description = "Open-loop gain at 1 kHz into 100 fF"   # optional
unit = "dB"
min = 30                                             # optional limits: min, max
typ = 36                                             # optional
measured = { tt = 36.2, ss = 33.8, ff = 37.1 }       # one value per corner
definition = "dc_gain = gain_db_at(V(out)/V(in), 1k) >= 30"   # optional: how it was measured
```

Every key of `measured` must be a corner from `macro.toml`; a corner with no value is a warning. Every measured value must lie within the limits: a value below `min` or above `max` is an error, so a package never claims a spec it does not meet. Spec names are unique.

## Evidence

`evidence/` holds one JSON summary per check that ran. Any `*.json` file there is read as a summary; other files (reports, logs) are kept but not read. A summary follows `#/$defs/evidence`:

```json
{
  "schema": "macro.evidence/v1",
  "operation": "drc",
  "tool": {"name": "klayout", "version": "0.30.4"},
  "pdk": {"name": "ihp-sg13g2", "commit": "22f2a25f1734796de3debbbf29cf697cbbc54081"},
  "engineering": {"status": "pass", "summary": "IHP DRC deck: 0 violations"},
  "inputs": [{"path": "tiny_ota.gds", "sha256": "..."}],
  "artifacts": [{"path": "evidence/drc.lyrdb", "sha256": "..."}],
  "data": {}
}
```

The shape mirrors OpenADA's result envelope (`openada.result/v0alpha1`): the same `schema`, `operation`, `tool`, `engineering` and `inputs` fields, with `engineering.status` one of `pass`, `fail`, `unknown` or `not_applicable`. It adds `pdk`, which OpenADA results do not carry, and drops the host and execution details a hand-off does not need. An OpenADA result can be dropped into `evidence/` unchanged; validators read its `operation`, `engineering.status` and `inputs` and skip the PDK comparison for it.

Rules:

- A package has at least one summary for `drc` and one for `lvs`, each with status `pass`.
- Every other summary has status `pass` or `not_applicable`.
- An input whose file name matches a file in the package root must have that file's SHA-256. A mismatch means the file changed after the check ran, so the evidence is stale.
- The summary's `pdk` must name the same PDK and commit as `macro.toml`. Two commits match when one is a prefix of the other.

Known operations: `drc`, `lvs`, `antenna`, `erc`, `density`, `sta`, `pex`, `sim`, `precheck`. Others are allowed.

## Checking against the chip's PDK

The tool that places a macro passes its own PDK: `macro check DIR --pdk ihp-sg13g2` or `--pdk ihp-sg13g2@22f2a25`. A different PDK name is an error; a different commit is a warning, because a PDK update often leaves a block valid but should be looked at.

## Limits

A package can come from anyone, so a validator reads it within limits that no real macro comes near. A file or a structure past a limit gives a finding, never a hang, a crash or a machine out of memory, and both tools use the same values. A MiB is 1,048,576 bytes.

| What | Limit | Finding |
|---|---|---|
| `macro.toml` | 4 MiB | `file.too_large` |
| `<name>.specs.toml` | 1 MiB | `file.too_large` |
| an evidence summary | 4 MiB | `file.too_large` |
| a liberty file | 16 MiB | `file.too_large` |
| the LEF, Verilog and SPICE files | 64 MiB each | `file.too_large` |
| the GDS, and a package file an evidence input names | 256 MiB | `file.too_large` |
| tables and arrays nested in `macro.toml` or the spec table | 64 deep | `manifest.limit`, `specs.limit` |
| objects and arrays nested in a summary | 64 deep | `evidence.limit` |
| liberty groups nested | 64 deep | `lib.limit` |
| pin bits in `macro.toml` | 10,000 | `manifest.limit` |
| corners in `macro.toml` | 64 | `manifest.limit` |
| pins of a LEF macro, of the macro's liberty cell (bus bits included), Verilog module or SPICE subcircuit | 10,000 | `lef.limit`, `lib.limit`, `verilog.limit`, `spice.limit` |
| tokens of the LEF, a liberty or the Verilog file; lines of the SPICE file | 8,000,000 | `lef.limit`, `lib.limit`, `verilog.limit`, `spice.limit` |
| GDS records, elements and cells | 32,000,000, 8,000,000 and 100,000 | `gds.limit` |
| GDS references nested below a cell | 64 deep | `gds.limit` |
| points measured for GDS references at angles other than quarter turns | 4,000,000 | `gds.limit` |
| summaries in `evidence/` | 64 | `evidence.limit` |
| package files the evidence inputs name | 32 | `evidence.limit` |
| findings listed in a report | 1,000 | `check.limit` |

- A file's size is checked before it is read. A file past its limit is not read, and the checks that need it are skipped.
- Nesting is checked before a file is parsed: in TOML it is bounded from brackets, braces, table headers and dotted keys, in JSON counted from the brackets outside strings. Past the limit the file is not parsed.
- A GDS with a cell that contains itself (`gds.cycle`) or with references nested too deep is not measured. Each cell is measured once, and an array reference is measured from its four corner instances, so a 32767 x 32767 array costs no more than one instance. Measuring also stops with `gds.limit` when a reference's magnification takes coordinates out of floating-point range.
- Each package file an evidence input names is hashed once per check, however many summaries name it.
- After 1,000 findings a report lists no more. It still counts them in `errors` and `warnings`, and ends with a `check.limit` warning that says how many it left out.

## Rules

Every finding has a rule id, a severity, the file it concerns (relative to the package, with forward slashes) and a message. A package passes when it has no errors; `--strict` also fails it on warnings. The validators check in the order of this table, pin by pin in `macro.toml` order and bit by bit, then extra pins by name. For each file, `file.too_large`, `file.unreadable` and a `*.limit` finding about the whole file come before anything else about it.

| Rule | Severity | Meaning |
|---|---|---|
| `manifest.syntax` | error | `macro.toml` is not valid TOML |
| `manifest.version` | error | `schema` names another `macro/` version |
| `manifest.schema` | error | `macro.toml` does not follow the schema |
| `manifest.limit` | error | `macro.toml` nests too deep, or has too many pin bits or corners (see Limits) |
| `manifest.duplicate_pin` | error | two pins share a name |
| `manifest.domain` | error | two domains share a name, a domain lists a pin that does not exist or is not a supply pin of that kind, a supply pin is in no domain, or a pin names an unknown domain |
| `manifest.corner` | error | two corners share a name, or a corner sets the voltage of an unknown domain |
| `package.dirname` | warning | the directory is not called `<name>.macro` |
| `file.missing` | error | a required file is missing |
| `file.too_large` | error | a file is larger than its limit, so it is not read |
| `file.unreadable` | error | a file is there but cannot be read |
| `gds.parse` | error | the GDS cannot be read |
| `gds.cycle` | error | a cell contains itself, directly or through other cells |
| `gds.limit` | error | the GDS goes past a limit |
| `gds.top_cell` | error | no top cell is called `<name>` |
| `gds.extra_top_cell` | warning | the GDS has more top cells |
| `gds.bbox` | error | the top cell is empty or reaches outside the boundary |
| `gds.label` | warning | a pin bit has no text label in the top cell |
| `lef.parse` | error | the LEF cannot be read |
| `lef.limit` | error | the LEF goes past a limit |
| `lef.macro` | error | no `MACRO <name>` |
| `lef.class` | warning | the macro's class is not `BLOCK` |
| `lef.size` | error | `SIZE` differs from `macro.toml` |
| `lef.missing_pin`, `lef.extra_pin` | error | pin sets differ |
| `lef.direction` | error | a direction differs |
| `lef.use` | error or warning | supply pin `USE` is wrong (error); signal `USE` differs (warning) |
| `lef.port` | error | a pin has no `PORT` shape |
| `lef.pin_layer` | error | a pin has no shape on the layer `macro.toml` names |
| `lib.parse` | error | the liberty file cannot be read |
| `lib.limit` | error | the liberty file goes past a limit |
| `lib.cell` | error | no `cell(<name>)` |
| `lib.missing_pin`, `lib.extra_pin` | error | pin sets differ |
| `lib.direction` | error | a direction differs |
| `lib.pg_pin` | error | a supply pin is not a `pg_pin` of the right type |
| `lib.timing` | error | a digital output pin has no timing arc |
| `verilog.limit` | error | the Verilog file goes past a limit |
| `verilog.module` | error | no `module <name>` |
| `verilog.missing_pin`, `verilog.extra_pin` | error | port sets differ |
| `verilog.direction` | error | a direction differs |
| `verilog.width` | error | a bus width differs, or a bus is declared as a scalar or the other way round |
| `spice.limit` | error | the SPICE file goes past a limit |
| `spice.subckt` | error | no `.subckt <name>` |
| `spice.missing_pin`, `spice.extra_pin` | error | port sets differ |
| `spice.direction` | error | a `*.PININFO` direction differs |
| `specs.limit` | error | the spec table nests too deep |
| `specs.parse` | error | the spec table is not valid TOML |
| `specs.schema` | error | the spec table does not follow `#/$defs/specs_file` |
| `specs.duplicate` | error | two specs share a name |
| `specs.corner` | error or warning | a value is for an unknown corner (error), or a corner has no value (warning) |
| `specs.fail` | error | a measured value is outside its limits |
| `evidence.limit` | error | too many summaries, a summary nested too deep, or too many package files named as inputs |
| `evidence.parse` | error | a summary is not valid JSON |
| `evidence.schema` | error | a summary does not follow `#/$defs/evidence` (or the OpenADA subset) |
| `evidence.missing` | error | no summary for `drc` or for `lvs` |
| `evidence.status` | error | a summary does not report the status it needs |
| `evidence.stale` | error | an input's SHA-256 differs from the package file |
| `pdk.mismatch` | error | a summary names another PDK or commit than `macro.toml` |
| `pdk.expected` | error or warning | the package is for another PDK (error) or commit (warning) than `--pdk` |
| `check.limit` | warning | the report leaves out findings past the first 1,000 |

Messages are identical in both tools, except the detail after `: ` in `*.parse`, `*.syntax` and `file.unreadable` messages, which comes from each tool's parser or operating system. The shared fixtures pin every message.

Report format (`--json`):

```json
{
  "schema": "macro.check/v1",
  "package": "path/as/given",
  "name": "tiny_ota",
  "kind": "analog",
  "pass": true,
  "errors": 0,
  "warnings": 0,
  "findings": [{"rule": "lef.size", "severity": "error", "file": "tiny_ota.lef", "message": "..."}]
}
```

`name` and `kind` are null when `macro.toml` cannot be read or does not follow the schema. `errors` and `warnings` count every finding, also those a report leaves out past its limit. The command exits with 0 when the package passes, 1 when it does not, and 2 when the directory does not exist or the arguments are wrong.

## Versioning

`macro/v1` does not change once released. A change to the layout, a field or a rule's meaning makes `macro/v2`, with a new schema file, and validators may read both. Fixing a message's wording or adding a warning is allowed within v1 when both tools change together.

## Shared fixtures

Both repositories carry the same fixtures (ChipLeap: `tests/fixtures/macro/`, aispice: `crates/aispice-core/tests/fixtures/macro/`): a tiny analog macro, a tiny digital macro, broken variants of each (among them files built to stall a validator and files cut short in every format), and `expected.json` with the exact findings each validator must report for each case. Their content is synthetic and written for the tests: a few rectangles on documented IHP SG13G2 layer numbers, hand-written LEF, liberty, Verilog and SPICE, invented spec values, and evidence that says so. No PDK file is copied. `generate.py` in that folder writes them all; `SHA256SUMS` lists every file's hash.
