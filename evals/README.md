# Design task suite

These are the tasks behind `aispice eval`. Each one is a small circuit design job of the kind a user would hand to the agent: change a value to hit a corner frequency, fix a broken circuit, design a filter from scratch, size a Sallen-Key, bias a transistor, run a Monte Carlo analysis, or explain a circuit. The agent works on each task in a fresh project, and a judge then checks the result by simulating it. What the model says it did does not count; only the circuit and, for explanation tasks, the answer are judged.

## Running the suite

```sh
aispice eval --provider anthropic --model claude-opus-5-5
```

The command runs every task once per model, prints a line as each run finishes, then prints a table per model with each task's result and a summary line per model: pass rate, median steps, median tokens, median time and errors.

Options:

| Option | What it does |
|---|---|
| `--suite DIR` | Folder of tasks. Default `evals`, so run it from the repository root or pass the path. |
| `--task ID ...` | Run only these tasks. |
| `--provider P` | Provider, as for `aispice chat`. Default: the one in your config. |
| `--model M ...` | One or more models from that provider. Default: the configured model. |
| `--repeat N` | Run each task N times per model, to see how consistent a model is. |
| `--jobs N` | Run N tasks at once for a model. Default 1. |
| `--rpm N` | Send at most N model requests per minute. Use it on free tiers. |
| `--timeout S` | Time limit for one task run in seconds. Default 900. |
| `--json` | Print the full report as JSON instead of tables. |
| `--out FILE` | Also write the full report as JSON to a file. |
| `--min-pass-rate X` | Exit with code 1 if any model's pass rate is below X (a fraction, 0.8 for 80%). |

The exit code is 0 whatever the results, because this is a benchmark and not a test. It is 2 if the runner itself fails (no ngspice, no API key, a broken task file). Add `--min-pass-rate` to make CI fail on a regression.

Rate limits: the providers already retry a rate-limited request a few times. During a suite run aispice waits longer on top of that (up to ten minutes per request, using the wait the provider suggests when it gives one), and `--rpm` spaces requests out so the limit is not hit in the first place. If a model's runs fail with a provider error three times in a row, its remaining tasks are skipped and shown as such.

For a free tier with about 5 requests per minute:

```sh
aispice eval --provider google --model gemini-3.1-flash-lite --jobs 1 --rpm 5 --out report.json
```

## What happens in one run

1. The task's `files/` folder is copied into a new temporary project named after the task. A task without `files/` starts from an empty project. A task that names a `pdk` gets an `aispice.toml` that opts the project into a registered PDK of that kind (`aispice pdk add`); where none is registered the task is skipped, not failed, and the report says why.
2. The agent gets the task prompt as the user's message, the usual system prompt and tools, and the task's step budget (the number of model calls it may make). Edits are applied at once; nobody is asked to approve them.
3. The project uses aispice's built-in symbols and embedded models only (the `embedded_models_only` runner setting), even when LTspice is installed, so results compare across machines. The agent's simulations and the judge's run on ngspice.
4. The judge then:
   - simulates the judged circuit on ngspice, with the task's analysis directive in place of the circuit's own when the task gives one, and evaluates the spec table; with `corners` (the corner set `check_specs` takes) it simulates every corner and judges each spec at its worst one;
   - runs the electrical rule checks; any lint error fails the task;
   - checks each constraint.

A run fails if it used up its step budget, hit a provider error or the time limit, if any spec fails, if lint reports an error, or if any constraint fails. The report gives every reason, for example `spec fc: 4.1kHz (9.5kHz..10.5kHz) FAIL, margin -57%`.

## Task format

One folder per task. The folder name is the task id.

```
evals/
  rc-corner/
    task.toml        what to ask, what to judge
    solution.json    a reference solution, used by the tests
    files/           the starting project (optional)
      rc.asc
```

`task.toml`:

```toml
id = "rc-corner"                 # must match the folder name
title = "Move an RC low-pass corner to 10 kHz"
difficulty = "easy"              # easy, medium or hard
prompt = """
The RC low-pass filter in rc.asc has its -3 dB corner near 1.6 kHz. ...
"""
circuit = "rc.asc"               # the file the result is judged on
analysis = ".ac dec 50 10 1Meg"  # optional: the analysis the judge runs
corners = { temperatures = [-40, 27, 125] }  # optional: specs must hold at every corner
pdk = "ihp-sg13g2"               # optional: the kind of PDK the circuits are drawn in
specs = """
fc = bandwidth_3db(V(out)) in 9.5k..10.5k
"""
max_steps = 8                    # model calls the agent may make
lint = true                      # optional, default true: lint errors fail
constraints = [
  { kind = "value", name = "C1", equals = "100n" },
  { kind = "e_series", prefix = "R", series = "E24" },
]
```

The prompt is written the way a user would type it. It must name the judged file and anything the specs depend on, such as net names (`out`) or the name of a source used in a measurement. The spec table uses the same format as `check_specs` and `.specs` files: one spec per line, `name = measurement op limit`. It is never shown to the model; the model only sees the prompt. A task with no specs (an explanation task) is judged on its constraints alone.

Constraints:

| Kind | Passes when |
|---|---|
| `part_exists` `name` | a component with that instance name exists |
| `max_parts` `count` | the circuit has at most that many components |
| `part_count` `symbol` or `prefix`, `min` `max` | the number of parts drawn with that symbol (`OpAmps/opamp`), or with that device letter (`C` counts `cap` and `polcap` alike), is in range |
| `value` `name` `equals` `tol_pct` | that component's value equals `equals` as a number (default tolerance 0.1%) |
| `value_range` `prefix` `min` `max` | every part with that device letter (R, C, L) has a value in range |
| `e_series` `prefix` `series` | every part with that device letter has a standard value of the series (E6, E12, E24, E48, E96) |
| `lint_clean` | lint reports no warnings either |
| `files_unchanged` | every starting file is unchanged and no new circuit file was created (a saved `.specs` file is fine) |
| `tool_called` `name` | the model ran that tool at least once without an error |
| `answer_mentions` `any` | the model's text contains at least one of the phrases, ignoring case |
| `answer_number` `value` `tol_pct` | the model's text contains a number within `tol_pct` of `value`; `1.03 kHz`, `1026 Hz` and `1.03k` all count |
| `answer_reports` `tool` `pointer` `tol` | the model's text contains the number a tool reported, read from the tool's structured output at a JSON pointer such as `/yield_pct` |
| `subckt_param` `subckt` `param` `min` `max` | every instance of that subcircuit sets the parameter within limits, for example the op-amp's `GBW` |

`solution.json` is a reference solution: the turns a model could take, each either a set of tool calls or the final answer.

```json
[
  {"tools": [{"name": "read_schematic", "input": {"circuit": "rc.asc"}}]},
  {"tools": [{"name": "edit_schematic", "input": {"circuit": "rc.asc", "edits": [{"op": "set_value", "name": "R1", "value": "160"}]}}]},
  {"answer": "R1 is now 160 ohm, so the corner is 9.95 kHz."}
]
```

The test suite plays every reference solution through the real tools and judge (`cargo test -p aispice-tools --test eval_suite`), which proves each task can be solved with ngspice and the built-in symbols. It also checks that a model which does nothing fails every task, and that every task file, starting circuit and spec table is valid. The IC tasks need IHP SG13G2: with `AISPICE_PDK_IHP` pointing at its `ihp-sg13g2` folder their reference solutions are played too, and without it they are checked to be skipped.

## Adding a task

1. Make a folder named after the task, with `task.toml` and, if the task starts from a circuit, `files/`. Build starting circuits with aispice itself (`create_schematic` through `aispice chat` or the MCP server) so they parse and netlist cleanly.
2. Write the specs so that only a correct design passes. Give the judge its own `analysis` when the result depends on one.
3. Write `solution.json` and run `cargo test -p aispice-tools --test eval_suite`.

## The tasks

| Task | Level | What the agent has to do |
|---|---|---|
| `rc-corner` | easy | Change R1 so an RC low-pass corner moves from 1.6 kHz to 10 kHz, with an E24 value. |
| `floating-node` | easy | Find and fix the unconnected resistor that leaves a 10 V divider reading 10 V instead of 5 V. |
| `explain-filter` | easy | Say what kind of filter a circuit is and its -3 dB frequency, without changing anything. |
| `rise-time` | easy | Change C1 so an RC step response has a 50 us rise time. |
| `diode-polarity` | medium | Fix a half-wave rectifier whose diode is in backwards. |
| `ce-bypass` | medium | Add the missing emitter bypass capacitor so a common-emitter stage reaches 30 dB. |
| `rc-design` | medium | Design an RC low-pass from scratch: 2 kHz corner, E24 and E12 values, named nets. |
| `inverting-amp` | medium | Design an inverting amplifier with a gain of -10 and a 10 kOhm input impedance. |
| `ce-bias` | medium | Choose R1 so a saturated common-emitter stage has 6 V on its collector. |
| `optimize-two-specs` | medium | Use the optimize tool to size two resistors for an attenuation and a corner at once. |
| `monte-carlo-yield` | medium | Run a Monte Carlo analysis with given tolerances and report the yield it finds. |
| `sallen-key` | hard | Resize a Sallen-Key low-pass for a Butterworth response with a 1 kHz cutoff. |
| `gain-bandwidth` | hard | Reach 26 dB and 100 kHz with op-amps whose GBW is 1 MHz, which takes two stages. |
| `noise-tia` | medium | Choose a front-end resistor that meets an input noise current and an output noise voltage budget, proved with a noise analysis. |
| `temp-corners` | medium | Choose C1 so an RC corner stays in band from -40 C to 125 C with a drifting resistor; judged at every temperature. |
| `ic-comparator-ss` | medium | IHP SG13G2: resize a StrongARM comparator that decides in time at tt but not at ss, so it does at all 15 process and temperature corners. |
| `ic-opamp-noise` | medium | IHP SG13G2: find the input-referred noise density of a two-stage op-amp at 1 kHz and 1 MHz with a noise analysis, without changing it. |
| `ic-opamp-corners` | hard | IHP SG13G2: size a two-stage Miller op-amp for 50 dB, 20 MHz and 60 degrees of phase margin at every process corner from -40 C to 125 C. |
