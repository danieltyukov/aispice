//! What the analysis tools share: running netlist variants a few at a time,
//! the derived AC runs PSRR and CMRR need, the corner libraries a circuit
//! names (read through the policy), and the corner report as text and data.

use crate::project::Project;
use crate::runner::{Runner, StoredRun};
use crate::workspace::Workspace;
use aispice_agent::ToolContext;
use aispice_core::netlist::Netlist;
use aispice_sim::Dataset;
use aispice_sim::corner::CornerReport;
use aispice_sim::library::LibraryFiles;
use aispice_sim::measure::{Measure, spaced};
use aispice_sim::stimulus;
use serde_json::{Value, json};
use std::sync::Arc;

/// Simulations run at once.
const BATCH: usize = 6;

/// A variant's run and the datasets its measurements read: the run's own,
/// plus those of a derived AC run for each source a PSRR or CMRR
/// measurement names.
pub(crate) struct VariantRun {
    pub run: Arc<StoredRun>,
    pub datasets: Vec<Dataset>,
}

/// The sources the measurements need derived AC runs for, checked against
/// the circuit so a wrong name fails before anything runs.
pub(crate) fn derived_sources<'a>(
    netlist: &Netlist,
    measures: impl IntoIterator<Item = &'a Measure>,
) -> Result<Vec<String>, String> {
    let sources = stimulus::needed_sources(measures);
    for s in &sources {
        stimulus::ac_on(netlist, s).map_err(|e| format!("{s}: {e}"))?;
    }
    Ok(sources)
}

/// Run netlist variants (and their derived AC runs) six at a time. Variants
/// whose run did not start, because the call was cancelled, are left out.
pub(crate) async fn run_variants(
    ws: &Workspace,
    circuit: &str,
    variants: Vec<(String, Netlist)>,
    std_libs: &[String],
    simulator: Option<&str>,
    sources: &[String],
    ctx: &ToolContext,
) -> Vec<(String, Result<VariantRun, String>)> {
    let p = match ws.project() {
        Ok(p) => p,
        Err(e) => {
            return variants
                .into_iter()
                .map(|(l, _)| (l, Err(e.to_string())))
                .collect();
        }
    };
    // One job per simulation: each variant, then its derived runs.
    let mut jobs: Vec<Result<Netlist, String>> = Vec::new();
    for (_, n) in &variants {
        jobs.push(Ok(n.clone()));
        for s in sources {
            jobs.push(stimulus::ac_on(n, s).map_err(|e| e.to_string()));
        }
    }
    let total = jobs.len();
    let mut done: Vec<Result<Arc<StoredRun>, String>> = Vec::with_capacity(total);
    for chunk in jobs.chunks(BATCH) {
        if ctx.cancel.is_cancelled() {
            break;
        }
        let futs = chunk.iter().map(|n| {
            let p = p.clone();
            async move {
                match n {
                    Ok(n) => ws
                        .runner
                        .run_netlist_with(&p, circuit, n, std_libs, simulator, &ctx.cancel, false)
                        .await
                        .map_err(|e| e.to_string()),
                    Err(e) => Err(e.clone()),
                }
            }
        });
        done.extend(futures::future::join_all(futs).await);
        ctx.progress(
            format!("{} of {total} runs", done.len()),
            Some(done.len() as f32 / total.max(1) as f32),
        );
    }
    // Variant i's jobs are i * per .. (i + 1) * per, in order.
    let per = 1 + sources.len();
    let mut out: Vec<(String, Result<VariantRun, String>)> = Vec::new();
    for (i, (label, _)) in variants.into_iter().enumerate() {
        let Some(main) = done.get(i * per) else {
            break;
        };
        let result = main.clone().and_then(|run| {
            let mut v = VariantRun {
                datasets: run.output.datasets.clone(),
                run,
            };
            for (j, s) in sources.iter().enumerate() {
                match done.get(i * per + 1 + j) {
                    Some(Ok(run)) => v.datasets.extend(marked(&run.output.datasets, s)),
                    Some(Err(e)) => {
                        return Err(format!("the AC run with AC 1 on {s} failed: {e}"));
                    }
                    None => return Err("cancelled".into()),
                }
            }
            Ok(v)
        });
        out.push((label, result));
    }
    out
}

fn marked(datasets: &[Dataset], source: &str) -> Vec<Dataset> {
    datasets
        .iter()
        .cloned()
        .map(|mut d| {
            stimulus::mark(&mut d, source);
            d
        })
        .collect()
}

/// The derived AC runs for one netlist, as datasets to add to its run's.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn derived_datasets(
    ws: &Workspace,
    p: &Project,
    circuit: &str,
    netlist: &Netlist,
    std_libs: &[String],
    simulator: Option<&str>,
    sources: &[String],
    ctx: &ToolContext,
) -> Result<Vec<Dataset>, String> {
    let mut out = Vec::new();
    for s in sources {
        let n = stimulus::ac_on(netlist, s).map_err(|e| format!("{s}: {e}"))?;
        let run = ws
            .runner
            .run_netlist_with(p, circuit, &n, std_libs, simulator, &ctx.cancel, false)
            .await
            .map_err(|e| format!("the AC run with AC 1 on {s} failed: {e}"))?;
        out.extend(marked(&run.output.datasets, s));
    }
    Ok(out)
}

/// The corner libraries a circuit's `.lib` lines name, read only through
/// the policy its simulations run under (see [`LibraryFiles`]), each at
/// most once in a call.
pub(crate) fn library_files(
    ws: &Workspace,
    p: &Project,
    circuit: &str,
) -> Result<LibraryFiles<'static>, String> {
    let base = Runner::base_dir(p, circuit).map_err(|e| e.to_string())?;
    Ok(LibraryFiles::new(ws.runner.policy_for(p, &base), p.root()))
}

/// The corner report for the model: the summary, each spec's worst case,
/// then every spec at every corner.
pub(crate) fn corner_text(r: &CornerReport) -> String {
    let mut t = format!("{}\n", r.summary);
    for row in &r.rows {
        t.push_str(&format!(
            "  {} {}\n",
            if row.pass { "PASS" } else { "FAIL" },
            row.display
        ));
    }
    for (c, why) in &r.failed {
        t.push_str(&format!("  {c}: the simulation failed: {why}\n"));
    }
    t.push_str("Every corner:\ncorner");
    for row in &r.rows {
        t.push_str(&format!("\t{}", row.name));
    }
    t.push('\n');
    for (i, c) in r.corners.iter().enumerate() {
        t.push_str(c);
        for row in &r.rows {
            let v = row.values.get(i).copied().flatten();
            t.push_str(&format!(
                "\t{}",
                v.map_or_else(|| "n/a".into(), |v| spaced(v, &row.unit))
            ));
        }
        t.push('\n');
    }
    t
}

/// The corner report as data: corner names, each spec's values in corner
/// order with its worst case, and the corners that failed.
pub(crate) fn corner_json(r: &CornerReport) -> Value {
    json!({
        "corners": r.corners,
        "worst_corner": r.worst_corner,
        "all_pass": r.all_pass,
        "summary": r.summary,
        "failed": r.failed.iter().map(|(c, why)| json!({"corner": c, "error": why})).collect::<Vec<_>>(),
        "rows": r.rows.iter().map(|row| json!({
            "name": row.name,
            "unit": row.unit,
            "min": row.min,
            "max": row.max,
            "values": row.values,
            "worst_value": row.worst_value,
            "worst_margin": row.worst_margin,
            "worst_corner": row.worst_corner,
            "pass": row.pass,
        })).collect::<Vec<_>>(),
    })
}
