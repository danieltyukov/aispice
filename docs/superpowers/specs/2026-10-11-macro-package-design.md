# Macro package contract (M0): design

Date: 2026-10-11. Status: implemented in aispice and ChipLeap on `feat/macro-package`.

The design record for this milestone lives in ChipLeap, which holds the canonical copy of the contract: [docs/superpowers/specs/2026-10-11-macro-package-design.md](https://github.com/danieltyukov/chipleap/blob/main/docs/superpowers/specs/2026-10-11-macro-package-design.md). It covers why the package is the only coupling between the tools, the layout, the evidence format (mirroring OpenADA's result envelope), why both tools carry a small JSON Schema interpreter instead of a library, the shared fixtures and how drift is caught. The format itself is [docs/macro-package.md](../../macro-package.md), identical in both repositories.

## What is specific to aispice

- **Where it lives.** `aispice_core::macropkg`, because checking a package is pure computation over files, which is what `aispice-core` is for. It adds three dependencies that were already in the lock file: `toml` (the same major version `aispice-tools` uses), `regex` (schema patterns) and `sha2` (evidence hashes).
- **One value type for TOML and JSON.** The schema interpreter runs on its own `Val` enum, converted from `toml::Value` and `serde_json::Value`, so TOML dates stay a distinct type that fails every type check, as they do in Python. Object keys are kept in a `BTreeMap`, whose order is the sorted order the interpreter's message order relies on.
- **TOML errors.** The toml crate's message carries no position, so the validator adds the line and column the way Python's `tomllib` words them. Only the text after `not valid TOML: ` differs between the tools, as the spec allows.
- **ASCII readers.** The LEF, liberty, Verilog and SPICE readers work on characters, treat only ASCII whitespace as whitespace and fold only ASCII case, matching ChipLeap's readers exactly.
- **The tool stays in the project.** `check_macro` resolves its path through `Project::resolve` and refuses a package whose files lead outside the project through symbolic links, since the check reads them and a parser message can quote a token from a file.
- **Parity checked beyond the fixtures.** During development a differential fuzzer mutated the fixtures 4,900 times (text edits, token swaps, numbers, truncation, stray Unicode, GDS byte flips, deleted files) and compared both validators' JSON reports: no differences. For the hardening it gained mutations aimed at the limits (nesting, numbers at the edges of their range, TOML 1.1 syntax, huge buses, generated GDS hierarchies with cycles, huge arrays, deep chains and magnified references); after fixing the six differences it found, 20,000 cases showed none. It is not part of CI, since it needs both repositories.
- **Limits and no panics.** `macropkg::Limits` holds the limits the spec lists; `CheckOptions` carries them so tests can lower them. Every reader returns an error instead of panicking on malformed input, the GDS hierarchy is checked without recursion before anything recurses, and a seeded random-bytes test per format (`tests/macro_limits.rs`) runs the readers and the whole check. The hardening is described in ChipLeap's design record.
- **TOML 1.0.** The toml crate reads TOML 1.1, so `macropkg::toml10` refuses what TOML 1.1 added (the `\e` and `\xHH` escapes, line breaks and trailing commas in inline tables, times without seconds) before parsing, as Python's `tomllib` does.

The plan is [docs/superpowers/plans/2026-10-11-macro-package.md](../plans/2026-10-11-macro-package.md).
