# Tools

Generated with `aispice docs tools`. The desktop app's agent and the MCP server expose the same tools.

## `list_circuits`

List the circuit files in the open project (.asc schematics and SPICE netlists), newest first. Only needed when you do not know the file name: when the user names a file, read it directly.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {},
  "type": "object"
}
```

</details>

## `read_schematic`

Describe a schematic in circuit terms: every component with its value, its attributes in brackets and the net on each pin, every net with its members, the directives, and electrical rule check results. Read a circuit before editing it. The value column is what set_value changes; for an op-amp or other subcircuit it is the subcircuit's name (opamp), and parameters such as [SpiceLine2: GBW=10Meg] are changed with set_attr on that attribute. The pin names listed for each part are the ones edit_schematic expects.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "circuit": {
      "description": "Path of the circuit relative to the project folder, e.g. `rc.asc`.",
      "type": "string"
    }
  },
  "required": [
    "circuit"
  ],
  "type": "object"
}
```

</details>

## `edit_schematic`

Change a schematic with circuit-level edits. Refer to parts by instance name (R1) and to pins as PART.PIN with the pin names read_schematic lists (R1.A, R1.2, Q1.B, V1.+, and U1.invin, U1.noninvin, U1.out for the built-in op-amp). Never compute coordinates: `connect` routes wires itself and only ever joins the two nets you name (it falls back to net labels when no clean route exists), `connect_to_net` attaches a pin to a named net or to ground (`0`), and `add_component` without `at` finds free space near `near`. Typical sequence for a new part: add_component, then connect or connect_to_net for each pin. Two-terminal parts are vertical by default; orient R90 makes them horizontal. Directives (.tran, .ac, .op, .param, .meas, .step) go in with add_directive.
The edit is saved immediately (the user can undo it) and LTspice reloads if it is open. The result lists what changed, a semantic diff, and any electrical rule problems the edit introduced; fix those before simulating.

Edits your files: each write is approved in ask-before-applying mode and can be undone.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "circuit": {
      "description": "Path of the schematic relative to the project folder.",
      "type": "string"
    },
    "edits": {
      "description": "Edits, applied in order and atomically: if one fails, none are kept.\nErrors name the edit by its position in this list, counting from 0.",
      "items": {
        "description": "One edit. Fields per op: add_component {symbol; optional name, value, orient, near, at: [x, y], attrs} | remove {name} | replace_symbol {name, symbol} | move {name, to: [x, y]} | rotate {name; optional orient} | set_value {name, value} | set_attr {name, key, value} | rename {name, new_name} | connect {from: PIN, to: PIN} | connect_to_net {pin, net} | disconnect {pin} | add_wire {from: [x, y], to: [x, y]} | remove_wire {from: [x, y], to: [x, y]} | add_label {at: [x, y], label} | remove_label {label; optional at: [x, y]} | add_directive {text; optional at: [x, y]} | remove_directive {matching} | replace_directive {matching, text} | add_comment {text; optional at: [x, y]}. PIN is PART.PIN with a pin name read_schematic lists, such as R1.A, R1.2, Q1.B, V1.+, or U1.invin, U1.noninvin, U1.out for the built-in op-amp. Also accepted: connect_to_net `to` for `net`, connect_to_net `net_name` for `net`, connect_to_net `from` for `pin`, connect `pin` for `from`, set_attr `attr` for `key`, set_attr `attribute` for `key`, move `at` for `to`, rename `to` for `new_name`, add_label `net` for `label`, remove_label `net` for `label`, add_directive `directive` for `text`, replace_directive `directive` for `text`, remove_directive `text` for `matching`; `component` or `part` for `name`; set_attribute for set_attr.",
        "properties": {
          "at": {
            "description": "add_component, add_label, remove_label, add_directive, add_comment: a sheet position [x, y]. Leave out for automatic placement.",
            "items": {
              "type": "integer"
            },
            "maxItems": 2,
            "minItems": 2,
            "type": "array"
          },
          "attrs": {
            "additionalProperties": {
              "type": "string"
            },
            "description": "add_component: extra attributes such as {\"SpiceLine\": \"AC 1\"}.",
            "type": "object"
          },
          "from": {
            "anyOf": [
              {
                "type": "string"
              },
              {
                "items": {
                  "type": "integer"
                },
                "maxItems": 2,
                "minItems": 2,
                "type": "array"
              }
            ],
            "description": "connect: a pin such as R1.B. add_wire, remove_wire: a point [x, y]."
          },
          "key": {
            "description": "set_attr: attribute name (Value, Value2, SpiceLine, SpiceLine2, SpiceModel, Prefix). `attr` is accepted in its place.",
            "type": "string"
          },
          "label": {
            "description": "add_label, remove_label: the net label text.",
            "type": "string"
          },
          "matching": {
            "description": "remove_directive, replace_directive: text that identifies the directive, e.g. .tran.",
            "type": "string"
          },
          "name": {
            "description": "The component's instance name (R1). For add_component, optional: defaults to the next free name.",
            "type": "string"
          },
          "near": {
            "description": "add_component: place next to this component.",
            "type": "string"
          },
          "net": {
            "description": "connect_to_net: the net name as read_schematic shows it; 0 or gnd for ground. A name, never a pin reference such as U1.out: to wire two pins use connect. `to` is accepted in its place.",
            "type": "string"
          },
          "new_name": {
            "description": "rename: the new instance name.",
            "type": "string"
          },
          "op": {
            "description": "The operation.",
            "enum": [
              "add_component",
              "remove",
              "replace_symbol",
              "move",
              "rotate",
              "set_value",
              "set_attr",
              "rename",
              "connect",
              "connect_to_net",
              "disconnect",
              "add_wire",
              "remove_wire",
              "add_label",
              "remove_label",
              "add_directive",
              "remove_directive",
              "replace_directive",
              "add_comment"
            ],
            "type": "string"
          },
          "orient": {
            "description": "add_component, rotate: R0 (default, vertical two-terminal parts), R90 (horizontal), R180, R270, M0, M90, M180, M270.",
            "enum": [
              "R0",
              "R90",
              "R180",
              "R270",
              "M0",
              "M90",
              "M180",
              "M270"
            ],
            "type": "string"
          },
          "pin": {
            "description": "connect_to_net, disconnect: a pin such as V1.- or C1.B.",
            "type": "string"
          },
          "symbol": {
            "description": "add_component, replace_symbol: symbol name such as res, cap, ind, voltage, current, diode, npn, pnp, nmos, pmos, OpAmps\\opamp, OpAmps\\opamp2, bv, e, g, sw, or a library part.",
            "type": "string"
          },
          "text": {
            "description": "add_directive, replace_directive, add_comment: the text, e.g. .tran 10m or .ac dec 20 10 100k.",
            "type": "string"
          },
          "to": {
            "anyOf": [
              {
                "type": "string"
              },
              {
                "items": {
                  "type": "integer"
                },
                "maxItems": 2,
                "minItems": 2,
                "type": "array"
              }
            ],
            "description": "connect: a pin such as C1.A. move: the new position [x, y]. add_wire, remove_wire: a point [x, y]."
          },
          "value": {
            "description": "add_component, set_value, set_attr: the value, e.g. 4.7k, 100n, SINE(0 1 1k).",
            "type": "string"
          }
        },
        "required": [
          "op"
        ],
        "type": "object"
      },
      "type": "array"
    },
    "reason": {
      "description": "One short sentence saying why, shown in the history.",
      "type": "string"
    }
  },
  "required": [
    "circuit",
    "edits"
  ],
  "type": "object"
}
```

</details>

## `create_schematic`

Create a new LTspice schematic in the project. Give a SPICE netlist to have it drawn as a readable schematic (parts placed, wires routed, checked to netlist back to the same circuit), or build it from an empty sheet with the edits edit_schematic accepts, or both (edits apply after the drawing). Refuses to overwrite an existing file.

Edits your files: each write is approved in ask-before-applying mode and can be undone.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "circuit": {
      "description": "New file path relative to the project folder, ending in `.asc`.",
      "type": "string"
    },
    "edits": {
      "description": "Edits to apply after the netlist is drawn, or to build the circuit\nfrom an empty sheet (same operations as edit_schematic).",
      "items": {
        "description": "One edit. Fields per op: add_component {symbol; optional name, value, orient, near, at: [x, y], attrs} | remove {name} | replace_symbol {name, symbol} | move {name, to: [x, y]} | rotate {name; optional orient} | set_value {name, value} | set_attr {name, key, value} | rename {name, new_name} | connect {from: PIN, to: PIN} | connect_to_net {pin, net} | disconnect {pin} | add_wire {from: [x, y], to: [x, y]} | remove_wire {from: [x, y], to: [x, y]} | add_label {at: [x, y], label} | remove_label {label; optional at: [x, y]} | add_directive {text; optional at: [x, y]} | remove_directive {matching} | replace_directive {matching, text} | add_comment {text; optional at: [x, y]}. PIN is PART.PIN with a pin name read_schematic lists, such as R1.A, R1.2, Q1.B, V1.+, or U1.invin, U1.noninvin, U1.out for the built-in op-amp. Also accepted: connect_to_net `to` for `net`, connect_to_net `net_name` for `net`, connect_to_net `from` for `pin`, connect `pin` for `from`, set_attr `attr` for `key`, set_attr `attribute` for `key`, move `at` for `to`, rename `to` for `new_name`, add_label `net` for `label`, remove_label `net` for `label`, add_directive `directive` for `text`, replace_directive `directive` for `text`, remove_directive `text` for `matching`; `component` or `part` for `name`; set_attribute for set_attr.",
        "properties": {
          "at": {
            "description": "add_component, add_label, remove_label, add_directive, add_comment: a sheet position [x, y]. Leave out for automatic placement.",
            "items": {
              "type": "integer"
            },
            "maxItems": 2,
            "minItems": 2,
            "type": "array"
          },
          "attrs": {
            "additionalProperties": {
              "type": "string"
            },
            "description": "add_component: extra attributes such as {\"SpiceLine\": \"AC 1\"}.",
            "type": "object"
          },
          "from": {
            "anyOf": [
              {
                "type": "string"
              },
              {
                "items": {
                  "type": "integer"
                },
                "maxItems": 2,
                "minItems": 2,
                "type": "array"
              }
            ],
            "description": "connect: a pin such as R1.B. add_wire, remove_wire: a point [x, y]."
          },
          "key": {
            "description": "set_attr: attribute name (Value, Value2, SpiceLine, SpiceLine2, SpiceModel, Prefix). `attr` is accepted in its place.",
            "type": "string"
          },
          "label": {
            "description": "add_label, remove_label: the net label text.",
            "type": "string"
          },
          "matching": {
            "description": "remove_directive, replace_directive: text that identifies the directive, e.g. .tran.",
            "type": "string"
          },
          "name": {
            "description": "The component's instance name (R1). For add_component, optional: defaults to the next free name.",
            "type": "string"
          },
          "near": {
            "description": "add_component: place next to this component.",
            "type": "string"
          },
          "net": {
            "description": "connect_to_net: the net name as read_schematic shows it; 0 or gnd for ground. A name, never a pin reference such as U1.out: to wire two pins use connect. `to` is accepted in its place.",
            "type": "string"
          },
          "new_name": {
            "description": "rename: the new instance name.",
            "type": "string"
          },
          "op": {
            "description": "The operation.",
            "enum": [
              "add_component",
              "remove",
              "replace_symbol",
              "move",
              "rotate",
              "set_value",
              "set_attr",
              "rename",
              "connect",
              "connect_to_net",
              "disconnect",
              "add_wire",
              "remove_wire",
              "add_label",
              "remove_label",
              "add_directive",
              "remove_directive",
              "replace_directive",
              "add_comment"
            ],
            "type": "string"
          },
          "orient": {
            "description": "add_component, rotate: R0 (default, vertical two-terminal parts), R90 (horizontal), R180, R270, M0, M90, M180, M270.",
            "enum": [
              "R0",
              "R90",
              "R180",
              "R270",
              "M0",
              "M90",
              "M180",
              "M270"
            ],
            "type": "string"
          },
          "pin": {
            "description": "connect_to_net, disconnect: a pin such as V1.- or C1.B.",
            "type": "string"
          },
          "symbol": {
            "description": "add_component, replace_symbol: symbol name such as res, cap, ind, voltage, current, diode, npn, pnp, nmos, pmos, OpAmps\\opamp, OpAmps\\opamp2, bv, e, g, sw, or a library part.",
            "type": "string"
          },
          "text": {
            "description": "add_directive, replace_directive, add_comment: the text, e.g. .tran 10m or .ac dec 20 10 100k.",
            "type": "string"
          },
          "to": {
            "anyOf": [
              {
                "type": "string"
              },
              {
                "items": {
                  "type": "integer"
                },
                "maxItems": 2,
                "minItems": 2,
                "type": "array"
              }
            ],
            "description": "connect: a pin such as C1.A. move: the new position [x, y]. add_wire, remove_wire: a point [x, y]."
          },
          "value": {
            "description": "add_component, set_value, set_attr: the value, e.g. 4.7k, 100n, SINE(0 1 1k).",
            "type": "string"
          }
        },
        "required": [
          "op"
        ],
        "type": "object"
      },
      "type": "array"
    },
    "netlist": {
      "description": "A SPICE netlist to draw as a readable schematic: elements, `.model`\nand `.subckt` definitions and analysis directives, one per line. Use\nit to turn a netlist, a textbook circuit or a circuit read from an\nimage into a schematic. aispice places and wires every part and\nchecks that the drawing netlists back to exactly this circuit.",
      "type": "string"
    }
  },
  "required": [
    "circuit"
  ],
  "type": "object"
}
```

</details>

## `lint`

Run electrical rule checks on a schematic: missing ground, floating pins, dangling wires, duplicate names, missing values, shorted parts, parallel voltage sources, nets with no DC path to ground, missing analysis directive. Run it after edits and before simulating.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "circuit": {
      "description": "Path of the circuit relative to the project folder, e.g. `rc.asc`.",
      "type": "string"
    }
  },
  "required": [
    "circuit"
  ],
  "type": "object"
}
```

</details>

## `netlist`

Show the SPICE netlist of a schematic, derived the way LTspice does it (instance names, node order, default models), with the PDK's model header in a PDK project. Useful to check exactly what will be simulated.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "circuit": {
      "description": "Path of the circuit relative to the project folder, e.g. `rc.asc`.",
      "type": "string"
    }
  },
  "required": [
    "circuit"
  ],
  "type": "object"
}
```

</details>

## `history`

List a schematic's saved versions, or undo, redo or restore one. Every edit made by aispice and every change made in LTspice in between is a version.

Edits your files: each write is approved in ask-before-applying mode and can be undone.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "action": {
      "enum": [
        "list",
        "undo",
        "redo",
        "restore"
      ],
      "type": "string"
    },
    "circuit": {
      "type": "string"
    },
    "snapshot": {
      "description": "Snapshot id for `restore`, from `list`.",
      "type": "string"
    }
  },
  "required": [
    "circuit",
    "action"
  ],
  "type": "object"
}
```

</details>

## `render_schematic`

Draw a schematic as an image (PNG by default) so you can look at its layout: overlapping parts, crossing wires, unreadable labels. For connectivity, read_schematic is more precise than looking.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "circuit": {
      "type": "string"
    },
    "format": {
      "anyOf": [
        {
          "description": "A PNG image, for models that read images.",
          "enum": [
            "png"
          ],
          "type": "string"
        },
        {
          "description": "SVG markup as text.",
          "enum": [
            "svg"
          ],
          "type": "string"
        }
      ]
    }
  },
  "required": [
    "circuit"
  ],
  "type": "object"
}
```

</details>

## `symbols`

Search the symbols available for add_component: aispice's built-in set and, when installed, LTspice's library (thousands of vendor parts such as LT1001 or ADA4898). Shows each symbol's name, prefix and pin names in SPICE order.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "limit": {
      "default": 25,
      "minimum": 0,
      "type": "integer"
    },
    "query": {
      "default": "",
      "description": "Words to match in symbol names and descriptions, e.g. `opamp`,\n`schottky`, `LT1001`. Empty lists the common ones.",
      "type": "string"
    }
  },
  "type": "object"
}
```

</details>

## `simulators`

List the simulators installed on this machine (ngspice, LTspice, Xyce, Spectre) with versions and how each will run.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {},
  "type": "object"
}
```

</details>

## `simulate`

Simulate a circuit and report what came back: analyses, vector names, operating point, the simulator's own .meas results, warnings and errors, plus any measurements you ask for. Fix lint errors first. Use the run id with measure, plot and read_waveform. Measurements are written `name = kind(args)`, for example `f3db = bandwidth_3db(V(out))`, `gain = gain_db_at(V(out)/V(in), 1k)`, `pm = phase_margin(V(out))`, `tr = rise_time(V(out), 10, 90)`, `vmax = max(V(out), 1m, 5m)`. Kinds: value_at(expr, at), min/max/pp/avg/rms/integral(expr[, from, to]), crossing(expr, level[, rise|fall|either, nth]), rise_time/fall_time(expr[, low_pct, high_pct]), overshoot_pct/undershoot_pct(expr), settling_time(expr[, tolerance_pct]), delay(from_expr, to_expr[, level_pct]), frequency/period/duty_cycle(expr), thd(expr, fundamental[, harmonics]), gain_db_at/phase_at(expr, freq), bandwidth_3db(expr[, dc|peak]), unity_gain_freq/phase_margin/gain_margin/peak_gain(expr), freq_at_db(expr, db), peaking_db(expr), q_lowpass(expr), output_noise_at/input_noise_at(freq), output_noise_rms/input_noise_rms([from, to]), psrr_db_at/cmrr_db_at(expr, source, freq). bandwidth_3db is the first frequency where the gain falls 3 dB below the gain at the lowest swept frequency (with `peak`, 3 dB below the peak gain, and for a bandpass the width between the two edges); freq_at_db is where the gain crosses an absolute level in dB, so freq_at_db(V(out), -3) is the -3 dB corner only when the low-frequency gain is 0 dB. peaking_db is the peak gain minus the low-frequency gain (0 for a Butterworth lowpass); q_lowpass estimates the Q of a second-order lowpass from an AC run (0.707 for Butterworth, with its natural frequency as f0); poles_zeros gives Q exactly. Noise measurements read a .noise analysis (such as `.noise V(out) Vin dec 20 1 10Meg`): densities in V/√Hz (A/√Hz for a current source input), rms integrated over the band. psrr_db_at and cmrr_db_at divide |expr| in the circuit's AC run, whose input carries AC 1, by |expr| in a second AC run aispice adds with AC 1 on `source` only (the supply, or the common-mode source that feeds both inputs); expr is the output, such as V(out). Expressions use V(node), V(a,b), I(R1), + - * /, and db(), mag(), ph().

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "analysis": {
      "description": "Run this analysis instead of the circuit's own, without changing the\nfile, e.g. `.ac dec 50 1 10Meg` or `.tran 0 5m 0 1u`.",
      "type": "string"
    },
    "circuit": {
      "type": "string"
    },
    "measurements": {
      "default": [],
      "description": "Measurements to take on the result (see the tool description).",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "simulator": {
      "description": "`ngspice`, `ltspice`, `xyce` or `spectre`. Default: automatic\n(ngspice, falling back to LTspice for LTspice-only constructs).",
      "type": "string"
    }
  },
  "required": [
    "circuit"
  ],
  "type": "object"
}
```

</details>

## `measure`

Take measurements on a simulation result without re-running it. Without a run id it uses the circuit's latest run, and simulates the circuit again (with that run's analysis) when the circuit has changed since, so numbers always describe the circuit as it is; the answer says when that happened. Measurements are written `name = kind(args)`, for example `f3db = bandwidth_3db(V(out))`, `gain = gain_db_at(V(out)/V(in), 1k)`, `pm = phase_margin(V(out))`, `tr = rise_time(V(out), 10, 90)`, `vmax = max(V(out), 1m, 5m)`. Kinds: value_at(expr, at), min/max/pp/avg/rms/integral(expr[, from, to]), crossing(expr, level[, rise|fall|either, nth]), rise_time/fall_time(expr[, low_pct, high_pct]), overshoot_pct/undershoot_pct(expr), settling_time(expr[, tolerance_pct]), delay(from_expr, to_expr[, level_pct]), frequency/period/duty_cycle(expr), thd(expr, fundamental[, harmonics]), gain_db_at/phase_at(expr, freq), bandwidth_3db(expr[, dc|peak]), unity_gain_freq/phase_margin/gain_margin/peak_gain(expr), freq_at_db(expr, db), peaking_db(expr), q_lowpass(expr), output_noise_at/input_noise_at(freq), output_noise_rms/input_noise_rms([from, to]), psrr_db_at/cmrr_db_at(expr, source, freq). bandwidth_3db is the first frequency where the gain falls 3 dB below the gain at the lowest swept frequency (with `peak`, 3 dB below the peak gain, and for a bandpass the width between the two edges); freq_at_db is where the gain crosses an absolute level in dB, so freq_at_db(V(out), -3) is the -3 dB corner only when the low-frequency gain is 0 dB. peaking_db is the peak gain minus the low-frequency gain (0 for a Butterworth lowpass); q_lowpass estimates the Q of a second-order lowpass from an AC run (0.707 for Butterworth, with its natural frequency as f0); poles_zeros gives Q exactly. Noise measurements read a .noise analysis (such as `.noise V(out) Vin dec 20 1 10Meg`): densities in V/√Hz (A/√Hz for a current source input), rms integrated over the band. psrr_db_at and cmrr_db_at divide |expr| in the circuit's AC run, whose input carries AC 1, by |expr| in a second AC run aispice adds with AC 1 on `source` only (the supply, or the common-mode source that feeds both inputs); expr is the output, such as V(out). Expressions use V(node), V(a,b), I(R1), + - * /, and db(), mag(), ph().

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "circuit": {
      "type": "string"
    },
    "measurements": {
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "run": {
      "description": "A run id from simulate. Default: the circuit's latest run, simulating\nfirst if there is none, or again (with that run's analysis) if the\ncircuit has changed since.",
      "type": "string"
    }
  },
  "required": [
    "circuit",
    "measurements"
  ],
  "type": "object"
}
```

</details>

## `check_specs`

Simulate the circuit and check it against a spec table, reporting pass or fail and the margin for each spec. This is how to prove a design meets its requirements; run it after every change that matters. With corners, each spec reports its worst value and the corner where it occurred, and passes only if it passes at every corner. With post_layout (a layout from the layout tool), the layout is extracted and simulated in place of the schematic's devices, and each spec is reported before and after layout (at every corner when corners are given). `corners` crosses process corners (presets tt, ss, ff, sf, fs, or maps such as `mos=ss res=wcs`, which rewrite the circuit's `.lib <file> <family>_<corner>` lines), temperatures in Celsius and supply values (a source or .param), and simulates every corner. Specs are one per line: `name = measurement op limit`, with op one of `>=`, `<=`, `in a..b` or `= target +- tol`, e.g. `bw = bandwidth_3db(V(out)) in 1Meg..2Meg`, `gain = gain_db_at(V(out)/V(in), 1k) >= 20`, `pm = phase_margin(V(out)) >= 45`, `q = q_lowpass(V(out)) = 0.707 +- 0.03`, `vn = input_noise_at(1k) <= 10n`, `psrr = psrr_db_at(V(out), VDD, 1k) >= 60`. Measurements are written `name = kind(args)`, for example `f3db = bandwidth_3db(V(out))`, `gain = gain_db_at(V(out)/V(in), 1k)`, `pm = phase_margin(V(out))`, `tr = rise_time(V(out), 10, 90)`, `vmax = max(V(out), 1m, 5m)`. Kinds: value_at(expr, at), min/max/pp/avg/rms/integral(expr[, from, to]), crossing(expr, level[, rise|fall|either, nth]), rise_time/fall_time(expr[, low_pct, high_pct]), overshoot_pct/undershoot_pct(expr), settling_time(expr[, tolerance_pct]), delay(from_expr, to_expr[, level_pct]), frequency/period/duty_cycle(expr), thd(expr, fundamental[, harmonics]), gain_db_at/phase_at(expr, freq), bandwidth_3db(expr[, dc|peak]), unity_gain_freq/phase_margin/gain_margin/peak_gain(expr), freq_at_db(expr, db), peaking_db(expr), q_lowpass(expr), output_noise_at/input_noise_at(freq), output_noise_rms/input_noise_rms([from, to]), psrr_db_at/cmrr_db_at(expr, source, freq). bandwidth_3db is the first frequency where the gain falls 3 dB below the gain at the lowest swept frequency (with `peak`, 3 dB below the peak gain, and for a bandpass the width between the two edges); freq_at_db is where the gain crosses an absolute level in dB, so freq_at_db(V(out), -3) is the -3 dB corner only when the low-frequency gain is 0 dB. peaking_db is the peak gain minus the low-frequency gain (0 for a Butterworth lowpass); q_lowpass estimates the Q of a second-order lowpass from an AC run (0.707 for Butterworth, with its natural frequency as f0); poles_zeros gives Q exactly. Noise measurements read a .noise analysis (such as `.noise V(out) Vin dec 20 1 10Meg`): densities in V/√Hz (A/√Hz for a current source input), rms integrated over the band. psrr_db_at and cmrr_db_at divide |expr| in the circuit's AC run, whose input carries AC 1, by |expr| in a second AC run aispice adds with AC 1 on `source` only (the supply, or the common-mode source that feeds both inputs); expr is the output, such as V(out). Expressions use V(node), V(a,b), I(R1), + - * /, and db(), mag(), ph().

Edits your files: each write is approved in ask-before-applying mode and can be undone.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "circuit": {
      "type": "string"
    },
    "corners": {
      "description": "Check at every corner of this set and report each spec's worst value,\nits margin and the corner where it occurred, plus every spec at every\ncorner. For example `{\"process\": [\"tt\", \"ss\", \"ff\"], \"temperatures\":\n[-40, 27, 125], \"supplies\": [{\"target\": \"VDD\", \"values\": [1.62,\n1.98]}]}`.",
      "properties": {
        "process": {
          "default": [],
          "description": "Process corners: presets `tt`, `ss`, `ff`, `sf`, `fs`, or\nfamily=corner pairs such as `mos=ss res=wcs` (families mos, res, cap,\ndio, hbt). Needs `.lib <file> <family>_<corner>` lines in the circuit.",
          "items": {
            "type": "string"
          },
          "maxItems": 64,
          "type": "array"
        },
        "supplies": {
          "default": [],
          "description": "Supply values, for example `[{\"target\": \"VDD\", \"values\": [1.62,\n1.8, 1.98]}]`. At most 200 corners in all.",
          "items": {
            "description": "Values for one supply: a voltage source or a `.param`.",
            "properties": {
              "target": {
                "description": "A source (`VDD`) or `.param` name.",
                "type": "string"
              },
              "values": {
                "description": "Its values, as numbers or SPICE text (`\"1.8\"`), at most 64.",
                "items": {
                  "type": "number"
                },
                "maxItems": 64,
                "type": "array"
              }
            },
            "required": [
              "target",
              "values"
            ],
            "type": "object"
          },
          "maxItems": 8,
          "type": "array"
        },
        "temperatures": {
          "default": [],
          "description": "Temperatures in degrees Celsius.",
          "items": {
            "type": "number"
          },
          "maxItems": 64,
          "type": "array"
        }
      },
      "type": "object"
    },
    "post_layout": {
      "description": "Check the specs after layout too: the layout (a GDS from the layout\ntool) is extracted and put in place of the schematic's devices, and\neach spec is reported before and after layout.",
      "properties": {
        "cell": {
          "description": "The cell; default the file's one top cell.",
          "type": "string"
        },
        "layout": {
          "description": "The layout, a GDS file in the project (from the layout tool).",
          "type": "string"
        },
        "resistance": {
          "default": false,
          "description": "Extract resistor networks as well as capacitance (default false).",
          "type": "boolean"
        }
      },
      "required": [
        "layout"
      ],
      "type": "object"
    },
    "save": {
      "default": true,
      "description": "Save the given specs to `<circuit>.specs` for later checks. Default true.",
      "type": "boolean"
    },
    "simulator": {
      "type": "string"
    },
    "specs": {
      "description": "Specs, one per line. When omitted, the `<circuit>.specs` file beside\nthe circuit is used.",
      "type": "string"
    }
  },
  "required": [
    "circuit"
  ],
  "type": "object"
}
```

</details>

## `plot`

Plot waveforms from a simulation as an image: time-domain traces, or a Bode plot (magnitude in dB and phase) for AC data. Use it to look at behaviour; use measure for numbers. Without a run id it uses the circuit's latest run, simulating again first if the circuit has changed since.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "circuit": {
      "type": "string"
    },
    "run": {
      "type": "string"
    },
    "title": {
      "type": "string"
    },
    "traces": {
      "description": "Expressions to draw, e.g. `V(out)`, `V(out)/V(in)` (a Bode plot for AC\ndata), `I(R1)`.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "x_range": {
      "description": "Limit the x axis, e.g. `[0, 0.001]`.",
      "items": {
        "type": "number"
      },
      "maxItems": 2,
      "minItems": 2,
      "type": "array"
    }
  },
  "required": [
    "circuit",
    "traces"
  ],
  "type": "object"
}
```

</details>

## `read_waveform`

Read sampled values of signals from a simulation as a table (evenly spaced over a range). For AC data values are magnitude in dB and phase in degrees. Without a run id it uses the circuit's latest run, simulating again first if the circuit has changed since.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "circuit": {
      "type": "string"
    },
    "from": {
      "type": "number"
    },
    "points": {
      "description": "Number of evenly spaced rows to return (default 40, at most 400).",
      "minimum": 0,
      "type": "integer"
    },
    "run": {
      "type": "string"
    },
    "signals": {
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "to": {
      "type": "number"
    }
  },
  "required": [
    "circuit",
    "signals"
  ],
  "type": "object"
}
```

</details>

## `poles_zeros`

Pole-zero analysis of a transfer function with ngspice's .pz: the poles and zeros of V(output)/V(input), or V(output)/I(input) with transfer `cur`, in Hz, with f0, Q and damping ratio for each complex pair and the corner of each real root, and whether the circuit is stable (any pole in the right half-plane means it is not). Nonlinear parts are linearised at the operating point, as in an AC analysis. The circuit's own analyses, .meas, .step and .save lines are left out of this run, and the file is not changed. Needs ngspice: LTspice and Xyce have no .pz analysis.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "circuit": {
      "type": "string"
    },
    "input": {
      "description": "Positive input node (a net name as read_schematic shows it).",
      "type": "string"
    },
    "input_neg": {
      "description": "Negative input node. Default: ground (`0`).",
      "type": "string"
    },
    "output": {
      "description": "Positive output node.",
      "type": "string"
    },
    "output_neg": {
      "description": "Negative output node. Default: ground (`0`).",
      "type": "string"
    },
    "params": {
      "additionalProperties": {
        "anyOf": [
          {
            "type": "number"
          },
          {
            "type": "string"
          }
        ],
        "description": "A number as JSON or as SPICE text (`2.2k`)."
      },
      "description": "Values for this analysis only, without changing the file: an element\n(`R1`) or `.param` name and its value, e.g. `{\"R1\": \"2.2k\"}`.",
      "type": "object"
    },
    "transfer": {
      "anyOf": [
        {
          "description": "Voltage gain: V(output) / V(input).",
          "enum": [
            "vol"
          ],
          "type": "string"
        },
        {
          "description": "Transimpedance: V(output) / I(input), for a current driven into the\ninput pair.",
          "enum": [
            "cur"
          ],
          "type": "string"
        }
      ],
      "description": "`vol` for voltage gain (default) or `cur` for transimpedance."
    }
  },
  "required": [
    "circuit",
    "input",
    "output"
  ],
  "type": "object"
}
```

</details>

## `operating_point`

Check bias: the DC operating point with each semiconductor device's small-signal values, for sizing transistors (the gm/Id method). MOSFETs: region (cutoff, subthreshold, triode, saturation), id, vgs, vds, vbs, vth (von for level 1 to 3), vdsat, gm, gds, gmbs, gm/id, intrinsic gain gm/gds, cgs and cgd when the model has them, W and L. BJTs: region (active, saturation, cutoff, reverse active), ic, ib, beta, vbe, vce, gm, rpi, ro. Diodes: id, vd, rd. Also node voltages and source currents. Devices inside subcircuits are named by path, as X1.M7. Points out devices in triode or cutoff that look like they should be saturated, such as a current-mirror output. Values are in each device's own polarity (for a PMOS, vgs is vsg). The circuit's own analyses, .meas, .step and .save lines are left out of this run and the file is not changed. Device values need ngspice; without it only node voltages and source currents come back, from LTspice or Xyce.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "circuit": {
      "type": "string"
    },
    "devices": {
      "default": [],
      "description": "Report only these devices: instance names such as `M1`, `X1.M7` for\nM7 inside subcircuit instance X1, or `X1` for every device in it.\nDefault: every MOSFET, BJT and diode.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "params": {
      "additionalProperties": {
        "anyOf": [
          {
            "type": "number"
          },
          {
            "type": "string"
          }
        ],
        "description": "A number as JSON or as SPICE text (`2.2k`)."
      },
      "description": "Values for this analysis only, without changing the file: an element\n(`R1`) or `.param` name and its value, e.g. `{\"Rbias\": \"47k\"}`.",
      "type": "object"
    }
  },
  "required": [
    "circuit"
  ],
  "type": "object"
}
```

</details>

## `sweep`

Sweep component values or .param values (several parameters give every combination, at most 200 runs) and tabulate measurements for each. Works with every simulator. Measurements are written `name = kind(args)`, for example `f3db = bandwidth_3db(V(out))`, `gain = gain_db_at(V(out)/V(in), 1k)`, `pm = phase_margin(V(out))`, `tr = rise_time(V(out), 10, 90)`, `vmax = max(V(out), 1m, 5m)`. Kinds: value_at(expr, at), min/max/pp/avg/rms/integral(expr[, from, to]), crossing(expr, level[, rise|fall|either, nth]), rise_time/fall_time(expr[, low_pct, high_pct]), overshoot_pct/undershoot_pct(expr), settling_time(expr[, tolerance_pct]), delay(from_expr, to_expr[, level_pct]), frequency/period/duty_cycle(expr), thd(expr, fundamental[, harmonics]), gain_db_at/phase_at(expr, freq), bandwidth_3db(expr[, dc|peak]), unity_gain_freq/phase_margin/gain_margin/peak_gain(expr), freq_at_db(expr, db), peaking_db(expr), q_lowpass(expr), output_noise_at/input_noise_at(freq), output_noise_rms/input_noise_rms([from, to]), psrr_db_at/cmrr_db_at(expr, source, freq). bandwidth_3db is the first frequency where the gain falls 3 dB below the gain at the lowest swept frequency (with `peak`, 3 dB below the peak gain, and for a bandpass the width between the two edges); freq_at_db is where the gain crosses an absolute level in dB, so freq_at_db(V(out), -3) is the -3 dB corner only when the low-frequency gain is 0 dB. peaking_db is the peak gain minus the low-frequency gain (0 for a Butterworth lowpass); q_lowpass estimates the Q of a second-order lowpass from an AC run (0.707 for Butterworth, with its natural frequency as f0); poles_zeros gives Q exactly. Noise measurements read a .noise analysis (such as `.noise V(out) Vin dec 20 1 10Meg`): densities in V/√Hz (A/√Hz for a current source input), rms integrated over the band. psrr_db_at and cmrr_db_at divide |expr| in the circuit's AC run, whose input carries AC 1, by |expr| in a second AC run aispice adds with AC 1 on `source` only (the supply, or the common-mode source that feeds both inputs); expr is the output, such as V(out). Expressions use V(node), V(a,b), I(R1), + - * /, and db(), mag(), ph().

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "analysis": {
      "type": "string"
    },
    "circuit": {
      "type": "string"
    },
    "measurements": {
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "params": {
      "description": "Parameters to sweep: an element (`R1`) or a `.param` name, with values\nas a list `[1000, 2200, 4700]`, a range `{\"kind\": \"dec\", \"start\": 1000,\n\"stop\": 100000, \"points_per_decade\": 5}`, or text `\"lin 1k 10k 10\"`.",
      "items": {
        "properties": {
          "target": {
            "description": "An element (`R1`, `C2`, `V1`) or a `.param` name.",
            "type": "string"
          },
          "values": {
            "anyOf": [
              {
                "items": {
                  "type": "number"
                },
                "type": "array"
              },
              {
                "anyOf": [
                  {
                    "description": "`points` values evenly spaced from `start` to `stop`, both included.",
                    "properties": {
                      "kind": {
                        "enum": [
                          "lin"
                        ],
                        "type": "string"
                      },
                      "points": {
                        "minimum": 0,
                        "type": "integer"
                      },
                      "start": {
                        "type": "number"
                      },
                      "stop": {
                        "type": "number"
                      }
                    },
                    "required": [
                      "kind",
                      "start",
                      "stop",
                      "points"
                    ],
                    "type": "object"
                  },
                  {
                    "description": "Logarithmic, `points_per_decade` values per decade from `start`.",
                    "properties": {
                      "kind": {
                        "enum": [
                          "dec"
                        ],
                        "type": "string"
                      },
                      "points_per_decade": {
                        "minimum": 0,
                        "type": "integer"
                      },
                      "start": {
                        "type": "number"
                      },
                      "stop": {
                        "type": "number"
                      }
                    },
                    "required": [
                      "kind",
                      "start",
                      "stop",
                      "points_per_decade"
                    ],
                    "type": "object"
                  },
                  {
                    "description": "Logarithmic, `points_per_octave` values per octave from `start`.",
                    "properties": {
                      "kind": {
                        "enum": [
                          "oct"
                        ],
                        "type": "string"
                      },
                      "points_per_octave": {
                        "minimum": 0,
                        "type": "integer"
                      },
                      "start": {
                        "type": "number"
                      },
                      "stop": {
                        "type": "number"
                      }
                    },
                    "required": [
                      "kind",
                      "start",
                      "stop",
                      "points_per_octave"
                    ],
                    "type": "object"
                  }
                ]
              },
              {
                "type": "string"
              }
            ],
            "description": "The values to sweep: an explicit list, a range, or the text form\n(`\"1k, 2.2k, 4.7k\"`, `\"lin 1k 10k 10\"`, `\"dec 1k 1Meg 10\"`,\n`\"oct 1k 8k 2\"`; ranges are start, stop, count as in `.step`)."
          }
        },
        "required": [
          "target",
          "values"
        ],
        "type": "object"
      },
      "type": "array"
    },
    "simulator": {
      "type": "string"
    }
  },
  "required": [
    "circuit",
    "params",
    "measurements"
  ],
  "type": "object"
}
```

</details>

## `monte_carlo`

Monte Carlo analysis: vary component values within their tolerances, device parameters through the PDK's mismatch or statistical model sections (`mismatch`), or both; simulate each sample and report the yield against the specs with per-spec spread and the worst run. Seeded and reproducible. With corners, every corner runs the same samples and reports its own yield. `corners` crosses process corners (presets tt, ss, ff, sf, fs, or maps such as `mos=ss res=wcs`, which rewrite the circuit's `.lib <file> <family>_<corner>` lines), temperatures in Celsius and supply values (a source or .param), and simulates every corner. Specs are one per line: `name = measurement op limit`, with op one of `>=`, `<=`, `in a..b` or `= target +- tol`, e.g. `bw = bandwidth_3db(V(out)) in 1Meg..2Meg`, `gain = gain_db_at(V(out)/V(in), 1k) >= 20`, `pm = phase_margin(V(out)) >= 45`, `q = q_lowpass(V(out)) = 0.707 +- 0.03`, `vn = input_noise_at(1k) <= 10n`, `psrr = psrr_db_at(V(out), VDD, 1k) >= 60`.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "circuit": {
      "type": "string"
    },
    "corners": {
      "description": "Run the analysis at every corner of this set, `runs` times each, and\nreport the yield per corner as well.",
      "properties": {
        "process": {
          "default": [],
          "description": "Process corners: presets `tt`, `ss`, `ff`, `sf`, `fs`, or\nfamily=corner pairs such as `mos=ss res=wcs` (families mos, res, cap,\ndio, hbt). Needs `.lib <file> <family>_<corner>` lines in the circuit.",
          "items": {
            "type": "string"
          },
          "maxItems": 64,
          "type": "array"
        },
        "supplies": {
          "default": [],
          "description": "Supply values, for example `[{\"target\": \"VDD\", \"values\": [1.62,\n1.8, 1.98]}]`. At most 200 corners in all.",
          "items": {
            "description": "Values for one supply: a voltage source or a `.param`.",
            "properties": {
              "target": {
                "description": "A source (`VDD`) or `.param` name.",
                "type": "string"
              },
              "values": {
                "description": "Its values, as numbers or SPICE text (`\"1.8\"`), at most 64.",
                "items": {
                  "type": "number"
                },
                "maxItems": 64,
                "type": "array"
              }
            },
            "required": [
              "target",
              "values"
            ],
            "type": "object"
          },
          "maxItems": 8,
          "type": "array"
        },
        "temperatures": {
          "default": [],
          "description": "Temperatures in degrees Celsius.",
          "items": {
            "type": "number"
          },
          "maxItems": 64,
          "type": "array"
        }
      },
      "type": "object"
    },
    "mismatch": {
      "description": "Device mismatch from the circuit's corner libraries: `local` switches\ntheir `.lib` sections to `_mismatch`, `global` to `_stat` (process\nspread plus mismatch, typical corner only). The simulator draws the\nparameters with its own seed per run; needs ngspice or Xyce.",
      "oneOf": [
        {
          "description": "`_mismatch` sections: device mismatch around the process corner.",
          "enum": [
            "local"
          ],
          "type": "string"
        },
        {
          "description": "`_stat` sections: global process spread plus mismatch, defined\naround the typical corner.",
          "enum": [
            "global"
          ],
          "type": "string"
        }
      ]
    },
    "runs": {
      "description": "Number of runs (default 50). Runs times corners may be at most 500.",
      "minimum": 0,
      "type": "integer"
    },
    "seed": {
      "minimum": 0,
      "type": "integer"
    },
    "simulator": {
      "type": "string"
    },
    "specs": {
      "description": "Specs to judge each run; default: the `<circuit>.specs` file.",
      "type": "string"
    },
    "tolerances": {
      "default": [],
      "description": "Tolerances: target is an element name or a glob such as `R*` or `C*`,\n`tol_pct` a percentage, `distribution` uniform (default) or gaussian\n(3 sigma = tolerance). May be empty when `mismatch` is set.",
      "items": {
        "properties": {
          "distribution": {
            "anyOf": [
              {
                "description": "Equally likely anywhere within plus or minus the tolerance.",
                "enum": [
                  "uniform"
                ],
                "type": "string"
              },
              {
                "description": "Normal with three standard deviations equal to the tolerance, as\ncomponent tolerances are usually specified.",
                "enum": [
                  "gaussian"
                ],
                "type": "string"
              }
            ],
            "default": "uniform"
          },
          "target": {
            "description": "An element or `.param` name, or a glob over element names such as\n`R*`, `C?` or `*`. Exact names take precedence over globs, and\nearlier globs over later ones.",
            "type": "string"
          },
          "tol_pct": {
            "description": "Tolerance in percent of the nominal value.",
            "type": "number"
          }
        },
        "required": [
          "target",
          "tol_pct"
        ],
        "type": "object"
      },
      "type": "array"
    }
  },
  "required": [
    "circuit"
  ],
  "type": "object"
}
```

</details>

## `optimize`

Size component values to meet a spec table with the simulator in the loop (Nelder-Mead or CMA-ES over log-scaled ranges), then round to standard E-series values and re-check. You choose the parameters and sensible ranges; the optimizer does the search. With corners it optimizes the worst case over every corner; the budget counts candidates, each simulated at every corner. `corners` crosses process corners (presets tt, ss, ff, sf, fs, or maps such as `mos=ss res=wcs`, which rewrite the circuit's `.lib <file> <family>_<corner>` lines), temperatures in Celsius and supply values (a source or .param), and simulates every corner. Specs are one per line: `name = measurement op limit`, with op one of `>=`, `<=`, `in a..b` or `= target +- tol`, e.g. `bw = bandwidth_3db(V(out)) in 1Meg..2Meg`, `gain = gain_db_at(V(out)/V(in), 1k) >= 20`, `pm = phase_margin(V(out)) >= 45`, `q = q_lowpass(V(out)) = 0.707 +- 0.03`, `vn = input_noise_at(1k) <= 10n`, `psrr = psrr_db_at(V(out), VDD, 1k) >= 60`.

Edits your files: each write is approved in ask-before-applying mode and can be undone.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "apply": {
      "default": false,
      "description": "Write the best (snapped) values into the schematic as an undoable edit.",
      "type": "boolean"
    },
    "circuit": {
      "type": "string"
    },
    "corners": {
      "description": "Meet the specs at every corner of this set: each candidate is\nsimulated at every corner and scored on its worst case.",
      "properties": {
        "process": {
          "default": [],
          "description": "Process corners: presets `tt`, `ss`, `ff`, `sf`, `fs`, or\nfamily=corner pairs such as `mos=ss res=wcs` (families mos, res, cap,\ndio, hbt). Needs `.lib <file> <family>_<corner>` lines in the circuit.",
          "items": {
            "type": "string"
          },
          "maxItems": 64,
          "type": "array"
        },
        "supplies": {
          "default": [],
          "description": "Supply values, for example `[{\"target\": \"VDD\", \"values\": [1.62,\n1.8, 1.98]}]`. At most 200 corners in all.",
          "items": {
            "description": "Values for one supply: a voltage source or a `.param`.",
            "properties": {
              "target": {
                "description": "A source (`VDD`) or `.param` name.",
                "type": "string"
              },
              "values": {
                "description": "Its values, as numbers or SPICE text (`\"1.8\"`), at most 64.",
                "items": {
                  "type": "number"
                },
                "maxItems": 64,
                "type": "array"
              }
            },
            "required": [
              "target",
              "values"
            ],
            "type": "object"
          },
          "maxItems": 8,
          "type": "array"
        },
        "temperatures": {
          "default": [],
          "description": "Temperatures in degrees Celsius.",
          "items": {
            "type": "number"
          },
          "maxItems": 64,
          "type": "array"
        }
      },
      "type": "object"
    },
    "goal": {
      "description": "Optionally also minimise or maximise one spec's value once all pass.",
      "properties": {
        "sense": {
          "enum": [
            "minimize",
            "maximize"
          ],
          "type": "string"
        },
        "spec": {
          "description": "Name of the spec row whose value to push.",
          "type": "string"
        },
        "weight": {
          "default": 0.01,
          "description": "Weight against the spec violations, which are squared fractions of\ntheir limits. Small weights keep the goal from trading a spec away.",
          "type": "number"
        }
      },
      "required": [
        "spec",
        "sense"
      ],
      "type": "object"
    },
    "max_evals": {
      "description": "Simulation budget in candidates (default 80, at most 400).\nCandidates times corners may be at most 2000.",
      "minimum": 0,
      "type": "integer"
    },
    "method": {
      "anyOf": [
        {
          "description": "Fast and reliable for a few parameters.",
          "enum": [
            "nelder_mead"
          ],
          "type": "string"
        },
        {
          "description": "Better for many parameters or rough objectives.",
          "enum": [
            "cma_es"
          ],
          "type": "string"
        }
      ]
    },
    "params": {
      "description": "Parameters to size: an element (`R1`) or `.param` name with `min` and\n`max` (`\"1k\"` style values accepted), optional `log_scale` (default true\nfor R, C, L) and `snap` to an E-series (E6, E12, E24, E48, E96) for the\nfinal answer.",
      "items": {
        "description": "A value to size.",
        "properties": {
          "initial": {
            "description": "Starting value; the middle of the range (in the search space) if\nabsent.",
            "type": "number"
          },
          "log_scale": {
            "description": "Search in log space. Defaults to yes for names starting with R, C or\nL, whose sensible ranges span decades.",
            "type": "boolean"
          },
          "max": {
            "type": "number"
          },
          "min": {
            "type": "number"
          },
          "name": {
            "description": "An element or `.param` name, as in a sweep.",
            "type": "string"
          },
          "snap": {
            "description": "Round the final answer to this series.",
            "enum": [
              "E6",
              "E12",
              "E24",
              "E48",
              "E96",
              null
            ],
            "type": "string"
          }
        },
        "required": [
          "name",
          "min",
          "max"
        ],
        "type": "object"
      },
      "type": "array"
    },
    "save_specs": {
      "default": true,
      "description": "Save the given specs to `<circuit>.specs`, as the circuit's\nrequirements from now on. Default true.",
      "type": "boolean"
    },
    "simulator": {
      "type": "string"
    },
    "specs": {
      "description": "Specs to meet; default: the `<circuit>.specs` file.",
      "type": "string"
    }
  },
  "required": [
    "circuit",
    "params"
  ],
  "type": "object"
}
```

</details>

## `templates`

Verified reference circuits to start a design from: passive and active filters, op-amp and transistor gain stages, voltage references, regulators and power switching, an oscillator, a logic level shifter and a photodiode front end. Every template is simulated in aispice's test suite and meets its own spec table. `list` shows the library (filter by category or by words), `show` gives a template's design equations, the parts usually changed, its spec table and the parts and nets of the schematic, and `use` copies it into the project as a new circuit with its specs saved beside it, ready for check_specs and then optimize. `use` refuses to overwrite existing files.

Edits your files: each write is approved in ask-before-applying mode and can be undone.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "action": {
      "anyOf": [
        {
          "description": "The library: id, title, category and a description of each.",
          "enum": [
            "list"
          ],
          "type": "string"
        },
        {
          "description": "One template in full: design equations, the parts to change, the\nspec table and the schematic.",
          "enum": [
            "show"
          ],
          "type": "string"
        },
        {
          "description": "Copy a template into the project as a new circuit, with its specs.",
          "enum": [
            "use"
          ],
          "type": "string"
        }
      ]
    },
    "category": {
      "description": "For `list`: only this category (filters, amplifiers, references,\npower, oscillators, digital, sensors).",
      "type": "string"
    },
    "circuit": {
      "description": "For `use`: path of the new schematic relative to the project folder,\nending in `.asc`. The spec table is written beside it as\n`<name>.specs`, where check_specs, optimize and monte_carlo find it.",
      "type": "string"
    },
    "id": {
      "description": "Template id for `show` and `use`, from `list`, e.g. `sallen_key_lowpass`.",
      "type": "string"
    },
    "query": {
      "description": "For `list`: words that must all appear in the id, title or\ndescription, e.g. `low-pass` or `photodiode`.",
      "type": "string"
    }
  },
  "required": [
    "action"
  ],
  "type": "object"
}
```

</details>

## `pdk`

Process design kits (PDKs) for transistor-level IC design. `list` shows the PDKs the user has registered and the one the open project uses. `show` describes one: its folder and commit, the compiled device models it loads, its corner libraries with their sections, and the model header aispice adds to every netlist (`.lib <corner library> mos_tt` and the like). `devices` lists the devices with the symbol to place (for example sg13g2\sg13_lv_nmos), the pins in order and the default parameters; set W, L, fingers (ng) and multiplier (m) through the SpiceLine attribute, e.g. `w=2u l=0.5u ng=2 m=1`. `use` makes the open project use a registered PDK by writing aispice.toml. Only the user can register a PDK (with `aispice pdk add`). A PDK project simulates on ngspice or Xyce; LTspice cannot load PDK models.

Edits your files: each write is approved in ask-before-applying mode and can be undone.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "action": {
      "anyOf": [
        {
          "description": "The registered PDKs, and the one the open project uses.",
          "enum": [
            "list"
          ],
          "type": "string"
        },
        {
          "description": "One PDK in full: folder, commit, compiled models, corner libraries\nwith their sections, and the model header aispice adds.",
          "enum": [
            "show"
          ],
          "type": "string"
        },
        {
          "description": "The PDK's devices: the symbol to place, pins, default parameters.",
          "enum": [
            "devices"
          ],
          "type": "string"
        },
        {
          "description": "Make the open project use this PDK (writes `aispice.toml`).",
          "enum": [
            "use"
          ],
          "type": "string"
        }
      ]
    },
    "name": {
      "description": "The registered name, e.g. `ihp-sg13g2`. Default for show and\ndevices: the project's PDK.",
      "type": "string"
    }
  },
  "required": [
    "action"
  ],
  "type": "object"
}
```

</details>

## `gm_id`

Size a PDK transistor with the gm/Id method. Give the device, the gm/Id you want (S/A), the channel length, and the drain current (or a width); get back W, the number of fingers, VGS, Vth, gm, gds, intrinsic gain gm/gds, Cgg and fT at that bias, and the SpiceLine to give the transistor (`w=... l=... ng=... m=1`). VDS defaults to half the supply. A lookup table made by sweeping the device on ngspice over L, VDS and VGS with the PDK's own models (typical corner) gives the first answer; the first use of a device builds its table (a few seconds) and later uses read it from the cache. The answer is then checked by simulating the device at that size and corrected, since the models do not scale exactly with width. Without gm_id and l, it reports what the table covers. Check the whole circuit by simulating it.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "device": {
      "description": "The MOS transistor, by subcircuit name: sg13_lv_nmos, sg13_lv_pmos,\nsg13_hv_nmos or sg13_hv_pmos for IHP SG13G2.",
      "type": "string"
    },
    "gm_id": {
      "description": "Target gm/Id in S/A (1/V): about 5 to 10 in strong inversion (fast,\nsmall), 10 to 18 in moderate inversion, 20 and up in weak inversion\n(efficient, slow). Leave out to see what the table covers.",
      "type": "number"
    },
    "id": {
      "anyOf": [
        {
          "type": "number"
        },
        {
          "type": "string"
        }
      ],
      "description": "The drain current to size the width for, e.g. \"50u\"."
    },
    "l": {
      "anyOf": [
        {
          "type": "number"
        },
        {
          "type": "string"
        }
      ],
      "description": "Channel length, e.g. \"0.5u\"."
    },
    "pdk": {
      "description": "A registered PDK. Default: the project's, or the only one registered.",
      "type": "string"
    },
    "refresh": {
      "default": false,
      "description": "Rebuild the table even when a cached one matches the PDK.",
      "type": "boolean"
    },
    "vds": {
      "anyOf": [
        {
          "type": "number"
        },
        {
          "type": "string"
        }
      ],
      "description": "Drain-source voltage magnitude. Default: half the device's supply."
    },
    "w": {
      "anyOf": [
        {
          "type": "number"
        },
        {
          "type": "string"
        }
      ],
      "description": "Or a width, to get the current it carries at this gm/Id."
    }
  },
  "required": [
    "device"
  ],
  "type": "object"
}
```

</details>

## `layout`

Analog layout on the project's PDK (IHP SG13G2), drawn with the PDK's own pcells. `generate` lays out a circuit's transistors with a generator: current_mirror (roles ref, out), diff_pair (pair), ota5t (pair, load, tail, bias), two_stage, a Miller op-amp (the ota5t roles plus cs, sink and cc, its cap_cmim capacitor, drawn as a MIM pcell with both plates brought down to the routing), or comparator, a StrongARM latch (tail, pair, latch_n, latch_p, reset); roles are found from the topology when not given. Each matched group becomes a row of two-finger units in a common-centroid or interdigitated pattern with dummy fingers at both ends and a guard ring; nets run on Metal3 tracks, joined across rows on Metal4, and pin nets are labelled. It draws the block, runs DRC and LVS, and when DRC fails on a rule a parameter addresses (routing spacing, wells, latch-up) it changes that parameter and draws again, up to `attempts` times; then it writes <cell>.gds and <cell>.layout.json (how it was made) and returns every remaining violation with a hint on which parameter to change. Parameters: pattern (common_centroid, interdigitated), dummies (0 to 4 per row end), guard_ring, fingers (of the smallest device in a row, even), track_pitch and row_gap in um. GDS is never edited by hand: when a check fails, change parameters or roles and generate again. `render` draws a layout as a PNG.

Edits your files: each write is approved in ask-before-applying mode and can be undone.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "action": {
      "anyOf": [
        {
          "description": "Lay out a circuit's transistors with a generator, then check it.",
          "enum": [
            "generate"
          ],
          "type": "string"
        },
        {
          "description": "Draw a layout as an image.",
          "enum": [
            "render"
          ],
          "type": "string"
        }
      ]
    },
    "attempts": {
      "description": "generate: how many times to draw it, changing the parameter that\nusually fixes a failed DRC between tries (default 3, at most 5).",
      "minimum": 0,
      "type": "integer"
    },
    "cell": {
      "description": "The cell name; default the circuit's file name.",
      "type": "string"
    },
    "check": {
      "default": true,
      "description": "generate: run DRC and LVS on the result (default true).",
      "type": "boolean"
    },
    "circuit": {
      "description": "generate: the circuit to lay out.",
      "type": "string"
    },
    "layout": {
      "description": "generate: the GDS to write, default `<cell>.gds` beside the circuit.\nrender: the GDS to draw.",
      "type": "string"
    },
    "params": {
      "description": "generate: pattern, dummies, guard_ring, fingers, track_pitch, row_gap.",
      "properties": {
        "dummies": {
          "default": 1,
          "description": "Dummy fingers at each end of every island (0 to 4, default 1).",
          "minimum": 0,
          "type": "integer"
        },
        "fingers": {
          "description": "Fingers of the smallest device in each row (even); the others get\nthe same finger width. By default the fewest that keep fingers at\nmost 2 um wide.",
          "minimum": 0,
          "type": "integer"
        },
        "guard_ring": {
          "default": true,
          "description": "A full guard ring around each row (default); false draws a tap bar\nunder the row only.",
          "type": "boolean"
        },
        "pattern": {
          "anyOf": [
            {
              "description": "`ABAB`: units alternate.",
              "enum": [
                "interdigitated"
              ],
              "type": "string"
            },
            {
              "description": "`ABBA`: units mirror about the row's centre.",
              "enum": [
                "common_centroid"
              ],
              "type": "string"
            }
          ],
          "default": "common_centroid",
          "description": "How matched devices are ordered in a row."
        },
        "pin_edge": {
          "default": false,
          "description": "Bring every pin to the block's bottom edge on a Metal4 column of its\nown, 1.2 um apart on a grid fixed to the block's origin, as a macro\npackage needs (default false: pins are labels on the Metal3 tracks).",
          "type": "boolean"
        },
        "row_gap": {
          "default": 1.0,
          "description": "Space between the routing channels of two rows, um (default 1).",
          "type": "number"
        },
        "track_pitch": {
          "default": 0.6,
          "description": "Centre-to-centre pitch of the routing tracks, um (default 0.6, at\nleast 0.51).",
          "type": "number"
        }
      },
      "type": "object"
    },
    "roles": {
      "additionalProperties": {
        "anyOf": [
          {
            "type": "string"
          },
          {
            "items": {
              "type": "string"
            },
            "type": "array"
          }
        ],
        "description": "A role's devices: one instance name or several."
      },
      "description": "generate: which instances play which part, e.g. {\"pair\": [\"M1\",\n\"M2\"], \"load\": [\"M3\", \"M4\"], \"tail\": \"M5\", \"bias\": \"M6\"}. Found from\nthe topology when omitted.",
      "type": "object"
    },
    "size": {
      "description": "render: image width and height in pixels (default 1200 x 900).",
      "items": {
        "minimum": 0,
        "type": "integer"
      },
      "maxItems": 2,
      "minItems": 2,
      "type": "array"
    },
    "template": {
      "description": "generate: current_mirror, diff_pair, ota5t, two_stage or comparator.",
      "oneOf": [
        {
          "description": "Roles `ref` (the diode-connected device) and `out` (one or more).",
          "enum": [
            "current_mirror"
          ],
          "type": "string"
        },
        {
          "description": "Role `pair` (two devices sharing a source).",
          "enum": [
            "diff_pair"
          ],
          "type": "string"
        },
        {
          "description": "Five-transistor OTA: roles `pair`, `load` (a mirror), `tail` and\noptionally `bias` (the diode that mirrors into the tail).",
          "enum": [
            "ota5t"
          ],
          "type": "string"
        },
        {
          "description": "StrongARM latch comparator: roles `tail` (the clocked foot), `pair`\n(the inputs), `latch_n` and `latch_p` (the cross-coupled pairs) and\n`reset` (the clocked precharge devices).",
          "enum": [
            "comparator"
          ],
          "type": "string"
        },
        {
          "description": "Two-stage Miller op-amp: the OTA's roles plus `cs` (the second\nstage), `sink` (its current source, mirrored from the tail) and `cc`\n(the cap_cmim compensation capacitor).",
          "enum": [
            "two_stage"
          ],
          "type": "string"
        }
      ]
    }
  },
  "required": [
    "action"
  ],
  "type": "object"
}
```

</details>

## `drc`

Design rule check of a layout (GDS) with the PDK's own KLayout rule decks: the main rules, IHP's extra rules and antenna rules by default. Each violation comes with its rule, the layer, a description and where it is (box in um). For a layout made by the layout tool, the report says which generator parameter usually fixes each rule family; change it and generate again. Never edit GDS by hand.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "antenna": {
      "default": true,
      "description": "Antenna rules (default true).",
      "type": "boolean"
    },
    "cell": {
      "description": "The cell to check; default the file's one top cell.",
      "type": "string"
    },
    "density": {
      "default": false,
      "description": "Density rules, which apply to a whole chip (default false).",
      "type": "boolean"
    },
    "extra": {
      "default": true,
      "description": "IHP's extra rules beyond the main set (default true).",
      "type": "boolean"
    },
    "layout": {
      "description": "The layout: a GDS file in the project.",
      "type": "string"
    }
  },
  "required": [
    "layout"
  ],
  "type": "object"
}
```

</details>

## `lvs`

Layout versus schematic with the PDK's KLayout LVS deck: compares the devices and nets extracted from a layout (GDS) with the circuit's PDK devices. For a layout the layout tool made, only the devices it drew are compared, with its dummy transistors added to the schematic side. Reports every net, device and pin that does not match, by its name in the schematic and in the layout. Pin nets must be labelled in the layout; the generators do that.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "cell": {
      "description": "The cell; default the file's one top cell.",
      "type": "string"
    },
    "circuit": {
      "description": "The circuit it should match: a schematic or netlist of the project.",
      "type": "string"
    },
    "layout": {
      "description": "The layout: a GDS file in the project.",
      "type": "string"
    }
  },
  "required": [
    "layout",
    "circuit"
  ],
  "type": "object"
}
```

</details>

## `extract`

Extract a layout's parasitics with Magic and the PDK's tech file: every transistor finger, coupling and ground capacitance and, with resistance, a resistor network per net. Returns the extracted subcircuit. To simulate it, run check_specs with post_layout, which puts the extracted layout in place of the schematic's devices and reports each spec before and after layout.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "cell": {
      "type": "string"
    },
    "layout": {
      "description": "The layout: a GDS file in the project.",
      "type": "string"
    },
    "resistance": {
      "default": false,
      "description": "Resistor networks as well as capacitance (default false).",
      "type": "boolean"
    }
  },
  "required": [
    "layout"
  ],
  "type": "object"
}
```

</details>

## `check_macro`

Check a macro package before a chip places it. A macro package (macro/v1) is the format aispice and ChipLeap use to hand hardened blocks to each other: macro.toml plus GDS, LEF, liberty, a Verilog model, a SPICE netlist, a measured spec table for analog blocks, and DRC and LVS evidence. The check covers macro.toml against its schema, the files each kind needs, every pin across the GDS, LEF, liberty, Verilog and SPICE views with its direction, the size, timing arcs of digital outputs, measured specs against their limits, and evidence that passes, matches the package files and names the same PDK. Returns every finding with its rule id, severity, file and message; the package passes when there are no errors. ChipLeap's `chipleap macro check` reports the same findings.

Read-only: writes nothing of yours, only aispice's own folders.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "path": {
      "description": "The package folder relative to the project folder, e.g.\n`macros/tiny_ota.macro`.",
      "type": "string"
    },
    "pdk": {
      "description": "The PDK of the chip that will place the block: `NAME` or\n`NAME@COMMIT`, e.g. `ihp-sg13g2`. A different PDK is an error, a\ndifferent commit a warning.",
      "type": "string"
    }
  },
  "required": [
    "path"
  ],
  "type": "object"
}
```

</details>

## `macro_export`

Export a generated block as a macro package (macro/v1), the format chips place blocks from: aispice's chip tool and ChipLeap both read it. The block is drawn again from its layout record with every pin brought to its bottom edge on Metal4 and ground (net 0) named as a pin, then checked with the PDK's DRC and LVS decks, extracted, and its specs measured at every corner before and after layout; every post-layout value must meet its limits. The package holds macro.toml (pins with directions and kinds, supply domain, corners, PDK and commit), the GDS, a LEF abstract from Magic, a liberty view with pin capacitance from extraction, a Verilog model (blackbox or wreal), the LVS netlist, the extracted netlist, the measured spec table, and DRC, LVS, extraction and simulation evidence with the hashes of the files they checked. It is checked with the same rules as check_macro before anything is written, then written into the project in one approved, undoable step. Needs a layout made by the layout tool.

Edits your files: each write is approved in ask-before-applying mode and can be undone.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "circuit": {
      "description": "The circuit the block comes from, with its test bench: a schematic or\nnetlist of the project.",
      "type": "string"
    },
    "corners": {
      "description": "The corners to characterize at, as for check_specs; default tt at\n27 C.",
      "properties": {
        "process": {
          "default": [],
          "description": "Process corners: presets `tt`, `ss`, `ff`, `sf`, `fs`, or\nfamily=corner pairs such as `mos=ss res=wcs` (families mos, res, cap,\ndio, hbt). Needs `.lib <file> <family>_<corner>` lines in the circuit.",
          "items": {
            "type": "string"
          },
          "maxItems": 64,
          "type": "array"
        },
        "supplies": {
          "default": [],
          "description": "Supply values, for example `[{\"target\": \"VDD\", \"values\": [1.62,\n1.8, 1.98]}]`. At most 200 corners in all.",
          "items": {
            "description": "Values for one supply: a voltage source or a `.param`.",
            "properties": {
              "target": {
                "description": "A source (`VDD`) or `.param` name.",
                "type": "string"
              },
              "values": {
                "description": "Its values, as numbers or SPICE text (`\"1.8\"`), at most 64.",
                "items": {
                  "type": "number"
                },
                "maxItems": 64,
                "type": "array"
              }
            },
            "required": [
              "target",
              "values"
            ],
            "type": "object"
          },
          "maxItems": 8,
          "type": "array"
        },
        "temperatures": {
          "default": [],
          "description": "Temperatures in degrees Celsius.",
          "items": {
            "type": "number"
          },
          "maxItems": 64,
          "type": "array"
        }
      },
      "type": "object"
    },
    "description": {
      "description": "A line about the block for `macro.toml`.",
      "type": "string"
    },
    "ground": {
      "description": "The name of the ground pin, which is net 0 in the circuit; default\n`vss`.",
      "type": "string"
    },
    "layout": {
      "description": "The block's layout from the layout tool: a GDS of the project with its\n`.layout.json` record beside it.",
      "type": "string"
    },
    "name": {
      "description": "The macro's name; default the layout's cell. Letters, digits and\nunderscores, starting with a letter.",
      "type": "string"
    },
    "out": {
      "description": "The package folder; default `macros/<name>.macro`.",
      "type": "string"
    },
    "pins": {
      "additionalProperties": {
        "description": "A change to what export found for a pin.",
        "properties": {
          "description": {
            "type": "string"
          },
          "direction": {
            "enum": [
              "input",
              "output",
              "inout",
              null
            ],
            "type": "string"
          },
          "kind": {
            "description": "signal, clock or analog. Supplies are found by name.",
            "enum": [
              "signal",
              "clock",
              "analog",
              null
            ],
            "type": "string"
          }
        },
        "type": "object"
      },
      "description": "Changes to the pins found from the circuit, by pin name, e.g.\n`{\"clk\": {\"kind\": \"clock\"}, \"out\": {\"description\": \"Output, 1 pF\"}}`.",
      "type": "object"
    },
    "resistance": {
      "default": false,
      "description": "Extract resistor networks as well as capacitance (default false).",
      "type": "boolean"
    },
    "simulator": {
      "type": "string"
    },
    "specs": {
      "description": "Specs to measure, one per line; default the circuit's `.specs` file.",
      "type": "string"
    },
    "verilog": {
      "description": "The Verilog model: `blackbox` (default) or `wreal`.",
      "enum": [
        "blackbox",
        "wreal",
        null
      ],
      "type": "string"
    },
    "voltage": {
      "description": "The block's supply in volts; default the circuit's supply source,\nelse 1.2.",
      "type": "number"
    }
  },
  "required": [
    "circuit",
    "layout"
  ],
  "type": "object"
}
```

</details>

## `chip`

Assemble an analog-on-top chip on the project's PDK (IHP SG13G2) from macro packages: aispice blocks exported with macro_export, and hardened digital blocks (ChipLeap packages). The chip description is a TOML file in the project: `name`, optional `pad_pitch_um` (default 100), `[[macros]]` with `instance`, `package` (a .macro folder of the project), optional `at_um = [x, y]` in the core and `nets = { pin = "net" }` (pins not listed join the net of their own name), and `[[pads]]` with `net`, `side` (north, east, south, west) and `kind`: analog (sg13g2_IOPadAnalog, its clamps and diodes; `resistor = true` connects through its secondary protection), input (sg13g2_IOPadIn), output (sg13g2_IOPadOut4mA), vdd, vss (core supplies), iovdd, iovss (the IO ring's supplies; at least one pad of each supply kind). Every package is checked with check_macro's rules against the project's PDK before use. `plan` gives the floorplan (die size, pad ring with corners and fillers, bond pads, seal ring, block placement) and the top-level routes (Metal3 and Metal4 on a 0.6 um grid, clean by construction) without running anything. `assemble` draws the chip with KLayout and IHP's pcells (bond pads, seal ring), adds Magic's metal and active fill, runs IHP's DRC decks (main, extra, antenna, density) on the finished chip and IHP's LVS deck on the chip without its pad cells against a netlist of the blocks with the pad nets as ports, then writes <name>.gds, <name>.lvs.spice, <name>.chip.json and DRC and LVS evidence into the output folder. When a block does not fit or a net cannot be routed, move blocks with at_um, change pad sides or pitch, and plan again.

Edits your files: each write is approved in ask-before-applying mode and can be undone.

<details><summary>Input schema</summary>

```json
{
  "properties": {
    "action": {
      "anyOf": [
        {
          "description": "Lay the chip out on paper: floorplan and routes, no files.",
          "enum": [
            "plan"
          ],
          "type": "string"
        },
        {
          "description": "Draw, fill and check the chip, and write it into the project.",
          "enum": [
            "assemble"
          ],
          "type": "string"
        }
      ]
    },
    "check": {
      "default": true,
      "description": "assemble: run full-chip DRC and LVS (default true).",
      "type": "boolean"
    },
    "out": {
      "description": "assemble: the folder the results go to; default `<name>.chip`\nbeside the description.",
      "type": "string"
    },
    "spec": {
      "description": "The chip description: a `.chip.toml` file of the project.",
      "type": "string"
    }
  },
  "required": [
    "action",
    "spec"
  ],
  "type": "object"
}
```

</details>

