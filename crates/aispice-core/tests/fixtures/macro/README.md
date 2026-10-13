# Macro package fixtures (macro/v1)

Shared by ChipLeap (`tests/fixtures/macro/`) and aispice (`crates/aispice-core/tests/fixtures/macro/`). The two copies are identical; change them together with `docs/macro-package.md` and the schema, in both repositories.

| Path | What it is |
|---|---|
| `tiny_ota.macro/` | An analog macro: a five-transistor OTA, 12 x 8 um |
| `tiny_counter.macro/` | A digital macro: a 2-bit counter with enable and active-low reset, 20 x 10 um, with a second liberty corner |
| `broken/<case>/` | Variants with one thing wrong (or one edge case), named after it |
| `gds/` | Layouts for the GDS readers' own tests: rotated, mirrored, arrayed and nested references, paths, and a cell that contains itself |
| `pdk/` | A stand-in for the libraries of IHP SG13G2 (`ihp-sg13g2/libs.ref`), given as the PDK's libraries to the cases that name it as `pdk_libs` |
| `expected.json` | What every validator must report for each case, finding by finding |
| `generate.py` | Writes both macros, the variants and `SHA256SUMS` (standard library only) |
| `SHA256SUMS` | The SHA-256 of every file here, checked by both repositories' tests |

Everything is synthetic and written for these tests. The GDS files hold a few rectangles on IHP SG13G2 layer numbers taken from the PDK's layer table (Activ 1/0, GatPoly 5/0, Metal1 8/0, 8/2 and 8/25, Metal2 10/0, 10/2 and 10/25, prBoundary 189/4). The LEF, liberty, Verilog and SPICE files are hand-written, the spec values are invented, and the evidence says that no check ran. The digital netlist instantiates IHP standard cells by name, as a real hardened block's netlist does, without defining them. No PDK file is copied: the stand-in libraries under `pdk/` hold made-up cells, a few rectangles each, under the names of IHP cells: an inverter `sg13g2_inv_1` and a buffer `sg13g2_buf_1` that places it in the standard-cell library (GDS, LEF and CDL), and a corner `sg13g2_Corner` in the IO library (GDS). The `std-cell-*` cases place copies of them, some changed.

To change a fixture, edit `generate.py`, run it, update `expected.json` by hand when the findings change, run it again so `SHA256SUMS` covers the new `expected.json`, then copy the folder to the other repository and update the pinned hash in both.

```sh
python tests/fixtures/macro/generate.py
```
