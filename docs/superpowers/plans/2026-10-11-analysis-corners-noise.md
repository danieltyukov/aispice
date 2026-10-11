# Plan: noise, PSRR and CMRR, corners, mismatch Monte Carlo

Spec: `docs/superpowers/specs/2026-10-11-analysis-corners-noise-design.md`. Each task is test first, then code, then `cargo test` for the crate, then a commit.

## 1. Noise data

1. `raw.rs`: rename ngspice `onoise_spectrum`/`inoise_spectrum` and Spectre `out`/`in` (`V/sqrt(Hz)`) to `V(onoise)`/`V(inoise)` in noise plots; `current-density` and `A/sqrt(Hz)` make `V(inoise)` a current. Unit tests with handwritten raw headers.
2. `noise.rs`: `read_xyce_noise_dat` (Tecplot text, squared densities), `input_is_current(deck)`, `integrate_rms` (power-law segments). Unit tests.
3. Xyce backend reads `<stem>.cir_noise.dat` and keeps it; LTspice backend marks a current input. Simulator tests in `tests/noise_sims.rs`: the divider (4kTR within 2 percent, input twice the output, integrated noise) on every simulator present; the common-source stage against the hand calculation within 10 percent.
4. `measure.rs`: `Domain::Noise`; AC measurements skip noise datasets; kinds `output_noise_at`, `input_noise_at`, `output_noise_rms`, `input_noise_rms` with parser signatures, units and display. Unit tests on synthetic noise datasets.

## 2. PSRR and CMRR

1. `stimulus.rs`: `ac_on` (remove every AC stimulus, add `AC 1` on one source, refuse unknown or non-source names), `needed_sources`, `mark`, `marked_source`. Unit tests on source lines with `DC`, `AC mag phase`, `SINE(...)`, `Rser=`.
2. `measure.rs`: `psrr_db_at` and `cmrr_db_at` read the main AC dataset and the marked one; other measurements never pick a marked dataset. Unit tests with synthetic datasets.
3. Tools: one helper runs a netlist plus its derived runs and merges the datasets; simulate, measure, check_specs, sweep, monte_carlo, optimize and the judge use it. Tool test on the controlled-source circuits (80 dB each).

## 3. Corners

1. `corner.rs`: `parse_section`, `lib_sections`, presets, `CornerSet::corners` (naming, order, limit), `apply` (sections, `.temp`, supplies), `variants`. Unit tests.
2. Dialect: Xyce `.temp T` to `.options device temp=T`. Unit test, and a Xyce run of a diode at two temperatures.
3. `expand_lib_sections` and its use in the runner for ngspice and LTspice, with the policy check after it. Unit tests and an ngspice run of a toy section library.
4. `summarize` and `worst_case` into `CornerReport`. Unit tests.
5. `optimize.rs`: `Objective::score_worst`. Unit test.
6. Tools: `corners` on check_specs, optimize and monte_carlo; text and data output. Tool tests on a toy library: worst corner named, optimize meets specs at every corner.
7. CLI: `aispice check --process --temp --supply`.

## 4. Mismatch Monte Carlo

1. `montecarlo.rs`: `Mismatch`, `statistical` (section switch with notes for missing variants), `sim_seed`, `variants_with`, `Variant.seed`. Unit tests (stable seeds, prefix property, tolerances unchanged).
2. Dialect: ngspice keeps `seed`; Xyce turns it into `.sampling` plus sample options; policy allows `.sampling`. Unit tests.
3. Tools: `mismatch` and `corners` on monte_carlo, refusing LTspice and Spectre for mismatch. Simulator tests: divider spread on ngspice and Xyce, reproducibility.

## 5. Agent surface

1. Tool descriptions, `MEASURE_SYNTAX`, `SPEC_SYNTAX`; the help test covers the new kinds.
2. Judge: derived runs and `corners` in `task.toml`. Evals `noise-divider` and `temp-corners` with solutions; `eval_suite` passes.
3. `docs/TOOLS.md` regenerated; README, CHANGELOG, ARCHITECTURE.
4. `npm run check`; optional Spectre run of the noise divider; PR.
