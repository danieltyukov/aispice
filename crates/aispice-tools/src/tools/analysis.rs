//! What the analysis tools share: running netlist variants a few at a time,
//! the derived AC runs PSRR and CMRR need, reading corner libraries'
//! sections, and the corner report as text and data.

use crate::project::Project;
use crate::runner::{Runner, StoredRun};
use crate::workspace::Workspace;
use aispice_agent::ToolContext;
use aispice_core::netlist::Netlist;
use aispice_sim::Dataset;
use aispice_sim::corner::{self, CornerReport};
use aispice_sim::measure::{Measure, spaced};
use aispice_sim::stimulus;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Simulations run at once.
const BATCH: usize = 6;

/// Library files larger than this are not read for their section names.
const MAX_LIBRARY_BYTES: u64 = 16 * 1024 * 1024;

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
    let mut jobs: Vec<(usize, Option<String>, Result<Netlist, String>)> = Vec::new();
    for (i, (_, n)) in variants.iter().enumerate() {
        jobs.push((i, None, Ok(n.clone())));
        for s in sources {
            jobs.push((
                i,
                Some(s.clone()),
                stimulus::ac_on(n, s).map_err(|e| e.to_string()),
            ));
        }
    }
    let total = jobs.len();
    let mut done: Vec<Result<Arc<StoredRun>, String>> = Vec::with_capacity(total);
    for chunk in jobs.chunks(BATCH) {
        if ctx.cancel.is_cancelled() {
            break;
        }
        let futs = chunk.iter().map(|(_, _, n)| {
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

/// Lists the sections of a library a circuit's `.lib` line names, reading
/// only files inside the folders the netlist policy lets a simulation read.
/// Answers are kept for the call.
pub(crate) fn section_lookup(
    ws: &Workspace,
    p: &Project,
    circuit: &str,
) -> impl Fn(&str) -> Option<Vec<String>> + use<> {
    let policy = Runner::base_dir(p, circuit)
        .ok()
        .map(|base| ws.runner.policy_for(p, &base));
    let cache: RefCell<HashMap<String, Option<Vec<String>>>> = RefCell::new(HashMap::new());
    move |file: &str| {
        if let Some(hit) = cache.borrow().get(file) {
            return hit.clone();
        }
        let found = policy.as_ref().and_then(|policy| {
            let path = Path::new(file);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                policy.base_dir.join(path)
            };
            let real = std::fs::canonicalize(&path).ok()?;
            let inside = std::iter::once(&policy.base_dir)
                .chain(&policy.allowed_dirs)
                .any(|d| real.starts_with(std::fs::canonicalize(d).unwrap_or_else(|_| d.clone())));
            if !inside || std::fs::metadata(&real).ok()?.len() > MAX_LIBRARY_BYTES {
                return None;
            }
            let bytes = std::fs::read(&real).ok()?;
            let (text, _) = aispice_core::encoding::decode(&bytes);
            Some(corner::lib_sections(&text))
        });
        cache.borrow_mut().insert(file.to_string(), found.clone());
        found
    }
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
