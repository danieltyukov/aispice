# IC templates, IC evals and Spectre end to end (A1 part 3): plan

Spec: `docs/superpowers/specs/2026-10-11-ic-templates-design.md`. Branch `feat/ic-templates`. Each task is test-first and ends with fmt, clippy and the crate's tests (built and run on the offload server, never on the laptop), then a commit.

## Tasks

Status 2026-10-11: tasks 1 to 7 done; results under "As built" in the spec.

1. **Template metadata.** `pdk`, `corners`, `simulators`, `layout` on `Template`; the layout crate's role types; `templates show` and `use` print them; `use` refuses an IC template in a project without a PDK of its kind. Unit tests for the metadata rules and the role maps against the generator's planner.
2. **Templates.** The six `.asc` and `.toml` files, sized with the gm/Id tool and checked with `aispice check` at their corner sets on the offload server; every device under 10 um per finger.
3. **Gated template test.** `tests/templates_ihp.rs`: `templates use` into a PDK project, `check_specs` at the corner set on ngspice and, where listed, Xyce; margins as the existing template test. Skip IC templates in the generic all-templates test. Add the test to CI's PDK job.
4. **Eval PDK support.** `pdk` in `task.toml`, the registry in `EvalOptions` and in the judge, skipped runs for a missing PDK, the CLI passing the user's registry. Tests in `eval_suite.rs`.
5. **IC eval tasks.** `ic-opamp-corners`, `ic-comparator-ss`, `ic-opamp-noise`, with starting circuits, judges and reference solutions sized on the offload server.
6. **Spectre.** Run `dialect_sims` and the noise, PSRR and corner features on both server releases with the configuration in environment variables; fix what fails.
7. **Docs.** README, `docs/TOOLS.md` regenerated with `cargo run -p aispice -- docs tools`, `evals/README.md`, CHANGELOG, roadmap status in this plan.
8. **Checks, PR, CI green.**
