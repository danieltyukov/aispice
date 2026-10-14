# Contributing to aispice

Thanks for helping. Bug reports, circuits that aispice gets wrong, new tools, simulator fixes and documentation are all welcome.

## Before you start

- For a bug, open an issue with the output of `aispice doctor` and, if you can, the smallest `.asc` file that shows the problem.
- For a new feature or a larger change, open an issue first so we can agree on the approach before you spend time on it.
- For a security problem, do not open an issue. Follow [SECURITY.md](SECURITY.md).

## Setting up

You need Rust 1.88 or newer, Node 22, and ngspice. [docs/SETUP.md](docs/SETUP.md) has the details, including the WebKitGTK packages the desktop app needs on Linux.

```sh
git clone https://github.com/danieltyukov/aispice
cd aispice
npm ci
cargo build -p aispice
```

[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) explains how the crates fit together.

## Checks

```sh
npm run check
```

runs what CI runs for the engine and the interface: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, then the interface typecheck, lint and unit tests, and the site build. For the desktop shell, also run `cargo clippy -p aispice-app --all-targets -- -D warnings`.

Other test suites:

| Command | What it covers |
|---|---|
| `npx playwright test` (in `app/`) | The interface in a browser against the mock backend, plus screenshots |
| `npm run test:desktop -w app` | The built desktop app over WebDriver. Build first with `npx tauri build --debug --no-bundle` in `app/`, and run under `xvfb-run -a` on a machine without a display |
| `AISPICE_LTSPICE_TESTS=1 cargo test -p aispice-sim` | Netlists compared live against LTspice, when it is installed |
| `AISPICE_SPECTRE_HOST=... AISPICE_SPECTRE_DIR=... cargo test -p aispice-sim --test dialect_sims --test analysis_sims --test noise_sims` | The same decks on a remote Spectre, compared with ngspice and Xyce: `.meas` results, corners, PSRR and noise. `AISPICE_SPECTRE_SETUP` sets a command that puts Spectre on the path. A test binary built elsewhere runs where your SSH keys are with `CARGO_MANIFEST_DIR` set to the crate's folder |
| `AISPICE_PDK_IHP=... cargo test -p aispice-tools --test templates_ihp --test eval_suite` | The IC templates at every corner of their sets (on Xyce too with `AISPICE_XYCE` set to a plugin-capable Xyce) and the IC tasks' reference solutions |

## Writing changes

- **Tests.** New behaviour needs a test that runs offline. Tests that need a simulator other than ngspice must skip cleanly when it is missing; look at the existing tests in `crates/aispice-sim/tests/` for the pattern.
- **Temporary folders.** A test makes its folders with `tempfile` (a `TempDir`, which removes the folder when dropped, also when the test fails), never by joining a name to `std::env::temp_dir()`. CI runs the tests with a temporary folder of their own and fails when an `aispice-*` or `.tmp*` folder is left in it. `AISPICE_KEEP=1` keeps a test's run folder where a test offers it, to inspect.
- **Netlister changes.** Compare against LTspice's own output. Add a schematic and its LTspice netlist to `crates/aispice-sim/tests/schematics/` when you fix a case.
- **Tools.** A tool returns text the model reads and, where the interface shows something, structured data. Keep the description short and concrete. After changing tools, regenerate the reference with `cargo run -p aispice -- docs tools > docs/TOOLS.md`.
- **Security.** Paths go through `Project::resolve`. Processes are started with argument vectors, never a shell string. Anything that writes a file goes through `Workspace::approve`. Treat circuit files, logs and model output as untrusted.
- **Style.** Follow the code around you. `cargo fmt` and the ESLint config settle formatting. Comments explain why, not what.
- **Macro packages.** The format is shared with [ChipLeap](https://github.com/danieltyukov/chipleap), whose copies are canonical. `docs/macro-package.md`, `crates/aispice-core/src/macropkg/macro-v1.schema.json` and `crates/aispice-core/tests/fixtures/macro/` are identical to ChipLeap's `docs/macro-package.md`, `src/chipleap/macro/macro-v1.schema.json` and `tests/fixtures/macro/`, and both validators must report what `expected.json` there says. Change them together: make the change in ChipLeap (its `generate.py` writes the fixtures), copy the three here, make the same change to `aispice_core::macropkg`, and update the pinned hashes in `crates/aispice-core/tests/macro_contract.rs` and ChipLeap's `tests/test_macro.py`.
- **Commits.** Small, focused commits with messages in the conventional style (`fix(core): ...`, `feat(tools): ...`). Update `CHANGELOG.md` under "Unreleased" when behaviour changes.

## What not to commit

- API keys or tokens of any kind.
- Vendor model libraries, process design kits, or anything under a non-disclosure agreement. Simulator tests use generic models only.
- Generated files: `target/`, `node_modules/`, `dist/`, `app/src-tauri/gen/`, `.aispice/` folders.

## Pull requests

Open the pull request against `master` and fill in the template. CI must pass before review. A maintainer merges with a squash or rebase, since the history is kept linear.

By contributing you agree that your work is released under the [MIT License](LICENSE), and that you follow the [Code of Conduct](CODE_OF_CONDUCT.md).
