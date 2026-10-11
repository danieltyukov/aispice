# Macro package contract (M0): plan

Design: `docs/superpowers/specs/2026-10-11-macro-package-design.md` (in ChipLeap; aispice has a pointer at the same path). Format: `docs/macro-package.md`. Two branches, `feat/macro-package` in chipleap and in aispice, one pull request each, linked to each other.

## Shared contract (written once in chipleap, copied to aispice)

1. Write `docs/macro-package.md`: layout, required files per kind, `macro.toml` fields, the rules for each view, the spec table, evidence, `--pdk`, the rule table, the report format, versioning, the fixtures.
2. Write `src/chipleap/macro/macro-v1.schema.json`: the manifest at the root, `$defs/evidence`, `$defs/openada_result`, `$defs/specs_file`.
3. Write `tests/fixtures/macro/generate.py`: a GDSII writer (standard library only), the analog macro `tiny_ota` (five-transistor OTA, 12 x 8 um, three corners, a spec table), the digital macro `tiny_counter` (2-bit counter, a bus, a second liberty corner, a path and an array reference in the GDS), evidence with real hashes, and 18 variants, each with one thing wrong or one edge case. It ends by writing `SHA256SUMS`.
4. Check the GDS files with KLayout: they load, the top cells, bounding boxes and labels are as intended.

## chipleap

5. `chipleap.macro.text`: `%.6g`, quoting, plurals, value descriptions.
6. `chipleap.macro.schema`: load the schema, the interpreter with its fixed keyword order and message templates.
7. `chipleap.macro.manifest`: dataclasses, `from_dict`, `to_dict`, `to_toml` (a small TOML writer), `load_manifest`.
8. `chipleap.macro.gds`: the record reader, hierarchy boxes, top cells, labels. Compare boxes with KLayout on rotated, mirrored, arrayed, nested references and paths.
9. `chipleap.macro.views`: LEF, liberty, Verilog and SPICE readers.
10. `chipleap.macro.check`: the rules in the documented order; `Report` with text and JSON forms.
11. `chipleap.macro.evidence`: `make_evidence`, `write_evidence`.
12. Run the validator over every fixture, review each finding by hand, write `tests/fixtures/macro/expected.json`, regenerate `SHA256SUMS`.
13. `chipleap macro check` and `chipleap macro schema`; the `check_macro` MCP tool (logged, not replayed: it reads files).
14. `tests/test_macro.py`: every case of `expected.json`; `SHA256SUMS` against the files; the pinned contract hashes; the generator reproduces the fixtures; the interpreter against the `jsonschema` package; reader unit tests; manifest round trip; evidence writing; CLI exit codes; the MCP tool.
15. README, `docs/mcp.md`, CONTRIBUTING (the shared files change together), CHANGELOG under Unreleased.
16. `ruff check`, `ruff format --check`, `mypy`, `pytest`.

## aispice

17. Copy the spec, the schema (to `crates/aispice-core/src/macropkg/`) and the fixtures (to `crates/aispice-core/tests/fixtures/macro/`).
18. `aispice_core::macropkg`: `text`, `schema` (the same interpreter over its own value type, so TOML dates stay distinct), `manifest` (serde), `gds`, `lef`, `liberty`, `verilog`, `spice`, `evidence`, `check`.
19. `crates/aispice-core/tests/macro_contract.rs`: every case of `expected.json`, `SHA256SUMS`, the pinned hashes, readers on the same inputs as ChipLeap's unit tests.
20. `aispice macro check` and `aispice macro schema`; the `check_macro` tool in the registry (agent and MCP), confined to the project folder; the tool title in the app.
21. Regenerate `docs/TOOLS.md`; README, ARCHITECTURE, CONTRIBUTING, CHANGELOG; a pointer design spec and this plan.
22. `npm ci`, `npm run check`.

## Both

23. Push, open the two pull requests with plain descriptions linking each other, watch CI, fix failures, stop when CI is green.
