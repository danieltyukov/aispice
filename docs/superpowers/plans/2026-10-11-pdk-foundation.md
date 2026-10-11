# PDK foundation (A1, part 1): plan

Spec: `docs/superpowers/specs/2026-10-11-pdk-foundation-design.md`. Branch `feat/pdk-foundation`. Each task is test-first and ends with `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and the crate's tests, then a commit.

## Tasks

1. **Device catalog** (`aispice-core/src/pdk.rs`). `Family`, `Device`, `Kit`, `IHP_SG13G2`, `device()`, `kit()`. Tests: every device has pins and a known corner library; lookups ignore case.
2. **Policy** (`aispice-core/src/netlist/policy.rs`). `trusted_dirs`; relaxed `/` heuristic inside trusted files; compiled-library includes and `osdi` lines refused; `check_module`. Tests as listed in the spec.
3. **Symbols** (`aispice-core/symbols/sg13g2/*.asy`, `symbol/builtin.rs`). Eleven symbols; netlist, lint (catalog names count as defined) and render tests; `add_component` names parts with the catalog letter.
4. **PDK layout** (`aispice-sim/src/pdk.rs`). `Pdk::open` (validation, narrowing a checkout to `ihp-sg13g2`), `commit()`, `osdi_files()`, `xyce_plugins()`, `header_for(netlist)`, `retarget(netlist, Target)`, `defined_names()`. Tests on a fake tree.
5. **Backends** (`backend/mod.rs`, `ngspice.rs`, `xyce.rs`). `SimJob::pdk: Option<PdkRun>`; ngspice `.spiceinit` with `osdi` lines and native mode; Xyce `-plugin`. Unit tests on the generated init file and argv.
6. **Resolver** (`models.rs`). Names defined elsewhere are not looked up. Test.
7. **Registry and project config** (`aispice-tools/src/pdk.rs`). `Registry` load, save, add, remove; `ProjectConfig` from `aispice.toml`. Tests with temp dirs.
8. **Runner** (`runner.rs`, `setup.rs`, `aispice-agent/src/config.rs`). `RunnerConfig::{pdks, xyce_exe}`; `Runner::project_pdk`; header in `netlist_for`; PDK-aware `choose`, `deck_for` and policy; `SimJob::pdk`; errors for missing opt-in. `xyce_path` in `config.toml`. Unit tests with a fake PDK (header present, LTspice refused, modules checked).
9. **CLI** (`aispice pdk add|list|show|remove`). Integration test with `XDG_CONFIG_HOME` pointing at a temp dir and a fake PDK tree.
10. **Tool `pdk`** (`list`, `show`, `devices`, `use`). Agent tool tests.
11. **gm/Id** (`aispice-sim/src/gmid.rs`, tool `gm_id`, `aispice gmid`). Interpolation tests everywhere; table build and a sized device verified by simulation behind `AISPICE_PDK_IHP`.
12. **Gated simulation tests** (`aispice-tools/tests/pdk_ihp.rs`). Inverter DC and 5T OTA AC from `.asc` files drawn with the new symbols, ngspice against Xyce.
13. **CI job** (`.github/workflows/ci.yml`, job `pdk`).
14. **Docs.** README feature list, `docs/SETUP.md` (registering a PDK, `aispice.toml`, OSDI and Xyce plugins), `docs/ARCHITECTURE.md` (security model), `docs/TOOLS.md` regenerated, CHANGELOG under Unreleased.
15. **Full checks.** `npm ci`, `npm run check`, the gated tests with `AISPICE_PDK_IHP=/home/danieltyukov/.local/share/pdk/IHP-Open-PDK/ihp-sg13g2`, push, PR, CI green.

## Local tool setup (outside the repo)

A plugin-capable Xyce 7.10 and ADMS 2.3.7 are built under `~/.local/xyce-plugin` against the existing Trilinos in `~/.local/trilinos-xyce` (script `~/.local/src/xyce-plugin-build.sh`), and IHP's Verilog-A models are compiled into `ihp-sg13g2/libs.tech/xyce/plugins` the way IHP's `adms-compile-va.sh` does. `/usr/local/bin/Xyce` is left as it is. The gated tests use it through `AISPICE_XYCE=~/.local/xyce-plugin/bin/Xyce`.

## Measured agreement (task 12, 2026-10-11)

ngspice 42 with OSDI from OpenVAF 23.5.0, against Xyce 7.10 (shared build) with ADMS 2.3.7 plugins, IHP-Open-PDK `22f2a25`:

| Circuit | Quantity | ngspice | Xyce |
|---|---|---|---|
| Inverter (W 1u/2u, L 0.13u, VDD 1.2 V) | switching threshold | 0.6183 V | 0.6183 V |
| | largest V(out) difference over the sweep | 0.26 mV | |
| 5T OTA (20 uA tail, 150 fF MIM load) | DC gain | 25.060 dB | 25.060 dB |
| | unity-gain frequency | 113.725 MHz | 113.725 MHz |

The `mosvar` plugin does not build with ADMS 2.3.7 (the MOS varactor only); PSP 103 and R3 do.
