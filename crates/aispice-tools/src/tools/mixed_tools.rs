//! Mixed-signal work: the project's behavioural models, spec budgets that
//! split a chip-level target into block specs, and trim range checks.

use super::analysis;
use super::schematic_tools::spec;
use super::sim_tools::{CORNERS_HELP, SPEC_SYNTAX, load_specs};
use crate::workspace::{FileWrite, Workspace};
use aispice_agent::tool::parse_input;
use aispice_agent::{Access, Tool, ToolContext, ToolOutput, ToolSpec};
use aispice_core::netlist::Netlist;
use aispice_sim::behavioral::{Direction, Library, LibraryKind};
use aispice_sim::budget::{self, AdcFrontEnd, Ldo};
use aispice_sim::corner::{self, CornerSet};
use aispice_sim::montecarlo::{self, Mismatch, Tolerance};
use aispice_sim::spec::evaluate;
use aispice_sim::sweep;
use aispice_sim::trim::{self, CodeResult, SampleResult};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

macro_rules! input {
    ($v:expr) => {
        match parse_input($v) {
            Ok(i) => i,
            Err(e) => return e,
        }
    };
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BehavioralAction {
    /// The declared models, their modules and ports, and what this session
    /// has built.
    List,
    /// Compile the declared models (or the one named) now.
    Build,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BehavioralInput {
    pub action: BehavioralAction,
    /// For `build`: a Verilog-A file path or a co-simulation model name.
    #[serde(default)]
    pub model: Option<String>,
}

const BEHAVIORAL: &str = r#"
Behavioural models the project declares in aispice.toml: Verilog-A files (`veriloga = ["models/x.va"]`), compiled by OpenVAF, and Verilog to co-simulate (`[[cosim]]` with `model`, `sources` and optionally `top`), compiled by Verilator for ngspice's d_cosim. A deck uses a Verilog-A module as a model type (`.model cmp comparator_va(gain=1e3)` and `N1 inp inn out cmp`) and a co-simulated model by name on an XSPICE instance (`A1 [clk en] [q3 q2 q1 q0] null counter`), bridged to analog nodes with adc_bridge and dac_bridge models. A deck never names a compiled file, and d_cosim or d_process models in a deck are refused: aispice checks the sources, builds them itself and loads them on ngspice, the only simulator that runs them. `list` shows the declared models with their ports and the instance line to use; `build` compiles now and reports compiler errors, which a simulation would otherwise report on first use. Only the user declares models, by editing aispice.toml.
"#;

pub struct BehavioralModels {
    pub ws: Arc<Workspace>,
}

/// The instance a deck writes for a library: the ports in order, buses
/// most significant bit first.
fn usage(lib: &Library) -> String {
    match lib.kind {
        LibraryKind::Cosim => {
            let [i, o, io] = lib.pin_groups();
            // Node names for the pins: `q[3]` becomes `q3`, a name a deck
            // can use.
            let group = |g: &[String]| {
                if g.is_empty() {
                    "null".to_string()
                } else {
                    let nodes: Vec<String> = g.iter().map(|p| p.replace(['[', ']'], "")).collect();
                    format!("[{}]", nodes.join(" "))
                }
            };
            format!("A1 {} {} {} {}", group(&i), group(&o), group(&io), lib.name)
        }
        LibraryKind::Osdi => lib
            .modules
            .iter()
            .map(|m| {
                let ports: Vec<&str> = m.ports.iter().map(|p| p.name.as_str()).collect();
                format!("N1 {} m1 with `.model m1 {}`", ports.join(" "), m.name)
            })
            .collect::<Vec<_>>()
            .join("; "),
    }
}

fn ports_text(ports: &[aispice_sim::behavioral::Port]) -> String {
    ports
        .iter()
        .map(|p| {
            let dir = match p.direction {
                Direction::Input => "in",
                Direction::Output => "out",
                Direction::Inout => "inout",
            };
            if p.width > 1 {
                format!("{} {dir}[{}]", p.name, p.width)
            } else {
                format!("{} {dir}", p.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn library_json(lib: &Library) -> Value {
    json!({
        "kind": match lib.kind { LibraryKind::Osdi => "veriloga", LibraryKind::Cosim => "cosim" },
        "name": lib.name,
        "file": lib.file_name(),
        "sha256": lib.sha256,
        "tool": lib.tool,
        "modules": lib.modules,
        "usage": usage(lib),
    })
}

#[async_trait]
impl Tool for BehavioralModels {
    /// Builds write only aispice's record under `.aispice/build/`.
    fn access(&self) -> Access {
        Access::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        spec::<BehavioralInput>("behavioral_models", BEHAVIORAL)
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: BehavioralInput = input!(input);
        let p = match self.ws.project() {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        match input.action {
            BehavioralAction::List => {
                let declared = match crate::behavioral::declared(&p) {
                    Ok(d) => d,
                    Err(e) => return ToolOutput::error(e),
                };
                if declared.is_empty() {
                    return ToolOutput::text(
                        "aispice.toml declares no behavioural models. The user adds Verilog-A files with `veriloga = [\"models/x.va\"]` and Verilog to co-simulate with a [[cosim]] table (model, sources, top).\n",
                    )
                    .with_data(json!({"kind": "generic", "value": {"veriloga": [], "cosim": []}}));
                }
                let mut t = String::new();
                for v in &declared.veriloga {
                    for m in &v.modules {
                        t.push_str(&format!(
                            "Verilog-A {} module {}: {}; use `.model <name> {}` and `N1 <{} nodes> <name>`\n",
                            v.source.name,
                            m.name,
                            ports_text(&m.ports),
                            m.name,
                            m.ports.len()
                        ));
                    }
                }
                for c in &declared.cosim {
                    t.push_str(&format!(
                        "co-simulated model {} (top module {}) from {}; build it to see its pins in order\n",
                        c.model,
                        c.top,
                        c.sources
                            .iter()
                            .map(|s| s.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                let built = self.ws.runner.behavioral_built().await;
                for lib in &built {
                    t.push_str(&format!(
                        "built this session: {} (SHA-256 {}): {}\n",
                        lib.file_name(),
                        &lib.sha256[..16],
                        usage(lib)
                    ));
                }
                let va: Vec<Value> = declared
                    .veriloga
                    .iter()
                    .map(|v| json!({"file": v.source.name, "modules": v.modules}))
                    .collect();
                let cosim: Vec<Value> = declared
                    .cosim
                    .iter()
                    .map(|c| json!({"model": c.model, "top": c.top, "modules": c.modules}))
                    .collect();
                let built: Vec<Value> = built.iter().map(|l| library_json(l)).collect();
                ToolOutput::text(t).with_data(json!({"kind": "generic", "value": {"veriloga": va, "cosim": cosim, "built": built}}))
            }
            BehavioralAction::Build => {
                match self
                    .ws
                    .runner
                    .build_behavioral(&p, input.model.as_deref(), &ctx.cancel)
                    .await
                {
                    Ok(libs) => {
                        let mut t = String::new();
                        for lib in &libs {
                            let ports = lib
                                .modules
                                .iter()
                                .map(|m| format!("{} ({})", m.name, ports_text(&m.ports)))
                                .collect::<Vec<_>>()
                                .join("; ");
                            t.push_str(&format!(
                                "built {} with {} (SHA-256 {}): {ports}\n  use: {}\n",
                                lib.file_name(),
                                lib.tool,
                                &lib.sha256[..16],
                                usage(lib)
                            ));
                        }
                        let data: Vec<Value> = libs.iter().map(|l| library_json(l)).collect();
                        ToolOutput::text(t)
                            .with_data(json!({"kind": "generic", "value": {"built": data}}))
                    }
                    Err(e) => ToolOutput::error(e),
                }
            }
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetKind {
    AdcFrontend,
    Ldo,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BudgetInput {
    pub kind: BudgetKind,
    /// For `adc_frontend`.
    #[serde(default)]
    pub adc_frontend: Option<AdcFrontEnd>,
    /// For `ldo`.
    #[serde(default)]
    pub ldo: Option<Ldo>,
    /// Save block specs: block name (`ota` for the ADC front end;
    /// `error_amp` and `ldo` for the LDO) to a `.specs` file in the project,
    /// usually the block circuit's, so check_specs and optimize read it.
    #[serde(default)]
    pub save: BTreeMap<String, String>,
}

const BUDGET: &str = r#"
Split a chip-level target into the specs each block must meet, with first-order equations shown step by step with their numbers. `adc_frontend`: an ADC's sample-and-hold or residue stage from bits, ENOB (or SNR), full scale, sample rate and feedback factor to the sampling capacitor (kT/C) and the OTA's DC gain, gain-bandwidth (settling to half an LSB), slew rate, phase margin and input noise density. `ldo`: a regulator from dropout, load regulation, PSRR, output noise and quiescent current to the divider, the pass device, the loop gain and bandwidth, and the error amplifier's gain and gain-bandwidth. Block specs come in check_specs' form; `save` writes them to `.specs` files (approved like an edit), ready for check_specs and optimize on a block such as the two_stage or ldo template.
"#;

pub struct BudgetTool {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for BudgetTool {
    fn access(&self) -> Access {
        Access::Edit
    }

    fn spec(&self) -> ToolSpec {
        spec::<BudgetInput>("budget", BUDGET)
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: BudgetInput = input!(input);
        let result = match (&input.kind, &input.adc_frontend, &input.ldo) {
            (BudgetKind::AdcFrontend, Some(p), _) => budget::adc_frontend(p),
            (BudgetKind::Ldo, _, Some(p)) => budget::ldo(p),
            (BudgetKind::AdcFrontend, None, _) => {
                return ToolOutput::error("kind adc_frontend needs adc_frontend parameters");
            }
            (BudgetKind::Ldo, _, None) => {
                return ToolOutput::error("kind ldo needs ldo parameters");
            }
        };
        let b = match result {
            Ok(b) => b,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let mut t = format!("{} budget:\n{}", b.kind, b.to_text());
        for block in &b.blocks {
            t.push_str(&format!(
                "Specs for the {} ({}):\n",
                block.block, block.bench
            ));
            for s in &block.specs {
                t.push_str(&format!("  {s}\n"));
            }
        }
        let mut writes = Vec::new();
        for (name, file) in &input.save {
            let Some(block) = b.blocks.iter().find(|bl| bl.block == *name) else {
                let names: Vec<&str> = b.blocks.iter().map(|bl| bl.block.as_str()).collect();
                return ToolOutput::error(format!(
                    "this budget has no block {name}; its blocks are {}",
                    names.join(", ")
                ));
            };
            if !file.to_ascii_lowercase().ends_with(".specs") {
                return ToolOutput::error(format!("{file} must be a .specs file"));
            }
            writes.push(FileWrite {
                rel: file.clone(),
                bytes: block.file_text(&b.kind).into_bytes(),
            });
        }
        let mut saved = Vec::new();
        if !writes.is_empty() {
            let preview: String = writes
                .iter()
                .map(|w| format!("{}:\n{}", w.rel, String::from_utf8_lossy(&w.bytes)))
                .collect();
            match self
                .ws
                .write_files(
                    self,
                    &writes,
                    &format!("Save the {} budget's block specs", b.kind),
                    &preview,
                )
                .await
            {
                Ok(done) => {
                    t.push_str(&format!("Saved {}.\n", done.join(", ")));
                    saved = done;
                }
                Err(e) => return ToolOutput::error(e),
            }
        }
        ToolOutput::text(t)
            .with_data(json!({"kind": "generic", "value": {"budget": b, "saved": saved}}))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TrimInput {
    pub circuit: String,
    /// The `.param` the trim DAC reads, such as `trim`.
    pub param: String,
    /// The first and last trim codes (at most 64 codes).
    pub first: i64,
    pub last: i64,
    /// The code without trimming, for the untrimmed yield (default: the
    /// middle of the range).
    #[serde(default)]
    pub nominal: Option<i64>,
    /// Specs to meet; default: the `<circuit>.specs` file.
    #[serde(default)]
    pub specs: Option<String>,
    #[serde(default)]
    pub corners: Option<CornerSet>,
    /// Device mismatch from the circuit's corner libraries, as monte_carlo
    /// takes it.
    #[serde(default)]
    pub mismatch: Option<Mismatch>,
    /// Part tolerances, as monte_carlo takes them.
    #[serde(default)]
    pub tolerances: Vec<Tolerance>,
    /// Monte Carlo samples per corner (default 0: corners only). Samples
    /// times corners times codes may be at most 2000 runs.
    #[serde(default)]
    pub runs: usize,
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub simulator: Option<String>,
}

/// Simulations one trim check may run.
const MAX_TRIM_RUNS: usize = 2000;
/// Codes one trim check may try.
const MAX_TRIM_CODES: usize = 64;

pub struct TrimCheck {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for TrimCheck {
    fn access(&self) -> Access {
        Access::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        spec::<TrimInput>(
            "trim_check",
            &format!(
                "Prove a trimmed block can always be trimmed into range: for every corner and every Monte Carlo sample (mismatch or part tolerances, as monte_carlo takes them), simulate every trim code (the `.param` the trim DAC reads, from `first` to `last`) and report whether some code brings every spec into range, the samples none does with how close they come, the code each sample needs as a histogram, how many codes are left unused at each end of the range, and the untrimmed yield at the nominal code. Every code is simulated, so the answer holds even where the spec does not move monotonically with the code. {CORNERS_HELP} {SPEC_SYNTAX}"
            ),
        )
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: TrimInput = input!(input);
        match trim_check(&self.ws, ctx, &input).await {
            Ok(out) => out,
            Err(e) => ToolOutput::error(e),
        }
    }
}

async fn trim_check(
    ws: &Workspace,
    ctx: &ToolContext,
    input: &TrimInput,
) -> Result<ToolOutput, String> {
    if input.last < input.first {
        return Err("last must not be below first".into());
    }
    let ncodes = (input.last - input.first + 1) as usize;
    if ncodes > MAX_TRIM_CODES {
        return Err(format!(
            "{ncodes} codes is more than the {MAX_TRIM_CODES} a trim check tries"
        ));
    }
    let (specs, _) = load_specs(ws, &input.circuit, input.specs.as_deref())?;
    let p = ws.project().map_err(|e| e.to_string())?;
    let (netlist, std_libs, _) = ws
        .runner
        .netlist_for(&p, &input.circuit)
        .map_err(|e| e.to_string())?;
    // The parameter must be one the netlist sets: a typo would otherwise
    // give the same result for every code.
    let mut probe = netlist.clone();
    sweep::set_value(&mut probe, &input.param, input.first as f64).map_err(|e| e.to_string())?;
    let corners = match &input.corners {
        Some(set) => set.count().map_err(|e| format!("corners: {e}"))?,
        None => 1,
    };
    let per_corner = input.runs.max(1);
    let total = corners * per_corner * ncodes;
    if total > MAX_TRIM_RUNS {
        return Err(format!(
            "{corners} corner(s) x {per_corner} sample(s) x {ncodes} codes is {total} simulations, over the {MAX_TRIM_RUNS} limit"
        ));
    }
    let sources = analysis::derived_sources(&netlist, specs.iter().map(|s| &s.measure))?;
    let netlist =
        analysis::as_simulator_reads_it(ws, &p, netlist, input.simulator.as_deref()).await;
    let mut samples: Vec<(String, Netlist)> = Vec::new();
    {
        let files = analysis::library_files(ws, &p, &input.circuit)?;
        let lookup = |f: &str| files.sections(f);
        let bases = match &input.corners {
            Some(set) => {
                corner::variants(&netlist, set, &lookup).map_err(|e| format!("corners: {e}"))?
            }
            None => vec![(String::new(), netlist.clone())],
        };
        for (name, base) in bases {
            if input.runs > 0 && (input.mismatch.is_some() || !input.tolerances.is_empty()) {
                let (variants, _) = montecarlo::variants_with(
                    &base,
                    &input.tolerances,
                    input.mismatch,
                    input.runs,
                    input.seed.unwrap_or(1),
                    &lookup,
                )
                .map_err(|e| e.to_string())?;
                for v in variants {
                    let label = if name.is_empty() {
                        v.label
                    } else {
                        format!("{name} {}", v.label)
                    };
                    samples.push((label, v.netlist));
                }
            } else {
                samples.push((
                    if name.is_empty() {
                        "nominal".into()
                    } else {
                        name
                    },
                    base,
                ));
            }
        }
    }
    let mut labelled = Vec::with_capacity(samples.len() * ncodes);
    for (label, n) in &samples {
        for code in input.first..=input.last {
            let mut v = n.clone();
            sweep::set_value(&mut v, &input.param, code as f64).map_err(|e| e.to_string())?;
            labelled.push((format!("{label} code {code}"), v));
        }
    }
    let results = analysis::run_variants(
        ws,
        &input.circuit,
        labelled,
        &std_libs,
        input.simulator.as_deref(),
        &sources,
        ctx,
    )
    .await;
    let mut per_sample = Vec::with_capacity(samples.len());
    let mut first_failure = None;
    for (k, (label, _)) in samples.iter().enumerate() {
        let mut codes = Vec::with_capacity(ncodes);
        for (j, code) in (input.first..=input.last).enumerate() {
            match results.get(k * ncodes + j) {
                Some((_, Ok(run))) => codes.push(CodeResult::from_report(
                    code,
                    &evaluate(&specs, &run.datasets),
                )),
                Some((l, Err(e))) => {
                    first_failure.get_or_insert_with(|| format!("{l}: {e}"));
                    codes.push(CodeResult::failed(code));
                }
                None => codes.push(CodeResult::failed(code)),
            }
        }
        per_sample.push(SampleResult {
            label: label.clone(),
            codes,
        });
    }
    let nominal = input
        .nominal
        .unwrap_or(input.first + (input.last - input.first) / 2);
    let r = trim::summarize(&per_sample, nominal);
    let mut t = format!("{}\n", r.summary());
    if let Some(f) = &first_failure {
        t.push_str(&format!("  some runs failed to simulate; the first, {f}\n"));
    }
    for n in r.uncovered.iter().take(10) {
        t.push_str(&format!(
            "  cannot trim {}: closest at code {}, margin {}\n",
            n.label,
            n.code.map_or("none".into(), |c| c.to_string()),
            n.margin
                .map_or("n/a".into(), |m| format!("{:+.1}%", m * 100.0))
        ));
    }
    let hist: Vec<String> = r
        .histogram
        .iter()
        .map(|(c, n)| format!("{c}: {n}"))
        .collect();
    if !hist.is_empty() {
        t.push_str(&format!(
            "  samples needing each code: {}\n",
            hist.join(", ")
        ));
    }
    for n in r.needs.iter().take(20) {
        t.push_str(&format!(
            "  {}: code {} ({})\n",
            n.label,
            n.code.map_or("none".into(), |c| c.to_string()),
            n.margin
                .map_or("n/a".into(), |m| format!("margin {:+.1}%", m * 100.0))
        ));
    }
    if r.needs.len() > 20 {
        t.push_str(&format!("  ... {} more samples\n", r.needs.len() - 20));
    }
    Ok(ToolOutput::text(t).with_data(json!({"kind": "generic", "value": r})))
}
