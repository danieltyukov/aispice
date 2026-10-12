//! Tools that simulate, measure, check specs and size circuits.

use super::analysis::{self, run_variants};
use super::schematic_tools::spec;
use crate::runner::{RunMods, StoredRun};
use crate::workspace::Workspace;
use aispice_agent::tool::parse_input;
use aispice_agent::{Tool, ToolContext, ToolOutput, ToolSpec};
use aispice_core::edit::{EditOp, apply};
use aispice_core::netlist::{Line, Netlist};
use aispice_core::units;
use aispice_sim::backend::SimId;
use aispice_sim::corner::{self, CornerSet};
use aispice_sim::dataset::{AnalysisKind, Dataset, Quantity};
use aispice_sim::measure::{Measure, MeasureResult, measure};
use aispice_sim::montecarlo::{self, Mismatch, Tolerance};
use aispice_sim::oppoint;
use aispice_sim::optimize::{
    self, CmaEs, CmaEsOptions, Goal, NelderMead, NelderMeadOptions, Objective, Param, RunOptions,
};
use aispice_sim::plot::{PlotRequest, plot_svg};
use aispice_sim::polezero::{PoleZero, Root, Stability};
use aispice_sim::spec::{Spec, SpecReport, evaluate, parse_specs};
use aispice_sim::sweep::{self, SweepParam};
use async_trait::async_trait;
use base64::Engine as _;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

const MEASURE_SYNTAX: &str = "Measurements are written `name = kind(args)`, for example `f3db = bandwidth_3db(V(out))`, `gain = gain_db_at(V(out)/V(in), 1k)`, `pm = phase_margin(V(out))`, `tr = rise_time(V(out), 10, 90)`, `vmax = max(V(out), 1m, 5m)`. Kinds: value_at(expr, at), min/max/pp/avg/rms/integral(expr[, from, to]), crossing(expr, level[, rise|fall|either, nth]), rise_time/fall_time(expr[, low_pct, high_pct]), overshoot_pct/undershoot_pct(expr), settling_time(expr[, tolerance_pct]), delay(from_expr, to_expr[, level_pct]), frequency/period/duty_cycle(expr), thd(expr, fundamental[, harmonics]), gain_db_at/phase_at(expr, freq), bandwidth_3db(expr[, dc|peak]), unity_gain_freq/phase_margin/gain_margin/peak_gain(expr), freq_at_db(expr, db), peaking_db(expr), q_lowpass(expr), output_noise_at/input_noise_at(freq), output_noise_rms/input_noise_rms([from, to]), psrr_db_at/cmrr_db_at(expr, source, freq). bandwidth_3db is the first frequency where the gain falls 3 dB below the gain at the lowest swept frequency (with `peak`, 3 dB below the peak gain, and for a bandpass the width between the two edges); freq_at_db is where the gain crosses an absolute level in dB, so freq_at_db(V(out), -3) is the -3 dB corner only when the low-frequency gain is 0 dB. peaking_db is the peak gain minus the low-frequency gain (0 for a Butterworth lowpass); q_lowpass estimates the Q of a second-order lowpass from an AC run (0.707 for Butterworth, with its natural frequency as f0); poles_zeros gives Q exactly. Noise measurements read a .noise analysis (such as `.noise V(out) Vin dec 20 1 10Meg`): densities in V/√Hz (A/√Hz for a current source input), rms integrated over the band. psrr_db_at and cmrr_db_at divide |expr| in the circuit's AC run, whose input carries AC 1, by |expr| in a second AC run aispice adds with AC 1 on `source` only (the supply, or the common-mode source that feeds both inputs); expr is the output, such as V(out). Expressions use V(node), V(a,b), I(R1), + - * /, and db(), mag(), ph().";

const SPEC_SYNTAX: &str = "Specs are one per line: `name = measurement op limit`, with op one of `>=`, `<=`, `in a..b` or `= target +- tol`, e.g. `bw = bandwidth_3db(V(out)) in 1Meg..2Meg`, `gain = gain_db_at(V(out)/V(in), 1k) >= 20`, `pm = phase_margin(V(out)) >= 45`, `q = q_lowpass(V(out)) = 0.707 +- 0.03`, `vn = input_noise_at(1k) <= 10n`, `psrr = psrr_db_at(V(out), VDD, 1k) >= 60`.";

const CORNERS_HELP: &str = "`corners` crosses process corners (presets tt, ss, ff, sf, fs, or maps such as `mos=ss res=wcs`, which rewrite the circuit's `.lib <file> <family>_<corner>` lines), temperatures in Celsius and supply values (a source or .param), and simulates every corner.";

fn parse_measures(lines: &[String]) -> Result<Vec<(String, Measure)>, String> {
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let (name, body) = match l.split_once('=') {
                Some((n, b)) if !n.contains('(') => (n.trim().to_string(), b.trim()),
                _ => (format!("m{}", i + 1), l.trim()),
            };
            Measure::parse(body)
                .map(|m| (name, m))
                .map_err(|e| format!("measurement `{l}`: {e}"))
        })
        .collect()
}

/// Specs for a circuit: given text, or the `<circuit>.specs` file beside it.
pub(crate) fn load_specs(
    ws: &Workspace,
    circuit: &str,
    given: Option<&str>,
) -> Result<(Vec<Spec>, String), String> {
    let p = ws.project().map_err(|e| e.to_string())?;
    let text = match given {
        Some(t) => t.to_string(),
        None => p.read_text(&specs_file(circuit)).map_err(|_| {
            format!(
                "no specs given and no {} file; pass specs, for example: {SPEC_SYNTAX}",
                specs_file(circuit)
            )
        })?,
    };
    let specs = parse_specs(&text).map_err(|e| format!("specs: {e}. {SPEC_SYNTAX}"))?;
    if specs.is_empty() {
        return Err(format!("no specs found. {SPEC_SYNTAX}"));
    }
    Ok((specs, text))
}

/// Save specs beside the circuit, with the user's approval when the front end
/// asks for it. Specs files are plain text and only ever `<circuit>.specs`.
async fn save_specs(ws: &Workspace, p: &crate::project::Project, circuit: &str, text: &str) {
    let file = specs_file(circuit);
    if !ws
        .approve(circuit, &format!("Save specs to {file}"), text)
        .await
    {
        return;
    }
    // The output resolver, not the reader's: a `.specs` that is a link to
    // another file of the project must not get the specs written into it.
    if let Ok(path) = p.resolve_output(&file) {
        let _ = crate::project::atomic_write(&path, text.as_bytes());
    }
}

pub fn specs_file(circuit: &str) -> String {
    match circuit.rsplit_once('.') {
        Some((stem, _)) => format!("{stem}.specs"),
        None => format!("{circuit}.specs"),
    }
}

/// A spec report in the shape the desktop app renders (`SpecReport` in
/// `app/src/ipc/types.ts`): the value with its unit for display, and the
/// margin as the distance to the nearest limit in the same unit, negative
/// when the spec fails.
pub fn ui_spec_report(report: &SpecReport) -> Value {
    let rows: Vec<Value> = report
        .rows
        .iter()
        .map(|r| {
            let margin = r.value.and_then(|v| {
                let limits = [r.min.map(|min| v - min), r.max.map(|max| max - v)];
                let nearest = limits.into_iter().flatten().reduce(f64::min);
                nearest.or_else(|| r.target.map(|t| -(v - t).abs()))
            });
            let display = r.value.map_or_else(
                || "n/a".into(),
                |v| aispice_sim::measure::spaced(v, &r.unit),
            );
            json!({
                "name": r.name,
                "value": r.value,
                "min": r.min,
                "max": r.max,
                "pass": r.pass,
                "margin": margin,
                "display": display,
            })
        })
        .collect();
    json!({"rows": rows, "all_pass": report.all_pass, "summary": report.summary})
}

/// The structured run payload the desktop app renders (`RunView`).
pub fn run_view(run: &StoredRun, measured: &[MeasureResult]) -> Value {
    let datasets: Vec<Value> = run
        .output
        .datasets
        .iter()
        .enumerate()
        .map(|(i, d)| {
            json!({
                "index": i,
                "plotname": d.plotname,
                "kind": d.kind,
                "axis": d.axis_vector().map(|v| v.name.clone()),
                "vectors": d.vectors.iter().map(|v| json!({"name": v.name, "quantity": v.quantity})).collect::<Vec<_>>(),
                "points": d.len(),
                "steps": d.steps.iter().map(|s| s.label.clone()).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut measurements: Vec<Value> = run
        .output
        .measurements
        .iter()
        .map(|m| {
            let display = m.value.map(units::format).unwrap_or_else(|| "failed".into());
            json!({"name": m.name, "value": m.value, "unit": "", "display": display, "note": m.detail})
        })
        .collect();
    measurements.extend(measured.iter().map(|m| json!({"name": m.name, "value": m.value, "unit": m.unit, "display": m.value_display(), "note": m.note})));
    json!({
        "run_id": run.id,
        "circuit": run.circuit,
        "simulator": run.simulator,
        "datasets": datasets,
        "measurements": measurements,
        "op": op_values(&run.output.datasets),
        "errors": run.output.errors,
        "warnings": run.output.warnings,
        "duration_ms": run.output.duration.as_millis() as u64,
        "log": run.output.log.chars().take(20_000).collect::<String>(),
    })
}

fn op_values(datasets: &[Dataset]) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    if let Some(op) = datasets.iter().find(|d| d.kind == AnalysisKind::Op) {
        for v in &op.vectors {
            if let Some(x) = v.data.real().first() {
                out.insert(v.name.clone(), json!(x));
            }
        }
    }
    out
}

fn run_text(run: &StoredRun, measured: &[MeasureResult]) -> String {
    let o = &run.output;
    let mut t = format!(
        "Simulated {} with {} in {} ms (run {}).\n",
        run.circuit,
        run.simulator.name(),
        o.duration.as_millis(),
        run.id
    );
    for n in &run.notes {
        t.push_str(&format!("  note: {n}\n"));
    }
    for e in &o.errors {
        t.push_str(&format!("  error: {e}\n"));
    }
    for w in o.warnings.iter().take(10) {
        t.push_str(&format!("  warning: {w}\n"));
    }
    for d in &o.datasets {
        let names: Vec<&str> = d.vectors.iter().map(|v| v.name.as_str()).take(40).collect();
        let more = d.vectors.len().saturating_sub(40);
        t.push_str(&format!(
            "  {} ({:?}): {} points{}; vectors: {}{}\n",
            d.plotname,
            d.kind,
            d.len(),
            if d.is_stepped() {
                format!(", {} steps", d.steps.len())
            } else {
                String::new()
            },
            names.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        ));
    }
    let op = op_values(&o.datasets);
    if !op.is_empty() {
        t.push_str("Operating point:\n");
        for (k, v) in op.iter().take(60) {
            let unit = if k.starts_with('I') { "A" } else { "V" };
            t.push_str(&format!(
                "  {k} = {}\n",
                units::format_with_unit(v.as_f64().unwrap_or(f64::NAN), unit)
            ));
        }
    }
    if !o.measurements.is_empty() {
        t.push_str("Simulator .meas results:\n");
        for m in &o.measurements {
            let step = m.step.map(|s| format!(" (step {s})")).unwrap_or_default();
            t.push_str(&format!(
                "  {}{step} = {}\n",
                m.name,
                m.value
                    .map(units::format)
                    .unwrap_or_else(|| "failed".into())
            ));
        }
    }
    if !measured.is_empty() {
        t.push_str("Measurements:\n");
        for m in measured {
            t.push_str(&format!("  {}\n", m.display()));
        }
    }
    t
}

/// The run a measurement, plot or table reads. A run named by id is used
/// as it is, with a note when the circuit has changed since. Otherwise the
/// circuit's latest run is used while it still matches the circuit; when
/// the circuit changed since that run, it is simulated again with the same
/// analysis, so numbers never describe an older version of the circuit.
/// Returns the run and a note for the answer.
async fn fresh_or_given_run(
    ws: &Workspace,
    circuit: &str,
    run_id: Option<&str>,
    ctx: &ToolContext,
) -> Result<(Arc<StoredRun>, Option<String>), String> {
    let p = ws.project().map_err(|e| e.to_string())?;
    let now = ws.runner.source_hash(&p, circuit).ok();
    let changed = |r: &StoredRun| r.source.is_some() && now.is_some() && r.source != now;
    if let Some(id) = run_id {
        let run = ws.runner.get(id).ok_or_else(|| {
            format!("no run {id} (runs are kept for the session; simulate again)")
        })?;
        let note = changed(&run).then(|| {
            format!("{circuit} changed since run {id}; these numbers describe it as it was then. Leave out `run` to simulate it again.")
        });
        return Ok((run, note));
    }
    match ws.runner.latest(circuit) {
        Some(r) if !changed(&r) => Ok((r, None)),
        Some(stale) => {
            let again = ws
                .runner
                .run_circuit(
                    &p,
                    circuit,
                    Some(stale.simulator.name()),
                    &stale.mods,
                    &ctx.cancel,
                )
                .await
                .map_err(|e| {
                    format!(
                        "{circuit} changed since run {}, and simulating it again failed: {e}",
                        stale.id
                    )
                })?;
            let note = format!(
                "{circuit} changed since run {}; simulated it again as run {} with the same analysis.",
                stale.id, again.id
            );
            Ok((again, Some(note)))
        }
        None => ws
            .runner
            .run_circuit(&p, circuit, None, &RunMods::default(), &ctx.cancel)
            .await
            .map(|r| (r, None))
            .map_err(|e| e.to_string()),
    }
}

/// A run's datasets plus the derived AC runs its PSRR and CMRR
/// measurements need, made from the circuit as it is now with the run's
/// changes, on the run's simulator.
#[allow(clippy::too_many_arguments)]
async fn with_derived(
    ws: &Workspace,
    p: &crate::project::Project,
    circuit: &str,
    mods: &RunMods,
    measures: &[(String, Measure)],
    simulator: SimId,
    datasets: &[Dataset],
    ctx: &ToolContext,
) -> Result<Vec<Dataset>, String> {
    let mut out = datasets.to_vec();
    if measures.iter().all(|(_, m)| m.stimulus_source().is_none()) {
        return Ok(out);
    }
    let (mut netlist, std_libs, _) = ws
        .runner
        .netlist_for(p, circuit)
        .map_err(|e| e.to_string())?;
    crate::runner::apply_mods(&mut netlist, mods);
    let sources = analysis::derived_sources(&netlist, measures.iter().map(|(_, m)| m))?;
    out.extend(
        analysis::derived_datasets(
            ws,
            p,
            circuit,
            &netlist,
            &std_libs,
            Some(simulator.name()),
            &sources,
            ctx,
        )
        .await?,
    );
    Ok(out)
}

macro_rules! input {
    ($v:expr) => {
        match parse_input($v) {
            Ok(i) => i,
            Err(e) => return e,
        }
    };
}

pub struct Simulators {
    pub ws: Arc<Workspace>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoArgs {}

#[async_trait]
impl Tool for Simulators {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        spec::<NoArgs>(
            "simulators",
            "List the simulators installed on this machine (ngspice, LTspice, Xyce, Spectre) with versions and how each will run.",
        )
    }

    async fn call(&self, _ctx: &ToolContext, _input: Value) -> ToolOutput {
        let found = self.ws.runner.detect().await;
        let mut t = String::new();
        for (id, d) in &found {
            t.push_str(&format!(
                "{}: {}{}\n",
                id.name(),
                if d.found { "available" } else { "not found" },
                d.version
                    .as_ref()
                    .map(|v| format!(" ({v})"))
                    .unwrap_or_default()
            ));
            for n in &d.notes {
                t.push_str(&format!("  {n}\n"));
            }
        }
        ToolOutput::text(t).with_data(json!({"kind": "generic", "value": found.iter().map(|(i, d)| json!({"id": i, "found": d.found, "version": d.version, "path": d.path, "notes": d.notes})).collect::<Vec<_>>()}))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimulateInput {
    pub circuit: String,
    /// `ngspice`, `ltspice`, `xyce` or `spectre`. Default: automatic
    /// (ngspice, falling back to LTspice for LTspice-only constructs).
    #[serde(default)]
    pub simulator: Option<String>,
    /// Run this analysis instead of the circuit's own, without changing the
    /// file, e.g. `.ac dec 50 1 10Meg` or `.tran 0 5m 0 1u`.
    #[serde(default)]
    pub analysis: Option<String>,
    /// Measurements to take on the result (see the tool description).
    #[serde(default)]
    pub measurements: Vec<String>,
}

pub struct Simulate {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for Simulate {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        spec::<SimulateInput>(
            "simulate",
            &format!(
                "Simulate a circuit and report what came back: analyses, vector names, operating point, the simulator's own .meas results, warnings and errors, plus any measurements you ask for. Fix lint errors first. Use the run id with measure, plot and read_waveform. {MEASURE_SYNTAX}"
            ),
        )
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: SimulateInput = input!(input);
        let measures = match parse_measures(&input.measurements) {
            Ok(m) => m,
            Err(e) => return ToolOutput::error(e),
        };
        let p = match self.ws.project() {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let mods = RunMods {
            analysis: input.analysis.clone(),
            extra: Vec::new(),
        };
        let run = match self
            .ws
            .runner
            .run_circuit(
                &p,
                &input.circuit,
                input.simulator.as_deref(),
                &mods,
                &ctx.cancel,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("Simulation failed: {e}")),
        };
        let datasets = match with_derived(
            &self.ws,
            &p,
            &input.circuit,
            &mods,
            &measures,
            run.simulator,
            &run.output.datasets,
            ctx,
        )
        .await
        {
            Ok(d) => d,
            Err(e) => return ToolOutput::error(e),
        };
        let measured: Vec<MeasureResult> = measures
            .iter()
            .map(|(n, m)| measure(n, m, &datasets))
            .collect();
        let text = run_text(&run, &measured);
        let out = ToolOutput::text(text)
            .with_data(json!({"kind": "run", "run": run_view(&run, &measured)}));
        if run.output.datasets.is_empty() && !run.output.errors.is_empty() {
            ToolOutput {
                is_error: true,
                ..out
            }
        } else {
            out
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MeasureInput {
    pub circuit: String,
    /// A run id from simulate. Default: the circuit's latest run, simulating
    /// first if there is none, or again (with that run's analysis) if the
    /// circuit has changed since.
    #[serde(default)]
    pub run: Option<String>,
    pub measurements: Vec<String>,
}

pub struct MeasureTool {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for MeasureTool {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        spec::<MeasureInput>(
            "measure",
            &format!(
                "Take measurements on a simulation result without re-running it. Without a run id it uses the circuit's latest run, and simulates the circuit again (with that run's analysis) when the circuit has changed since, so numbers always describe the circuit as it is; the answer says when that happened. {MEASURE_SYNTAX}"
            ),
        )
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: MeasureInput = input!(input);
        let measures = match parse_measures(&input.measurements) {
            Ok(m) => m,
            Err(e) => return ToolOutput::error(e),
        };
        let (run, note) =
            match fresh_or_given_run(&self.ws, &input.circuit, input.run.as_deref(), ctx).await {
                Ok(r) => r,
                Err(e) => return ToolOutput::error(e),
            };
        let note = note.map(|n| format!("note: {n}\n")).unwrap_or_default();
        let p = match self.ws.project() {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let datasets = match with_derived(
            &self.ws,
            &p,
            &input.circuit,
            &run.mods,
            &measures,
            run.simulator,
            &run.output.datasets,
            ctx,
        )
        .await
        {
            Ok(d) => d,
            Err(e) => return ToolOutput::error(e),
        };
        let measured: Vec<MeasureResult> = measures
            .iter()
            .map(|(n, m)| measure(n, m, &datasets))
            .collect();
        let mut t = format!("{note}Measured run {} of {}:\n", run.id, run.circuit);
        for m in &measured {
            t.push_str(&format!("  {}\n", m.display()));
        }
        let data: Vec<Value> = measured.iter().map(|m| json!({"name": m.name, "value": m.value, "unit": m.unit, "display": m.value_display(), "note": m.note})).collect();
        ToolOutput::text(t).with_data(json!({"kind": "measure", "measurements": data}))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpecsInput {
    pub circuit: String,
    /// Specs, one per line. When omitted, the `<circuit>.specs` file beside
    /// the circuit is used.
    #[serde(default)]
    pub specs: Option<String>,
    /// Save the given specs to `<circuit>.specs` for later checks. Default true.
    #[serde(default = "yes")]
    pub save: bool,
    #[serde(default)]
    pub simulator: Option<String>,
    /// Check at every corner of this set and report each spec's worst value,
    /// its margin and the corner where it occurred, plus every spec at every
    /// corner. For example `{"process": ["tt", "ss", "ff"], "temperatures":
    /// [-40, 27, 125], "supplies": [{"target": "VDD", "values": [1.62,
    /// 1.98]}]}`.
    #[serde(default)]
    pub corners: Option<CornerSet>,
    /// Check the specs after layout too: the layout (a GDS from the layout
    /// tool) is extracted and put in place of the schematic's devices, and
    /// each spec is reported before and after layout.
    #[serde(default)]
    pub post_layout: Option<crate::layout::PostLayout>,
}

fn yes() -> bool {
    true
}

/// Specs as named measurements, for the derived runs they need.
fn spec_measures(specs: &[Spec]) -> Vec<(String, Measure)> {
    specs
        .iter()
        .map(|s| (s.name.clone(), s.measure.clone()))
        .collect()
}

/// The circuit's corner variants. The library reader is not `Send`, so it
/// lives only here, never across an await.
fn corner_variants(
    ws: &Workspace,
    p: &crate::project::Project,
    circuit: &str,
    netlist: &Netlist,
    set: &CornerSet,
) -> Result<Vec<(String, Netlist)>, String> {
    let files = analysis::library_files(ws, p, circuit)?;
    let lookup = |f: &str| files.sections(f);
    corner::variants(netlist, set, &lookup).map_err(|e| format!("corners: {e}"))
}

/// Specs at every corner of a set, on a base netlist: the circuit's own,
/// or the post-layout one, which corner rewrites treat the same way since
/// its PDK model header is untouched.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn corner_report(
    ws: &Workspace,
    p: &crate::project::Project,
    circuit: &str,
    base: Netlist,
    std_libs: &[String],
    specs: &[Spec],
    set: &CornerSet,
    simulator: Option<&str>,
    ctx: &ToolContext,
) -> Result<(corner::CornerReport, Option<SimId>), String> {
    let netlist = analysis::as_simulator_reads_it(ws, p, base, simulator).await;
    let sources = analysis::derived_sources(&netlist, specs.iter().map(|s| &s.measure))?;
    let variants = corner_variants(ws, p, circuit, &netlist, set)?;
    let names: Vec<String> = variants.iter().map(|(n, _)| n.clone()).collect();
    let results = run_variants(ws, circuit, variants, std_libs, simulator, &sources, ctx).await;
    let mut sim = None;
    let reports: Vec<Result<SpecReport, String>> = results
        .iter()
        .map(|(_, r)| match r {
            Ok(v) => {
                sim.get_or_insert(v.run.simulator);
                Ok(evaluate(specs, &v.datasets))
            }
            Err(e) => Err(e.clone()),
        })
        .collect();
    Ok((
        corner::summarize(specs, &names[..reports.len()], &reports),
        sim,
    ))
}

/// Check specs at every corner of a set.
async fn check_corners(
    ws: &Workspace,
    p: &crate::project::Project,
    circuit: &str,
    specs: &[Spec],
    set: &CornerSet,
    simulator: Option<&str>,
    ctx: &ToolContext,
) -> Result<ToolOutput, String> {
    let (netlist, std_libs, _) = ws
        .runner
        .netlist_for(p, circuit)
        .map_err(|e| e.to_string())?;
    let (report, sim) = corner_report(
        ws, p, circuit, netlist, &std_libs, specs, set, simulator, ctx,
    )
    .await?;
    let t = format!(
        "{circuit} at {} corners{}\n{}",
        report.corners.len(),
        sim.map(|s| format!(" on {}", s.name())).unwrap_or_default(),
        analysis::corner_text(&report)
    );
    Ok(ToolOutput::text(t).with_data(json!({
        "kind": "specs",
        "report": ui_spec_report(&report.worst),
        "corners": analysis::corner_json(&report),
    })))
}

/// A post-layout netlist, its standard libraries and the extraction.
type PostLayoutRun = (Netlist, Vec<String>, aispice_layout::extract::Extracted);

fn extracted_json(ex: &aispice_layout::extract::Extracted) -> Value {
    json!({"devices": ex.devices, "capacitors": ex.capacitors, "resistors": ex.resistors, "total_cap_ff": ex.total_cap_ff})
}

/// Check specs at every corner, on the schematic and after layout: each
/// spec's worst case before and after, then every corner after layout.
#[allow(clippy::too_many_arguments)]
async fn check_corners_post_layout(
    ws: &Workspace,
    p: &crate::project::Project,
    circuit: &str,
    specs: &[Spec],
    set: &CornerSet,
    simulator: Option<&str>,
    ctx: &ToolContext,
    layout: &str,
    post: PostLayoutRun,
) -> Result<ToolOutput, String> {
    let (base, base_libs, _) = ws
        .runner
        .netlist_for(p, circuit)
        .map_err(|e| e.to_string())?;
    let (pre, sim) =
        corner_report(ws, p, circuit, base, &base_libs, specs, set, simulator, ctx).await?;
    let (netlist, std_libs, ex) = post;
    let (after, _) = corner_report(
        ws, p, circuit, netlist, &std_libs, specs, set, simulator, ctx,
    )
    .await?;
    let t = format!(
        "{circuit} before and after layout ({layout}) at {} corners{}, worst case of each spec\n{}{}After layout, every corner:\n{}",
        pre.corners.len(),
        sim.map(|s| format!(" on {}", s.name())).unwrap_or_default(),
        ex.to_text(),
        pre_post_text(&pre.worst, &after.worst),
        analysis::corner_text(&after)
    );
    Ok(
        ToolOutput::text(t).with_data(json!({"kind": "generic", "value": {
            "pre": ui_spec_report(&pre.worst),
            "post": ui_spec_report(&after.worst),
            "corners_pre": analysis::corner_json(&pre),
            "corners_post": analysis::corner_json(&after),
            "layout": layout,
            "extracted": extracted_json(&ex),
        }})),
    )
}

pub struct CheckSpecs {
    pub ws: Arc<Workspace>,
}

fn spec_text(report: &SpecReport) -> String {
    let mut t = format!("{}\n", report.summary);
    for r in &report.rows {
        t.push_str(&format!(
            "  {} {}\n",
            if r.pass { "PASS" } else { "FAIL" },
            r.display
        ));
    }
    t
}

#[async_trait]
impl Tool for CheckSpecs {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::Edit
    }

    fn spec(&self) -> ToolSpec {
        spec::<SpecsInput>(
            "check_specs",
            &format!(
                "Simulate the circuit and check it against a spec table, reporting pass or fail and the margin for each spec. This is how to prove a design meets its requirements; run it after every change that matters. With corners, each spec reports its worst value and the corner where it occurred, and passes only if it passes at every corner. With post_layout (a layout from the layout tool), the layout is extracted and simulated in place of the schematic's devices, and each spec is reported before and after layout (at every corner when corners are given). {CORNERS_HELP} {SPEC_SYNTAX} {MEASURE_SYNTAX}"
            ),
        )
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: SpecsInput = input!(input);
        let (specs, text) = match load_specs(&self.ws, &input.circuit, input.specs.as_deref()) {
            Ok(s) => s,
            Err(e) => return ToolOutput::error(e),
        };
        let p = match self.ws.project() {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        if input.specs.is_some() && input.save {
            save_specs(&self.ws, &p, &input.circuit, &text).await;
        }
        // The layout is extracted first, so a problem with it is reported
        // before any simulation.
        let post = match &input.post_layout {
            None => None,
            Some(pl) => {
                match crate::layout::post_layout_netlist(&self.ws, &input.circuit, pl, &ctx.cancel)
                    .await
                {
                    Ok(r) => Some((pl.layout.clone(), r)),
                    Err(e) => return ToolOutput::error(format!("Post-layout: {e}")),
                }
            }
        };
        if let Some(set) = &input.corners {
            let simulator = input.simulator.as_deref();
            let out = match post {
                None => {
                    check_corners(&self.ws, &p, &input.circuit, &specs, set, simulator, ctx).await
                }
                Some((layout, run)) => {
                    check_corners_post_layout(
                        &self.ws,
                        &p,
                        &input.circuit,
                        &specs,
                        set,
                        simulator,
                        ctx,
                        &layout,
                        run,
                    )
                    .await
                }
            };
            return out.unwrap_or_else(ToolOutput::error);
        }
        let run = match self
            .ws
            .runner
            .run_circuit(
                &p,
                &input.circuit,
                input.simulator.as_deref(),
                &RunMods::default(),
                &ctx.cancel,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("Simulation failed: {e}")),
        };
        let datasets = match with_derived(
            &self.ws,
            &p,
            &input.circuit,
            &RunMods::default(),
            &spec_measures(&specs),
            run.simulator,
            &run.output.datasets,
            ctx,
        )
        .await
        {
            Ok(d) => d,
            Err(e) => return ToolOutput::error(e),
        };
        let report = evaluate(&specs, &datasets);
        let t = format!(
            "{} (run {} on {})\n{}",
            input.circuit,
            run.id,
            run.simulator.name(),
            spec_text(&report)
        );
        let Some((layout, (netlist, std_libs, ex))) = post else {
            return ToolOutput::text(t)
                .with_data(json!({"kind": "specs", "report": ui_spec_report(&report)}));
        };
        // One more variant: the post-layout netlist, with the derived runs
        // its measurements need, as for any variant of the circuit.
        let sources = match analysis::derived_sources(&netlist, specs.iter().map(|s| &s.measure)) {
            Ok(s) => s,
            Err(e) => return ToolOutput::error(format!("{t}Post-layout: {e}")),
        };
        let mut runs = run_variants(
            &self.ws,
            &input.circuit,
            vec![("after layout".into(), netlist)],
            &std_libs,
            input.simulator.as_deref(),
            &sources,
            ctx,
        )
        .await;
        let after = match runs.pop() {
            Some((_, Ok(v))) => v,
            Some((_, Err(e))) => {
                return ToolOutput::error(format!("{t}Post-layout simulation failed: {e}"));
            }
            None => return ToolOutput::error(format!("{t}Post-layout simulation cancelled")),
        };
        let post_report = evaluate(&specs, &after.datasets);
        let t = format!(
            "{} before and after layout ({layout}; schematic run {}, post-layout run on {})\n{}{}",
            input.circuit,
            run.id,
            after.run.simulator.name(),
            ex.to_text(),
            pre_post_text(&report, &post_report)
        );
        ToolOutput::text(t).with_data(json!({"kind": "generic", "value": {
            "pre": ui_spec_report(&report),
            "post": ui_spec_report(&post_report),
            "layout": layout,
            "extracted": extracted_json(&ex),
        }}))
    }
}

/// Specs before and after layout side by side, with the change.
pub fn pre_post_text(pre: &SpecReport, post: &SpecReport) -> String {
    let mut t = format!(
        "Schematic: {}\nAfter layout: {}\n",
        pre.summary, post.summary
    );
    for a in &pre.rows {
        let b = post.row(&a.name);
        let show = |v: Option<f64>| {
            v.map_or_else(
                || "n/a".to_string(),
                |v| aispice_sim::measure::spaced(v, &a.unit),
            )
        };
        let after = b.and_then(|b| b.value);
        let change = match (a.value, after) {
            (Some(x), Some(y)) if x != 0.0 => format!(" ({:+.1}%)", (y - x) / x.abs() * 100.0),
            (Some(x), Some(y)) => format!(" ({:+})", y - x),
            _ => String::new(),
        };
        let verdict = |pass: Option<bool>| match pass {
            Some(true) => "pass",
            Some(false) => "FAIL",
            None => "n/a",
        };
        t.push_str(&format!(
            "  {}: {} {} -> {} {}{}\n",
            a.name,
            show(a.value),
            verdict(Some(a.pass)),
            show(after),
            verdict(b.map(|b| b.pass)),
            change
        ));
    }
    t
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PlotInput {
    pub circuit: String,
    #[serde(default)]
    pub run: Option<String>,
    /// Expressions to draw, e.g. `V(out)`, `V(out)/V(in)` (a Bode plot for AC
    /// data), `I(R1)`.
    pub traces: Vec<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// Limit the x axis, e.g. `[0, 0.001]`.
    #[serde(default)]
    pub x_range: Option<[f64; 2]>,
}

pub struct Plot {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for Plot {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        spec::<PlotInput>(
            "plot",
            "Plot waveforms from a simulation as an image: time-domain traces, or a Bode plot (magnitude in dB and phase) for AC data. Use it to look at behaviour; use measure for numbers. Without a run id it uses the circuit's latest run, simulating again first if the circuit has changed since.",
        )
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: PlotInput = input!(input);
        let (run, note) =
            match fresh_or_given_run(&self.ws, &input.circuit, input.run.as_deref(), ctx).await {
                Ok(r) => r,
                Err(e) => return ToolOutput::error(e),
            };
        let note = note.map(|n| format!("note: {n}\n")).unwrap_or_default();
        let req = PlotRequest {
            traces: input.traces.clone(),
            title: input.title.clone(),
            x_range: input.x_range,
            ..Default::default()
        };
        let svg = match plot_svg(&run.output.datasets, &req) {
            Ok(s) => s,
            Err(e) => {
                return ToolOutput::error(format!(
                    "Could not plot: {e}. Vectors in this run: {}",
                    run.output
                        .datasets
                        .iter()
                        .flat_map(|d| d.names())
                        .take(40)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        };
        match aispice_core::render::render_png(&svg, 1.0) {
            Ok(png) => ToolOutput::text(format!(
                "{note}Plot of {} from run {}",
                input.traces.join(", "),
                run.id
            ))
            .with_image(
                "image/png",
                base64::engine::general_purpose::STANDARD.encode(png),
            )
            .with_data(json!({"kind": "plot", "svg": svg})),
            Err(e) => ToolOutput::error(format!("Could not rasterise the plot: {e}")),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaveInput {
    pub circuit: String,
    #[serde(default)]
    pub run: Option<String>,
    pub signals: Vec<String>,
    #[serde(default)]
    pub from: Option<f64>,
    #[serde(default)]
    pub to: Option<f64>,
    /// Number of evenly spaced rows to return (default 40, at most 400).
    #[serde(default)]
    pub points: Option<usize>,
}

pub struct ReadWaveform {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for ReadWaveform {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        spec::<WaveInput>(
            "read_waveform",
            "Read sampled values of signals from a simulation as a table (evenly spaced over a range). For AC data values are magnitude in dB and phase in degrees. Without a run id it uses the circuit's latest run, simulating again first if the circuit has changed since.",
        )
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: WaveInput = input!(input);
        let (run, note) =
            match fresh_or_given_run(&self.ws, &input.circuit, input.run.as_deref(), ctx).await {
                Ok(r) => r,
                Err(e) => return ToolOutput::error(e),
            };
        let note = note.map(|n| format!("note: {n}\n")).unwrap_or_default();
        let Some(ds) = run.output.datasets.iter().find(|d| {
            input.signals.iter().all(|s| {
                aispice_sim::expr::parse(s)
                    .ok()
                    .and_then(|e| e.eval_all(d).ok())
                    .is_some()
            })
        }) else {
            return ToolOutput::error(format!(
                "No dataset has all of {}. Vectors: {}",
                input.signals.join(", "),
                run.output
                    .datasets
                    .iter()
                    .flat_map(|d| d.names())
                    .take(40)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };
        let Some(axis) = ds.axis_vector() else {
            return ToolOutput::error(
                "this analysis has no independent axis (an operating point); simulate shows its values",
            );
        };
        let x = axis.data.real();
        let n = input.points.unwrap_or(40).clamp(2, 400);
        let lo = input.from.unwrap_or(x.first().copied().unwrap_or(0.0));
        let hi = input.to.unwrap_or(x.last().copied().unwrap_or(0.0));
        let ac = ds.kind == AnalysisKind::Ac;
        let series: Vec<aispice_sim::expr::Series> = input
            .signals
            .iter()
            .filter_map(|s| {
                aispice_sim::expr::parse(s)
                    .ok()
                    .and_then(|e| e.eval_all(ds).ok())
            })
            .collect();
        let mut t = note;
        t.push_str(if axis.quantity == Quantity::Frequency {
            "frequency"
        } else {
            axis.name.as_str()
        });
        for s in &input.signals {
            t.push_str(&if ac {
                format!("\t{s} dB\t{s} deg")
            } else {
                format!("\t{s}")
            });
        }
        t.push('\n');
        for k in 0..n {
            let target = if ac && lo > 0.0 && hi > 0.0 {
                lo * (hi / lo).powf(k as f64 / (n - 1) as f64)
            } else {
                lo + (hi - lo) * k as f64 / (n - 1) as f64
            };
            let i = x
                .partition_point(|v| *v < target)
                .min(x.len().saturating_sub(1));
            t.push_str(&units::format(x[i]));
            for s in &series {
                match s {
                    aispice_sim::expr::Series::Real(v) => {
                        t.push_str(&format!("\t{}", units::format(v[i])))
                    }
                    aispice_sim::expr::Series::Complex(v) => {
                        t.push_str(&format!("\t{:.3}\t{:.2}", v[i].db(), v[i].phase_deg()))
                    }
                }
            }
            t.push('\n');
        }
        ToolOutput::text(t)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SweepInput {
    pub circuit: String,
    /// Parameters to sweep: an element (`R1`) or a `.param` name, with values
    /// as a list `[1000, 2200, 4700]`, a range `{"kind": "dec", "start": 1000,
    /// "stop": 100000, "points_per_decade": 5}`, or text `"lin 1k 10k 10"`.
    pub params: Vec<SweepParam>,
    pub measurements: Vec<String>,
    #[serde(default)]
    pub analysis: Option<String>,
    #[serde(default)]
    pub simulator: Option<String>,
}

pub struct Sweep {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for Sweep {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        spec::<SweepInput>(
            "sweep",
            &format!(
                "Sweep component values or .param values (several parameters give every combination, at most 200 runs) and tabulate measurements for each. Works with every simulator. {MEASURE_SYNTAX}"
            ),
        )
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: SweepInput = input!(input);
        let measures = match parse_measures(&input.measurements) {
            Ok(m) => m,
            Err(e) => return ToolOutput::error(e),
        };
        let p = match self.ws.project() {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let (mut netlist, std_libs, _) = match self.ws.runner.netlist_for(&p, &input.circuit) {
            Ok(v) => v,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        crate::runner::apply_mods(
            &mut netlist,
            &RunMods {
                analysis: input.analysis.clone(),
                extra: vec![],
            },
        );
        let variants = match sweep::variants_with_limit(&netlist, &input.params, 200) {
            Ok(v) => v,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let sources = match analysis::derived_sources(&netlist, measures.iter().map(|(_, m)| m)) {
            Ok(s) => s,
            Err(e) => return ToolOutput::error(e),
        };
        let results = run_variants(
            &self.ws,
            &input.circuit,
            variants,
            &std_libs,
            input.simulator.as_deref(),
            &sources,
            ctx,
        )
        .await;
        let mut t = format!(
            "variant\t{}\n",
            measures
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join("\t")
        );
        let mut rows = Vec::new();
        for (label, r) in &results {
            match r {
                Ok(run) => {
                    let vals: Vec<MeasureResult> = measures
                        .iter()
                        .map(|(n, m)| measure(n, m, &run.datasets))
                        .collect();
                    t.push_str(&format!(
                        "{label}\t{}\n",
                        vals.iter()
                            .map(|v| v.value.map(units::format).unwrap_or_else(|| "n/a".into()))
                            .collect::<Vec<_>>()
                            .join("\t")
                    ));
                    rows.push(json!({"label": label, "values": vals.iter().map(|v| v.value).collect::<Vec<_>>()}));
                }
                Err(e) => t.push_str(&format!("{label}\tfailed: {e}\n")),
            }
        }
        ToolOutput::text(t).with_data(json!({"kind": "generic", "value": {"measurements": measures.iter().map(|(n, _)| n).collect::<Vec<_>>(), "rows": rows}}))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MonteCarloInput {
    pub circuit: String,
    /// Tolerances: target is an element name or a glob such as `R*` or `C*`,
    /// `tol_pct` a percentage, `distribution` uniform (default) or gaussian
    /// (3 sigma = tolerance). May be empty when `mismatch` is set.
    #[serde(default)]
    pub tolerances: Vec<Tolerance>,
    /// Device mismatch from the circuit's corner libraries: `local` switches
    /// their `.lib` sections to `_mismatch`, `global` to `_stat` (process
    /// spread plus mismatch, typical corner only). The simulator draws the
    /// parameters with its own seed per run; needs ngspice or Xyce.
    #[serde(default)]
    pub mismatch: Option<Mismatch>,
    /// Run the analysis at every corner of this set, `runs` times each, and
    /// report the yield per corner as well.
    #[serde(default)]
    pub corners: Option<CornerSet>,
    /// Number of runs (default 50). Runs times corners may be at most 500.
    #[serde(default)]
    pub runs: Option<usize>,
    #[serde(default)]
    pub seed: Option<u64>,
    /// Specs to judge each run; default: the `<circuit>.specs` file.
    #[serde(default)]
    pub specs: Option<String>,
    #[serde(default)]
    pub simulator: Option<String>,
}

pub struct MonteCarlo {
    pub ws: Arc<Workspace>,
}

/// Simulations one Monte Carlo analysis may run, over all corners.
const MAX_MC_RUNS: usize = 500;

#[async_trait]
impl Tool for MonteCarlo {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        spec::<MonteCarloInput>(
            "monte_carlo",
            &format!(
                "Monte Carlo analysis: vary component values within their tolerances, device parameters through the PDK's mismatch or statistical model sections (`mismatch`), or both; simulate each sample and report the yield against the specs with per-spec spread and the worst run. Seeded and reproducible. With corners, every corner runs the same samples and reports its own yield. {CORNERS_HELP} {SPEC_SYNTAX}"
            ),
        )
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: MonteCarloInput = input!(input);
        let (specs, _) = match load_specs(&self.ws, &input.circuit, input.specs.as_deref()) {
            Ok(s) => s,
            Err(e) => return ToolOutput::error(e),
        };
        let p = match self.ws.project() {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let (netlist, std_libs, _) = match self.ws.runner.netlist_for(&p, &input.circuit) {
            Ok(v) => v,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        // Statistical parameters need a simulator that draws them per run,
        // and the same one for every run: no fallback to another.
        let simulator = match input.mismatch {
            None => input.simulator.clone(),
            Some(_) => match self.ws.runner.choose(input.simulator.as_deref()).await {
                Ok(id @ (SimId::Ngspice | SimId::Xyce)) => Some(id.name().to_string()),
                Ok(id) => {
                    return ToolOutput::error(format!(
                        "mismatch Monte Carlo needs ngspice or Xyce: {} gives the nominal value of a statistical parameter outside its own Monte Carlo analysis",
                        id.name()
                    ));
                }
                Err(e) => return ToolOutput::error(e.to_string()),
            },
        };
        let sources = match analysis::derived_sources(&netlist, specs.iter().map(|s| &s.measure)) {
            Ok(s) => s,
            Err(e) => return ToolOutput::error(e),
        };
        // The size of the request, from the list lengths alone, before any
        // netlist is built.
        let corner_count = match input.corners.as_ref().map(CornerSet::count) {
            Some(Ok(n)) => n,
            Some(Err(e)) => return ToolOutput::error(format!("corners: {e}")),
            None => 1,
        };
        let mut notes: Vec<String> = Vec::new();
        let n = match mc_runs(input.runs, corner_count) {
            Ok((n, note)) => {
                notes.extend(note);
                n
            }
            Err(e) => return ToolOutput::error(e),
        };
        let netlist =
            analysis::as_simulator_reads_it(&self.ws, &p, netlist, simulator.as_deref()).await;
        let bases = match &input.corners {
            Some(set) => match corner_variants(&self.ws, &p, &input.circuit, &netlist, set) {
                Ok(v) => v,
                Err(e) => return ToolOutput::error(e),
            },
            None => vec![(String::new(), netlist.clone())],
        };
        let seed = input.seed.unwrap_or(1);
        let mut labelled: Vec<(String, Netlist)> = Vec::with_capacity(n * bases.len());
        {
            let files = match analysis::library_files(&self.ws, &p, &input.circuit) {
                Ok(f) => f,
                Err(e) => return ToolOutput::error(e),
            };
            let lookup = |f: &str| files.sections(f);
            for (name, base) in &bases {
                let (variants, more) = match montecarlo::variants_with(
                    base,
                    &input.tolerances,
                    input.mismatch,
                    n,
                    seed,
                    &lookup,
                ) {
                    Ok(v) => v,
                    Err(e) => return ToolOutput::error(e.to_string()),
                };
                for note in more {
                    if !notes.contains(&note) {
                        notes.push(note);
                    }
                }
                labelled.extend(variants.into_iter().map(|v| {
                    let label = if name.is_empty() {
                        v.label
                    } else {
                        format!("{name} {}", v.label)
                    };
                    (label, v.netlist)
                }));
            }
        }
        let results = run_variants(
            &self.ws,
            &input.circuit,
            labelled,
            &std_libs,
            simulator.as_deref(),
            &sources,
            ctx,
        )
        .await;
        let mut all: Vec<SpecReport> = Vec::new();
        let mut per_corner: Vec<(String, montecarlo::YieldReport)> = Vec::new();
        for (name, chunk) in bases.iter().map(|(n, _)| n).zip(results.chunks(n)) {
            let reports: Vec<SpecReport> = chunk
                .iter()
                .filter_map(|(_, r)| r.as_ref().ok())
                .map(|run| evaluate(&specs, &run.datasets))
                .collect();
            if !name.is_empty() {
                per_corner.push((name.clone(), montecarlo::summarize(&reports)));
            }
            all.extend(reports);
        }
        let failed_runs = results.len() - all.len();
        let first_failure = results
            .iter()
            .find_map(|(label, r)| r.as_ref().err().map(|e| format!("{label}: {e}")))
            .unwrap_or_default();
        // A yield over no runs says nothing; the reason the runs failed does.
        if all.is_empty() {
            return ToolOutput::error(format!(
                "every one of the {failed_runs} runs failed to simulate; the first, {first_failure}"
            ));
        }
        let y = montecarlo::summarize(&all);
        let mut t = format!("{}\n", y.summary);
        if failed_runs > 0 {
            t.push_str(&format!(
                "  {failed_runs} run(s) failed to simulate; the first, {first_failure}\n"
            ));
        }
        for note in &notes {
            t.push_str(&format!("  note: {note}\n"));
        }
        for (name, c) in &per_corner {
            t.push_str(&format!(
                "  {name}: yield {}% ({} of {} runs pass)\n",
                (c.yield_pct * 10.0).round() / 10.0,
                c.passed,
                c.runs
            ));
        }
        for s in &y.specs {
            t.push_str(&format!(
                "  {}\n",
                serde_json::to_string(s).unwrap_or_default()
            ));
        }
        let corners: Vec<Value> = per_corner
            .iter()
            .map(|(name, c)| json!({"corner": name, "runs": c.runs, "passed": c.passed, "yield_pct": c.yield_pct}))
            .collect();
        ToolOutput::text(t).with_data(json!({"kind": "montecarlo", "runs": y.runs, "yield_pct": y.yield_pct, "report": y.summary, "specs": y.specs, "corners": corners}))
    }
}

#[derive(Debug, Deserialize, JsonSchema, Default, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// Fast and reliable for a few parameters.
    #[default]
    NelderMead,
    /// Better for many parameters or rough objectives.
    CmaEs,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OptimizeInput {
    pub circuit: String,
    /// Parameters to size: an element (`R1`) or `.param` name with `min` and
    /// `max` (`"1k"` style values accepted), optional `log_scale` (default true
    /// for R, C, L) and `snap` to an E-series (E6, E12, E24, E48, E96) for the
    /// final answer.
    pub params: Vec<Param>,
    /// Specs to meet; default: the `<circuit>.specs` file.
    #[serde(default)]
    pub specs: Option<String>,
    /// Optionally also minimise or maximise one spec's value once all pass.
    #[serde(default)]
    pub goal: Option<Goal>,
    #[serde(default)]
    pub method: Method,
    /// Simulation budget in candidates (default 80, at most 400).
    /// Candidates times corners may be at most 2000.
    #[serde(default)]
    pub max_evals: Option<usize>,
    /// Write the best (snapped) values into the schematic as an undoable edit.
    #[serde(default)]
    pub apply: bool,
    /// Save the given specs to `<circuit>.specs`, as the circuit's
    /// requirements from now on. Default true.
    #[serde(default = "yes")]
    pub save_specs: bool,
    #[serde(default)]
    pub simulator: Option<String>,
    /// Meet the specs at every corner of this set: each candidate is
    /// simulated at every corner and scored on its worst case.
    #[serde(default)]
    pub corners: Option<CornerSet>,
}

/// Simulations one optimization may run, candidates times corners.
const MAX_OPT_SIMS: usize = 2000;

/// Runs per corner for a Monte Carlo analysis: the runs asked for, refused
/// when runs times corners is over [`MAX_MC_RUNS`]; without a number, 50 or
/// as many as fit, with a note.
fn mc_runs(asked: Option<usize>, corners: usize) -> Result<(usize, Option<String>), String> {
    let corners = corners.max(1);
    match asked {
        Some(runs) if runs.saturating_mul(corners) > MAX_MC_RUNS => Err(format!(
            "{runs} runs{} is more than the limit of {MAX_MC_RUNS} simulations; ask for at most {} runs",
            if corners > 1 {
                format!(" at each of {corners} corners")
            } else {
                String::new()
            },
            MAX_MC_RUNS / corners
        )),
        Some(runs) => Ok((runs.max(1), None)),
        None if 50 * corners > MAX_MC_RUNS => {
            let n = (MAX_MC_RUNS / corners).max(1);
            Ok((
                n,
                Some(format!(
                    "{n} runs per corner, to stay within {MAX_MC_RUNS} simulations"
                )),
            ))
        }
        None => Ok((50, None)),
    }
}

/// Candidates for an optimization: the budget asked for (4 to 400), refused
/// when candidates times corners is over [`MAX_OPT_SIMS`]; without a
/// number, 80 or as many as fit, with a note.
fn opt_candidates(asked: Option<usize>, corners: usize) -> Result<(usize, Option<String>), String> {
    let corners = corners.max(1);
    match asked {
        Some(evals) if evals.clamp(4, 400).saturating_mul(corners) > MAX_OPT_SIMS => Err(format!(
            "{} candidates at each of {corners} corners is more than the limit of {MAX_OPT_SIMS} simulations; ask for at most {} (max_evals)",
            evals.clamp(4, 400),
            (MAX_OPT_SIMS / corners).max(4)
        )),
        Some(evals) => Ok((evals.clamp(4, 400), None)),
        None if 80 * corners > MAX_OPT_SIMS => {
            let n = (MAX_OPT_SIMS / corners).max(4);
            Ok((
                n,
                Some(format!(
                    "budget cut to {n} candidates, {corners} corners each, to stay within {MAX_OPT_SIMS} simulations"
                )),
            ))
        }
        None => Ok((80, None)),
    }
}

pub struct Optimize {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for Optimize {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::Edit
    }

    fn spec(&self) -> ToolSpec {
        spec::<OptimizeInput>(
            "optimize",
            &format!(
                "Size component values to meet a spec table with the simulator in the loop (Nelder-Mead or CMA-ES over log-scaled ranges), then round to standard E-series values and re-check. You choose the parameters and sensible ranges; the optimizer does the search. With corners it optimizes the worst case over every corner; the budget counts candidates, each simulated at every corner. {CORNERS_HELP} {SPEC_SYNTAX}"
            ),
        )
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: OptimizeInput = input!(input);
        let (specs, spec_source) =
            match load_specs(&self.ws, &input.circuit, input.specs.as_deref()) {
                Ok(s) => s,
                Err(e) => return ToolOutput::error(e),
            };
        let p = match self.ws.project() {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        if input.specs.is_some() && input.save_specs {
            save_specs(&self.ws, &p, &input.circuit, &spec_source).await;
        }
        let (netlist, std_libs, _) = match self.ws.runner.netlist_for(&p, &input.circuit) {
            Ok(v) => v,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let mut opt: Box<dyn optimize::Optimizer + Send> = match input.method {
            Method::NelderMead => {
                match NelderMead::new(&input.params, NelderMeadOptions::default()) {
                    Ok(o) => Box::new(o),
                    Err(e) => return ToolOutput::error(e.to_string()),
                }
            }
            Method::CmaEs => match CmaEs::new(&input.params, CmaEsOptions::default()) {
                Ok(o) => Box::new(o),
                Err(e) => return ToolOutput::error(e.to_string()),
            },
        };
        let objective = Objective {
            goal: input.goal.clone(),
        };
        let sources = match analysis::derived_sources(&netlist, specs.iter().map(|s| &s.measure)) {
            Ok(s) => s,
            Err(e) => return ToolOutput::error(e),
        };
        // The size of the request, from the list lengths alone, before any
        // netlist is built.
        let corner_count = match input.corners.as_ref().map(CornerSet::count) {
            Some(Ok(n)) => n,
            Some(Err(e)) => return ToolOutput::error(format!("corners: {e}")),
            None => 1,
        };
        let (max_evals, budget_note) = match opt_candidates(input.max_evals, corner_count) {
            Ok(v) => v,
            Err(e) => return ToolOutput::error(e),
        };
        // The corners, checked against the circuit's libraries once here;
        // candidates differ only in part values.
        let netlist = if input.corners.is_some() {
            analysis::as_simulator_reads_it(&self.ws, &p, netlist, input.simulator.as_deref()).await
        } else {
            netlist
        };
        let corners = match &input.corners {
            Some(set) => {
                if let Err(e) = corner_variants(&self.ws, &p, &input.circuit, &netlist, set) {
                    return ToolOutput::error(e);
                }
                set.corners().unwrap_or_default()
            }
            None => Vec::new(),
        };
        let per_candidate = corners.len().max(1);
        let options = RunOptions {
            max_evals,
            ..Default::default()
        };
        let ws = self.ws.clone();
        let circuit = input.circuit.clone();
        let params = input.params.clone();
        let sim = input.simulator.clone();
        let ctx2 = ctx.clone();
        let (specs_in, libs_in, netlist_in) = (specs.clone(), std_libs.clone(), netlist.clone());
        let (corners_in, sources_in) = (corners.clone(), sources.clone());
        let handle = tokio::runtime::Handle::current();
        // The optimizer drives a synchronous batch callback; each batch runs
        // its simulations concurrently on the async runtime.
        let result = tokio::task::spawn_blocking(move || {
            optimize::run(opt.as_mut(), &params, &options, |batch: &[Vec<f64>]| {
                let mut variants: Vec<(String, Netlist)> = Vec::new();
                for (i, values) in batch.iter().enumerate() {
                    let n = optimize::apply(&netlist_in, &params, values)
                        .unwrap_or_else(|_| netlist_in.clone());
                    if corners_in.is_empty() {
                        variants.push((format!("p{i}"), n));
                        continue;
                    }
                    for c in &corners_in {
                        let at =
                            corner::apply(&n, c, &corner::no_lookup).unwrap_or_else(|_| n.clone());
                        variants.push((format!("p{i} {}", c.name), at));
                    }
                }
                let results = handle.block_on(run_variants(
                    &ws,
                    &circuit,
                    variants,
                    &libs_in,
                    sim.as_deref(),
                    &sources_in,
                    &ctx2,
                ));
                (0..batch.len())
                    .map(|i| {
                        let runs = results.get(i * per_candidate..(i + 1) * per_candidate);
                        let reports: Option<Vec<SpecReport>> = runs.and_then(|runs| {
                            runs.iter()
                                .map(|(_, r)| {
                                    r.as_ref().ok().map(|v| evaluate(&specs_in, &v.datasets))
                                })
                                .collect()
                        });
                        match reports {
                            Some(reports) => objective.score_worst(&reports),
                            None => optimize::Evaluation {
                                value: optimize::MISSING_PENALTY,
                                feasible: false,
                            },
                        }
                    })
                    .collect()
            })
        })
        .await;
        let result = match result {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("internal error: {e}")),
        };
        let values = result
            .snapped
            .as_ref()
            .map(|s| s.values.clone())
            .unwrap_or_else(|| result.best.clone());
        let feasible = result
            .snapped
            .as_ref()
            .map(|s| s.feasible)
            .unwrap_or(result.feasible);
        let mut best = serde_json::Map::new();
        let mut t = if corners.is_empty() {
            format!(
                "{} after {} simulations ({:?}).\n",
                if feasible {
                    "All specs pass"
                } else {
                    "Specs not all met"
                },
                result.evaluations,
                result.stop
            )
        } else {
            format!(
                "{} after {} candidates, each at {} corners ({:?}).\n",
                if feasible {
                    "All specs pass at every corner"
                } else {
                    "Specs not all met at every corner"
                },
                result.evaluations,
                corners.len(),
                result.stop
            )
        };
        if let Some(note) = &budget_note {
            t.push_str(&format!("  note: {note}\n"));
        }
        for (name, v) in result.names.iter().zip(&values) {
            best.insert(name.clone(), json!(units::format(*v)));
            t.push_str(&format!("  {name} = {}\n", units::format(*v)));
        }
        if input.apply {
            let ops: Vec<EditOp> = result
                .names
                .iter()
                .zip(&values)
                .filter(|(n, _)| !n.starts_with('.'))
                .map(|(n, v)| EditOp::SetValue {
                    name: n.clone(),
                    value: units::format(*v),
                })
                .collect();
            let summary = ops
                .iter()
                .map(|o| match o {
                    EditOp::SetValue { name, value } => format!("{name} = {value}"),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join(", ");
            match p.load(&input.circuit) {
                Ok((mut sch, _)) => match apply(&mut sch, p.library(), &ops) {
                    Ok(_)
                        if !self
                            .ws
                            .approve(
                                &input.circuit,
                                &format!("Apply optimized values: {summary}"),
                                &summary,
                            )
                            .await =>
                    {
                        t.push_str("The user declined applying these values; the schematic is unchanged.\n");
                    }
                    Ok(_) => match p.save(&input.circuit, &sch, "Optimized values") {
                        Ok(_) => {
                            if let Ok(path) = p.resolve(&input.circuit) {
                                self.ws.after_save(&path);
                            }
                            t.push_str("Applied to the schematic (undo is available).\n");
                        }
                        Err(e) => t.push_str(&format!("Could not save: {e}\n")),
                    },
                    Err(e) => t.push_str(&format!(
                        "Could not apply: {e} (.param values must be edited with edit_schematic)\n"
                    )),
                },
                Err(e) => t.push_str(&format!("Could not load: {e}\n")),
            }
        }
        // Re-check the values actually reported (snapped) with fresh runs,
        // at every corner when there are corners.
        let mut corner_report = None;
        let final_report = match optimize::apply(&netlist, &input.params, &values) {
            Ok(n) => {
                let (labels, variants): (Vec<String>, Vec<(String, Netlist)>) =
                    if corners.is_empty() {
                        (vec![String::new()], vec![(String::new(), n)])
                    } else {
                        corners
                            .iter()
                            .map(|c| {
                                let at = corner::apply(&n, c, &corner::no_lookup)
                                    .unwrap_or_else(|_| n.clone());
                                (c.name.clone(), (c.name.clone(), at))
                            })
                            .unzip()
                    };
                let results = run_variants(
                    &self.ws,
                    &input.circuit,
                    variants,
                    &std_libs,
                    input.simulator.as_deref(),
                    &sources,
                    ctx,
                )
                .await;
                let reports: Vec<Result<SpecReport, String>> = results
                    .iter()
                    .map(|(_, r)| {
                        r.as_ref()
                            .map(|v| evaluate(&specs, &v.datasets))
                            .map_err(Clone::clone)
                    })
                    .collect();
                if corners.is_empty() {
                    reports.into_iter().next().and_then(Result::ok)
                } else {
                    let r = corner::summarize(&specs, &labels[..reports.len()], &reports);
                    let worst = r.worst.clone();
                    corner_report = Some(r);
                    Some(worst)
                }
            }
            Err(_) => None,
        };
        match (&corner_report, &final_report) {
            (Some(c), _) => t.push_str(&analysis::corner_text(c)),
            (None, Some(r)) => t.push_str(&spec_text(r)),
            _ => {}
        }
        ToolOutput::text(t).with_data(json!({"kind": "optimize", "best": best, "evaluations": result.evaluations, "report": final_report.as_ref().map(ui_spec_report), "corners": corner_report.as_ref().map(analysis::corner_json)}))
    }
}

const NEEDS_NGSPICE: &str = "Pole-zero analysis needs ngspice, and it was not found on this machine. Install it (Debian or Ubuntu: `sudo apt install ngspice`; Fedora: `sudo dnf install ngspice`; macOS: `brew install ngspice`; Windows: the zip from https://ngspice.sourceforge.io with its bin folder on PATH) and try again. LTspice and Xyce have no .pz analysis.";

#[derive(Debug, Deserialize, JsonSchema, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Transfer {
    /// Voltage gain: V(output) / V(input).
    #[default]
    Vol,
    /// Transimpedance: V(output) / I(input), for a current driven into the
    /// input pair.
    Cur,
}

/// A number as JSON or as SPICE text (`2.2k`).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ParamValue {
    Value(f64),
    Text(String),
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PolesZerosInput {
    pub circuit: String,
    /// Positive input node (a net name as read_schematic shows it).
    pub input: String,
    /// Negative input node. Default: ground (`0`).
    #[serde(default)]
    pub input_neg: Option<String>,
    /// Positive output node.
    pub output: String,
    /// Negative output node. Default: ground (`0`).
    #[serde(default)]
    pub output_neg: Option<String>,
    /// `vol` for voltage gain (default) or `cur` for transimpedance.
    #[serde(default)]
    pub transfer: Transfer,
    /// Values for this analysis only, without changing the file: an element
    /// (`R1`) or `.param` name and its value, e.g. `{"R1": "2.2k"}`.
    #[serde(default)]
    pub params: BTreeMap<String, ParamValue>,
}

pub struct PolesZeros {
    pub ws: Arc<Workspace>,
}

/// A node for the `.pz` line: ground, or a node the circuit has, spelled as
/// the netlist spells it. Checking against the netlist also keeps anything
/// but a node name out of the deck.
fn pz_node(netlist: &Netlist, given: &str) -> Result<String, String> {
    let name = given.trim();
    let lower = name.to_ascii_lowercase();
    let name = lower
        .strip_prefix("v(")
        .and_then(|s| s.strip_suffix(')'))
        .map_or(name, |inner| &name[2..2 + inner.len()]);
    if name == "0" || name.eq_ignore_ascii_case("gnd") {
        return Ok("0".into());
    }
    let nodes = netlist.nodes();
    nodes
        .iter()
        .find(|n| n.eq_ignore_ascii_case(name))
        .cloned()
        .ok_or_else(|| {
            let shown: Vec<&str> = nodes
                .iter()
                .map(String::as_str)
                .filter(|n| *n != "0")
                .take(60)
                .collect();
            format!(
                "the circuit has no node `{given}`; ground is `0` and its other nodes are: {}",
                shown.join(", ")
            )
        })
}

/// Directives left out when a tool runs one analysis of its own (`.pz`,
/// `.op`). ngspice runs every analysis in a deck, `.meas`, `.four` and print
/// lines belong to another analysis, `.step` is not ngspice syntax, and the
/// tool decides what is saved (with a `.save` line ngspice saves no
/// pole-zero data and skips the analysis).
fn left_out_of_own_run(directive: &str) -> bool {
    let kw = directive
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    aispice_core::netlist::spice::is_analysis(directive)
        || matches!(
            kw.as_str(),
            ".meas"
                | ".measure"
                | ".step"
                | ".save"
                | ".four"
                | ".fourier"
                | ".print"
                | ".plot"
                | ".disto"
                | ".net"
                | ".wave"
        )
}

/// The circuit with `analysis` as its only analysis and `extra` directives
/// added, for a tool's own run.
pub fn only_analysis(netlist: &Netlist, analysis: &str, extra: Vec<String>) -> Netlist {
    let mut n = netlist.clone();
    n.items
        .retain(|l| !matches!(l, Line::Directive { text } if left_out_of_own_run(text)));
    crate::runner::apply_mods(
        &mut n,
        &RunMods {
            analysis: Some(analysis.to_string()),
            extra,
        },
    );
    n
}

/// Set run-only values (`{"R1": "2.2k"}`) in a netlist.
fn apply_params(
    netlist: &mut Netlist,
    params: &BTreeMap<String, ParamValue>,
) -> Result<(), String> {
    for (name, value) in params {
        let v = match value {
            ParamValue::Value(v) => Some(*v),
            ParamValue::Text(t) => aispice_sim::expr::parse_number(t),
        };
        let Some(v) = v else {
            return Err(format!("params: the value of {name} is not a number"));
        };
        sweep::set_value(netlist, name, v).map_err(|e| format!("params: {e}"))?;
    }
    Ok(())
}

/// `, with R1=2.2k C1=10n` for a report heading, or nothing.
fn params_text(params: &BTreeMap<String, ParamValue>) -> String {
    if params.is_empty() {
        return String::new();
    }
    let set: Vec<String> = params
        .iter()
        .map(|(k, v)| match v {
            ParamValue::Value(v) => format!("{k}={}", units::format(*v)),
            ParamValue::Text(s) => format!("{k}={s}"),
        })
        .collect();
    format!(", with {}", set.join(" "))
}

fn roots_json(roots: &[Root]) -> Vec<Value> {
    roots
        .iter()
        .map(|r| json!({"re_hz": r.re_hz, "im_hz": r.im_hz, "f0_hz": r.f0_hz(), "q": r.q()}))
        .collect()
}

#[async_trait]
impl Tool for PolesZeros {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        spec::<PolesZerosInput>(
            "poles_zeros",
            "Pole-zero analysis of a transfer function with ngspice's .pz: the poles and zeros of V(output)/V(input), or V(output)/I(input) with transfer `cur`, in Hz, with f0, Q and damping ratio for each complex pair and the corner of each real root, and whether the circuit is stable (any pole in the right half-plane means it is not). Nonlinear parts are linearised at the operating point, as in an AC analysis. The circuit's own analyses, .meas, .step and .save lines are left out of this run, and the file is not changed. Needs ngspice: LTspice and Xyce have no .pz analysis.",
        )
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: PolesZerosInput = input!(input);
        let have_ngspice = self
            .ws
            .runner
            .detect()
            .await
            .iter()
            .any(|(id, d)| *id == SimId::Ngspice && d.found);
        if !have_ngspice {
            return ToolOutput::error(NEEDS_NGSPICE);
        }
        let p = match self.ws.project() {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let (mut netlist, std_libs, warnings) = match self.ws.runner.netlist_for(&p, &input.circuit)
        {
            Ok(v) => v,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let neg = |n: &Option<String>| n.clone().unwrap_or_else(|| "0".into());
        let nodes = [
            input.input.clone(),
            neg(&input.input_neg),
            input.output.clone(),
            neg(&input.output_neg),
        ]
        .map(|n| pz_node(&netlist, &n));
        let [in_p, in_n, out_p, out_n] = match nodes {
            [Ok(a), Ok(b), Ok(c), Ok(d)] => [a, b, c, d],
            other => {
                let e = other.into_iter().find_map(Result::err).unwrap_or_default();
                return ToolOutput::error(e);
            }
        };
        if let Err(e) = apply_params(&mut netlist, &input.params) {
            return ToolOutput::error(e);
        }
        let kind = match input.transfer {
            Transfer::Vol => "vol",
            Transfer::Cur => "cur",
        };
        let netlist = only_analysis(
            &netlist,
            &format!(".pz {in_p} {in_n} {out_p} {out_n} {kind} pz"),
            Vec::new(),
        );
        // Not kept as a run: measure and plot keep using the circuit's own
        // latest simulation.
        let run = match self
            .ws
            .runner
            .run_netlist_with(
                &p,
                &input.circuit,
                &netlist,
                &std_libs,
                Some("ngspice"),
                &ctx.cancel,
                false,
            )
            .await
        {
            Ok(r) => r,
            Err(crate::runner::RunError::Unavailable(_)) => {
                return ToolOutput::error(NEEDS_NGSPICE);
            }
            Err(e) => return ToolOutput::error(format!("Pole-zero analysis failed: {e}")),
        };
        let Some(pz) = PoleZero::from_datasets(&run.output.datasets) else {
            let mut t = String::from("ngspice returned no pole-zero results.");
            for e in run
                .output
                .errors
                .iter()
                .chain(&run.output.warnings)
                .take(10)
            {
                t.push_str(&format!("\n  {e}"));
            }
            return ToolOutput::error(t);
        };
        let pair = |p: &str, n: &str| {
            if n == "0" {
                p.to_string()
            } else {
                format!("{p},{n}")
            }
        };
        let source = match input.transfer {
            Transfer::Vol => "V",
            Transfer::Cur => "I",
        };
        let mut t = format!(
            "Poles and zeros of V({})/{source}({}) in {} (ngspice .pz{}):\n",
            pair(&out_p, &out_n),
            pair(&in_p, &in_n),
            input.circuit,
            params_text(&input.params)
        );
        t.push_str(&pz.report());
        for n in warnings.iter().chain(&run.notes) {
            t.push_str(&format!("  note: {n}\n"));
        }
        for w in run.output.warnings.iter().take(10) {
            t.push_str(&format!("  warning: {w}\n"));
        }
        ToolOutput::text(t).with_data(json!({
            "kind": "poles_zeros",
            "poles": roots_json(&pz.poles),
            "zeros": roots_json(&pz.zeros),
            "stable": pz.stability() == Stability::Stable,
        }))
    }
}

const OP_NEEDS_NGSPICE: &str = "Device parameters (region, gm, gds, gm/id and the rest) need ngspice, which was not found on this machine. Install it (Debian or Ubuntu: `sudo apt install ngspice`; Fedora: `sudo dnf install ngspice`; macOS: `brew install ngspice`; Windows: the zip from https://ngspice.sourceforge.io with its bin folder on PATH) and try again.";

/// At most this many devices have their quantities saved in one run.
const MAX_OP_DEVICES: usize = 400;
/// At most this many device lines go into the text.
const MAX_OP_LINES: usize = 60;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OperatingPointInput {
    pub circuit: String,
    /// Values for this analysis only, without changing the file: an element
    /// (`R1`) or `.param` name and its value, e.g. `{"Rbias": "47k"}`.
    #[serde(default)]
    pub params: BTreeMap<String, ParamValue>,
    /// Report only these devices: instance names such as `M1`, `X1.M7` for
    /// M7 inside subcircuit instance X1, or `X1` for every device in it.
    /// Default: every MOSFET, BJT and diode.
    #[serde(default)]
    pub devices: Vec<String>,
}

pub struct OperatingPoint {
    pub ws: Arc<Workspace>,
}

/// `Nodes: V(a)=1V, ...` and `Source currents: ...` lines.
fn op_nodes_text(nodes: &[(String, f64)]) -> String {
    let list = |volts: bool, unit: &str| {
        let items: Vec<String> = nodes
            .iter()
            .filter(|(k, _)| k.starts_with("V(") == volts)
            .take(60)
            .map(|(k, v)| format!("{k}={}", units::format_with_unit(*v, unit)))
            .collect();
        items.join(", ")
    };
    let mut t = String::new();
    let (v, i) = (list(true, "V"), list(false, "A"));
    if !v.is_empty() {
        t.push_str(&format!("Nodes: {v}\n"));
    }
    if !i.is_empty() {
        t.push_str(&format!(
            "Source currents (into the + terminal, so negative means the source delivers it): {i}\n"
        ));
    }
    t
}

fn op_nodes_json(nodes: &[(String, f64)]) -> serde_json::Map<String, Value> {
    nodes.iter().map(|(k, v)| (k.clone(), json!(v))).collect()
}

impl OperatingPoint {
    /// One `.op` run, not kept: measure and plot keep using the circuit's
    /// own latest simulation.
    async fn run(
        &self,
        p: &crate::project::Project,
        circuit: &str,
        netlist: &Netlist,
        std_libs: &[String],
        simulator: Option<&str>,
        ctx: &ToolContext,
    ) -> Result<Arc<StoredRun>, String> {
        self.ws
            .runner
            .run_netlist_with(p, circuit, netlist, std_libs, simulator, &ctx.cancel, false)
            .await
            .map_err(|e| format!("The operating point failed: {e}"))
    }
}

#[async_trait]
impl Tool for OperatingPoint {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        spec::<OperatingPointInput>(
            "operating_point",
            "Check bias: the DC operating point with each semiconductor device's small-signal values, for sizing transistors (the gm/Id method). MOSFETs: region (cutoff, subthreshold, triode, saturation), id, vgs, vds, vbs, vth (von for level 1 to 3), vdsat, gm, gds, gmbs, gm/id, intrinsic gain gm/gds, cgs and cgd when the model has them, W and L. BJTs: region (active, saturation, cutoff, reverse active), ic, ib, beta, vbe, vce, gm, rpi, ro. Diodes: id, vd, rd. Also node voltages and source currents. Devices inside subcircuits are named by path, as X1.M7. Points out devices in triode or cutoff that look like they should be saturated, such as a current-mirror output. Values are in each device's own polarity (for a PMOS, vgs is vsg). The circuit's own analyses, .meas, .step and .save lines are left out of this run and the file is not changed. Device values need ngspice; without it only node voltages and source currents come back, from LTspice or Xyce.",
        )
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: OperatingPointInput = input!(input);
        let p = match self.ws.project() {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let (mut netlist, std_libs, warnings) = match self.ws.runner.netlist_for(&p, &input.circuit)
        {
            Ok(v) => v,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        if let Err(e) = apply_params(&mut netlist, &input.params) {
            return ToolOutput::error(e);
        }
        let heading = |sim: &str| {
            format!(
                "Operating point of {}{} ({sim} .op):\n",
                input.circuit,
                params_text(&input.params)
            )
        };
        let have_ngspice = self
            .ws
            .runner
            .detect()
            .await
            .iter()
            .any(|(id, d)| *id == SimId::Ngspice && d.found);
        let mut notes: Vec<String> = warnings;

        if !have_ngspice {
            let deck = only_analysis(&netlist, ".op", Vec::new());
            let run = match self
                .run(&p, &input.circuit, &deck, &std_libs, None, ctx)
                .await
            {
                Ok(r) => r,
                Err(e) => return ToolOutput::error(format!("{e}\n{OP_NEEDS_NGSPICE}")),
            };
            let nodes = oppoint::node_values(&run.output.datasets);
            let mut t = heading(run.simulator.name());
            t.push_str(&op_nodes_text(&nodes));
            t.push_str(&format!(
                "Only node voltages and source currents: {OP_NEEDS_NGSPICE}\n"
            ));
            for n in notes.iter().chain(&run.notes) {
                t.push_str(&format!("  note: {n}\n"));
            }
            return ToolOutput::text(t).with_data(json!({
                "kind": "operating_point",
                "simulator": run.simulator,
                "nodes": op_nodes_json(&nodes),
                "devices": [],
                "checks": [],
                "note": OP_NEEDS_NGSPICE,
            }));
        }

        // First run: `savecurrents` lists every device by its full name.
        let deck = only_analysis(&netlist, ".op", vec![".options savecurrents".into()]);
        let first = match self
            .run(&p, &input.circuit, &deck, &std_libs, Some("ngspice"), ctx)
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(e),
        };
        let resolved = aispice_sim::models::resolve_with(
            &netlist,
            &std_libs,
            &self.ws.runner.model_lib_dirs(),
        )
        .netlist;
        let found: Vec<(String, Option<oppoint::Located>)> =
            oppoint::discover(&first.output.datasets)
                .into_iter()
                .map(|id| {
                    let at = oppoint::locate(&resolved, &id);
                    (id, at)
                })
                .collect();
        let name_of = |(id, at): &(String, Option<oppoint::Located>)| {
            at.as_ref()
                .map_or_else(|| oppoint::display_name(id), |l| l.name.clone())
        };
        let mut chosen: Vec<(String, Option<oppoint::Located>)> = if input.devices.is_empty() {
            found.clone()
        } else {
            found
                .iter()
                .filter(|d| {
                    let name = name_of(d);
                    input
                        .devices
                        .iter()
                        .any(|f| oppoint::matches(&name, &d.0, f))
                })
                .cloned()
                .collect()
        };
        if chosen.is_empty() && !input.devices.is_empty() {
            let names: Vec<String> = found.iter().map(name_of).take(80).collect();
            return ToolOutput::error(if names.is_empty() {
                "the circuit has no MOSFETs, BJTs or diodes".to_string()
            } else {
                format!(
                    "no device matches {}; the circuit's MOSFETs, BJTs and diodes are: {}",
                    input.devices.join(", "),
                    names.join(", ")
                )
            });
        }
        let skipped = chosen.len().saturating_sub(MAX_OP_DEVICES);
        chosen.truncate(MAX_OP_DEVICES);

        // Second run: each device's quantities.
        let mut devices: Vec<oppoint::Device> = Vec::new();
        let mut run = first.clone();
        if !chosen.is_empty() {
            let ids: Vec<String> = chosen.iter().map(|(id, _)| id.clone()).collect();
            let deck = only_analysis(&netlist, ".op", oppoint::save_directives(&ids));
            match self
                .run(&p, &input.circuit, &deck, &std_libs, Some("ngspice"), ctx)
                .await
            {
                Ok(second) => {
                    let missing = oppoint::unrecognized(&second.output.log);
                    let values = oppoint::quantities(&second.output.datasets, &missing);
                    let none = BTreeMap::new();
                    devices = chosen
                        .iter()
                        .filter_map(|(id, at)| {
                            oppoint::device(id, values.get(id).unwrap_or(&none), at.as_ref())
                        })
                        .collect();
                    oppoint::sort(&mut devices);
                    run = second;
                }
                Err(e) => notes.push(format!("device values could not be read: {e}")),
            }
        }

        let nodes = oppoint::node_values(&run.output.datasets);
        let checks = oppoint::checks(&devices);
        let mut t = heading("ngspice");
        t.push_str(&op_nodes_text(&nodes));
        if devices.is_empty() {
            if found.is_empty() {
                t.push_str("No MOSFETs, BJTs or diodes in this circuit.\n");
            }
        } else {
            t.push_str("Devices (values in each device's own polarity: for a PMOS vgs is vsg and vds is vsd, for a PNP vbe is veb and vce is vec, and their currents count positive from source to drain or emitter to collector):\n");
            for d in devices.iter().take(MAX_OP_LINES) {
                t.push_str(&format!("  {}\n", oppoint::line(d)));
            }
            let more = devices.len().saturating_sub(MAX_OP_LINES) + skipped;
            if more > 0 {
                t.push_str(&format!(
                    "  and {more} more; pass `devices` to see particular ones\n"
                ));
            }
        }
        if !checks.is_empty() {
            t.push_str("Check:\n");
            for c in &checks {
                t.push_str(&format!("  {c}\n"));
            }
        }
        for n in &run.notes {
            if !notes.contains(n) {
                notes.push(n.clone());
            }
        }
        for n in &notes {
            t.push_str(&format!("  note: {n}\n"));
        }
        for e in &run.output.errors {
            t.push_str(&format!("  error: {e}\n"));
        }
        let shown_warnings = run
            .output
            .warnings
            .iter()
            .filter(|w| !oppoint::is_unrecognized_warning(w));
        for w in shown_warnings.take(5) {
            t.push_str(&format!("  warning: {w}\n"));
        }
        let devices_json: Vec<Value> = devices
            .iter()
            .map(|d| {
                json!({
                    "name": d.name,
                    "type": d.type_name(),
                    "region": d.region,
                    "params": d.params,
                    "notes": d.notes,
                })
            })
            .collect();
        ToolOutput::text(t).with_data(json!({
            "kind": "operating_point",
            "simulator": SimId::Ngspice,
            "nodes": op_nodes_json(&nodes),
            "devices": devices_json,
            "checks": checks,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Oversized requests fail before anything is built; defaults shrink to
    /// fit, and say so.
    #[test]
    fn run_budgets_are_checked_from_the_counts() {
        assert_eq!(mc_runs(None, 1).unwrap(), (50, None));
        assert_eq!(mc_runs(Some(500), 1).unwrap(), (500, None));
        assert_eq!(mc_runs(Some(0), 1).unwrap(), (1, None));
        let (n, note) = mc_runs(None, 20).unwrap();
        assert_eq!(n, 25);
        assert!(note.unwrap().contains("25 runs per corner"));
        let err = mc_runs(Some(30), 20).unwrap_err();
        assert!(err.contains("at most 25 runs"), "{err}");
        assert!(mc_runs(Some(usize::MAX), 200).is_err());
        assert!(mc_runs(Some(501), 1).is_err());

        assert_eq!(opt_candidates(None, 1).unwrap(), (80, None));
        assert_eq!(opt_candidates(Some(1000), 1).unwrap(), (400, None));
        let (n, note) = opt_candidates(None, 50).unwrap();
        assert_eq!(n, 40);
        assert!(note.unwrap().contains("40 candidates"));
        let err = opt_candidates(Some(100), 50).unwrap_err();
        assert!(err.contains("at most 40"), "{err}");
        assert_eq!(opt_candidates(None, 200).unwrap().0, 10);
    }

    /// The measure and check_specs descriptions must say what bandwidth_3db
    /// is measured from and how freq_at_db differs, and name every kind.
    #[test]
    fn measurement_help_explains_the_reference_levels() {
        for kind in aispice_sim::measure::KINDS {
            assert!(
                MEASURE_SYNTAX.contains(kind),
                "{kind} missing from the help"
            );
        }
        for words in [
            "below the gain at the lowest swept frequency",
            "absolute level",
            "poles_zeros",
        ] {
            assert!(MEASURE_SYNTAX.contains(words), "`{words}` missing");
        }
    }
}
