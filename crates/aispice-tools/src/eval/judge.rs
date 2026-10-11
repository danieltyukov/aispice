//! Judging a finished run: simulate the judged circuit, measure the specs,
//! run the rule checks and check the constraints. The model's own claims
//! count for nothing except where a constraint asks about its answer.

use super::task::{Constraint, Task, collect_files};
use crate::project::{Project, ProjectOptions, STATE_DIR};
use crate::runner::{RunMods, RunnerConfig};
use crate::tools::analysis;
use crate::workspace::Workspace;
use aispice_agent::ToolContext;
use aispice_core::lint::{LintReport, Severity, lint};
use aispice_core::schematic::{Schematic, normalize_symbol_name};
use aispice_core::symbol::SymbolLibrary;
use aispice_core::units;
use aispice_sim::corner;
use aispice_sim::optimize::snap;
use aispice_sim::spec::{SpecReport, evaluate};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hasher};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// One tool call the model made.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRecord {
    pub name: String,
    #[serde(default)]
    pub input: Value,
    pub is_error: bool,
    /// The start of the tool's text output, enough to see in a report why a
    /// call failed or what it told the model.
    #[serde(default)]
    pub output: String,
    /// The tool's structured output, for constraints that compare the answer
    /// with what a tool reported. Not kept in reports (plots are large).
    #[serde(skip)]
    pub data: Option<Value>,
}

/// How much of each tool's text output a report keeps.
pub(crate) const OUTPUT_KEPT: usize = 1500;

/// What the model did: everything it wrote, and its tool calls in order.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Transcript {
    pub text: String,
    pub tools: Vec<ToolRecord>,
}

/// The files of a project (outside aispice's own state folder), by content
/// hash, to tell what a run changed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileState(BTreeMap<String, u64>);

impl FileState {
    pub fn capture(root: &Path) -> FileState {
        let mut files = Vec::new();
        collect_files(root, root, &mut files);
        FileState(
            files
                .into_iter()
                .filter(|f| f != STATE_DIR && !f.starts_with(&format!("{STATE_DIR}/")))
                .filter_map(|f| {
                    let bytes = std::fs::read(root.join(&f)).ok()?;
                    let mut h = DefaultHasher::new();
                    h.write(&bytes);
                    Some((f, h.finish()))
                })
                .collect(),
        )
    }

    /// Starting files that changed or disappeared, and new circuit files.
    /// New `.specs` files and other non-circuit files are not counted.
    pub fn changes_since(&self, before: &FileState) -> Vec<String> {
        let mut out = Vec::new();
        for (f, h) in &before.0 {
            match self.0.get(f) {
                None => out.push(format!("{f} was deleted")),
                Some(now) if now != h => out.push(format!("{f} was changed")),
                _ => {}
            }
        }
        for f in self.0.keys().filter(|f| !before.0.contains_key(*f)) {
            if is_circuit_file(f) {
                out.push(format!("{f} was created"));
            }
        }
        out
    }
}

fn is_circuit_file(path: &str) -> bool {
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "asc" | "cir" | "net" | "sp" | "spi" | "spice" | "ckt"
    )
}

/// The judge's findings for one run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Verdict {
    pub pass: bool,
    /// Why the run fails, most important first; empty when it passes.
    pub failures: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub specs: Option<SpecReport>,
    pub lint_errors: usize,
    pub lint_warnings: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lint_findings: Vec<String>,
}

/// Measure the task's specs on the judged circuit with the task's analysis:
/// one run plus the derived AC runs its PSRR and CMRR specs need, or, when
/// the task names corners, every corner and the worst case of each spec.
async fn judge_specs(
    ws: &Workspace,
    p: &Project,
    task: &Task,
    simulator: &str,
) -> Result<SpecReport, String> {
    let mods = RunMods {
        analysis: task.analysis.clone(),
        extra: Vec::new(),
    };
    let (mut netlist, std_libs, _) = ws
        .runner
        .netlist_for(p, &task.circuit)
        .map_err(|e| format!("simulation failed: {e}"))?;
    crate::runner::apply_mods(&mut netlist, &mods);
    let sources = analysis::derived_sources(&netlist, task.specs.iter().map(|s| &s.measure))?;
    let ctx = ToolContext::default();
    if let Some(set) = &task.corners {
        let variants = {
            let files = analysis::library_files(ws, p, &task.circuit)?;
            let lookup = |f: &str| files.sections(f);
            corner::variants(&netlist, set, &lookup).map_err(|e| format!("corners: {e}"))?
        };
        let names: Vec<String> = variants.iter().map(|(n, _)| n.clone()).collect();
        let results = analysis::run_variants(
            ws,
            &task.circuit,
            variants,
            &std_libs,
            Some(simulator),
            &sources,
            &ctx,
        )
        .await;
        let reports: Vec<Result<SpecReport, String>> = results
            .into_iter()
            .map(|(_, r)| r.map(|v| evaluate(&task.specs, &v.datasets)))
            .collect();
        let report = corner::summarize(&task.specs, &names[..reports.len()], &reports);
        if let Some((c, why)) = report.failed.first() {
            return Err(format!("simulation failed at corner {c}: {why}"));
        }
        return Ok(report.worst);
    }
    let run = ws
        .runner
        .run_circuit(
            p,
            &task.circuit,
            Some(simulator),
            &mods,
            &CancellationToken::new(),
        )
        .await
        .map_err(|e| format!("simulation failed: {e}"))?;
    if run.output.datasets.is_empty() {
        let why = run
            .output
            .errors
            .first()
            .cloned()
            .unwrap_or_else(|| "no results".into());
        return Err(format!("simulation gave no results: {why}"));
    }
    let mut datasets = run.output.datasets.clone();
    datasets.extend(
        analysis::derived_datasets(
            ws,
            p,
            &task.circuit,
            &netlist,
            &std_libs,
            Some(simulator),
            &sources,
            &ctx,
        )
        .await?,
    );
    Ok(evaluate(&task.specs, &datasets))
}

/// Judge the project at `root` after a run. `before` is the state of the
/// files when the run started. Specs are measured on `simulator` (ngspice
/// for the suite), with the task's analysis when it names one.
pub async fn judge(
    task: &Task,
    root: &Path,
    before: &FileState,
    transcript: &Transcript,
    simulator: &str,
    symbols: &[PathBuf],
) -> Verdict {
    let mut v = Verdict::default();
    let options = ProjectOptions {
        extra_symbol_dirs: symbols.to_vec(),
        ltspice_lib: None,
    };
    let project = match Project::open(root, options) {
        Ok(p) => p,
        Err(e) => {
            v.failures.push(format!("could not open the project: {e}"));
            return v;
        }
    };
    let loaded = match project.load(&task.circuit) {
        Ok((sch, _)) => Some(sch),
        Err(e) => {
            v.failures.push(format!("{}: {e}", task.circuit));
            None
        }
    };
    let lib = project.library().clone();

    if let Some(sch) = &loaded {
        if !task.specs.is_empty() {
            let ws = Workspace::new();
            ws.runner.set_config(RunnerConfig {
                simulator: simulator.to_string(),
                // aispice's own models only, so results compare across
                // machines whatever LTspice library is installed.
                embedded_models_only: true,
                ..RunnerConfig::default()
            });
            ws.set_project(project);
            let p = ws.project().expect("project was just set");
            match judge_specs(&ws, &p, task, simulator).await {
                Ok(report) => {
                    for row in report.rows.iter().filter(|r| !r.pass) {
                        v.failures.push(format!("spec {}", row.display));
                    }
                    v.specs = Some(report);
                }
                Err(e) => v.failures.push(e),
            }
        }
        let report = lint(sch, &lib);
        v.lint_errors = report.errors();
        v.lint_warnings = report.warnings();
        v.lint_findings = report
            .findings
            .iter()
            .filter(|f| f.severity != Severity::Info)
            .map(|f| format!("{:?} [{}] {}", f.severity, f.rule, f.message))
            .collect();
        if task.lint && v.lint_errors > 0 {
            let first = report
                .findings
                .iter()
                .find(|f| f.severity == Severity::Error)
                .map(|f| f.message.clone())
                .unwrap_or_default();
            v.failures
                .push(format!("lint: {} error(s), first: {first}", v.lint_errors));
        }
        let cx = Context {
            sch: Some(sch),
            lib: &lib,
            lint: Some(&report),
            transcript,
            before,
            root,
        };
        for c in &task.constraints {
            if let Some(f) = check(c, &cx) {
                v.failures.push(f);
            }
        }
    } else {
        // Without the circuit only the checks on the answer and the files
        // mean anything; the missing circuit is already a failure.
        let cx = Context {
            sch: None,
            lib: &lib,
            lint: None,
            transcript,
            before,
            root,
        };
        for c in &task.constraints {
            if let Some(f) = check(c, &cx) {
                v.failures.push(f);
            }
        }
    }
    v.pass = v.failures.is_empty();
    v
}

struct Context<'a> {
    sch: Option<&'a Schematic>,
    lib: &'a SymbolLibrary,
    lint: Option<&'a LintReport>,
    transcript: &'a Transcript,
    before: &'a FileState,
    root: &'a Path,
}

/// A placed part: instance name, symbol, value and device letter.
struct Part {
    name: String,
    symbol: String,
    value: String,
    prefix: char,
}

fn parts(sch: &Schematic, lib: &SymbolLibrary) -> Vec<Part> {
    sch.symbols()
        .map(|s| {
            let def_prefix = lib
                .resolve(&s.name)
                .ok()
                .map(|(d, _)| d.prefix().to_string());
            let prefix = s
                .attr("Prefix")
                .map(str::to_string)
                .or(def_prefix)
                .unwrap_or_else(|| "X".into());
            Part {
                name: s.inst_name().unwrap_or("").to_string(),
                symbol: s.name.clone(),
                value: s.value().unwrap_or("").trim().to_string(),
                prefix: prefix.chars().next().unwrap_or('X').to_ascii_uppercase(),
            }
        })
        .collect()
}

fn letter(prefix: &str) -> char {
    prefix.chars().next().unwrap_or('X').to_ascii_uppercase()
}

fn num(text: &str) -> f64 {
    units::parse(text).unwrap_or(f64::NAN)
}

/// Check one constraint; `Some(reason)` when it fails.
fn check(c: &Constraint, cx: &Context) -> Option<String> {
    let structural = matches!(
        c,
        Constraint::PartExists { .. }
            | Constraint::MaxParts { .. }
            | Constraint::PartCount { .. }
            | Constraint::Value { .. }
            | Constraint::ValueRange { .. }
            | Constraint::ESeries { .. }
            | Constraint::LintClean
            | Constraint::SubcktParam { .. }
    );
    if structural && cx.sch.is_none() {
        return None;
    }
    let all = cx.sch.map(|s| parts(s, cx.lib)).unwrap_or_default();
    let find = |name: &str| all.iter().find(|p| p.name.eq_ignore_ascii_case(name));
    match c {
        Constraint::PartExists { name } => {
            find(name).is_none().then(|| format!("{name} is missing"))
        }
        Constraint::MaxParts { count } => (all.len() > *count)
            .then(|| format!("{} parts, more than the {count} allowed", all.len())),
        Constraint::PartCount {
            symbol,
            prefix,
            min,
            max,
        } => {
            let (n, what) = match (symbol, prefix) {
                (Some(symbol), _) => {
                    let key = normalize_symbol_name(symbol);
                    let n = all
                        .iter()
                        .filter(|p| normalize_symbol_name(&p.symbol) == key)
                        .count();
                    (n, symbol.clone())
                }
                (None, Some(prefix)) => {
                    let n = all.iter().filter(|p| p.prefix == letter(prefix)).count();
                    (n, format!("{}-prefix", letter(prefix)))
                }
                (None, None) => return None,
            };
            if min.is_some_and(|m| n < m) || max.is_some_and(|m| n > m) {
                let range = match (min, max) {
                    (Some(a), Some(b)) if a == b => format!("exactly {a}"),
                    (Some(a), Some(b)) => format!("{a} to {b}"),
                    (Some(a), None) => format!("at least {a}"),
                    (None, Some(b)) => format!("at most {b}"),
                    (None, None) => String::new(),
                };
                Some(format!("{n} {what} part(s), expected {range}"))
            } else {
                None
            }
        }
        Constraint::Value {
            name,
            equals,
            tol_pct,
        } => {
            let Some(p) = find(name) else {
                return Some(format!("{name} is missing"));
            };
            let (got, want) = (num(&p.value), num(equals));
            let close = (got - want).abs() <= want.abs() * tol_pct / 100.0;
            (!close).then(|| format!("{name} is {}, expected {equals}", p.value))
        }
        Constraint::ValueRange { prefix, min, max } => {
            let (lo, hi) = (min.as_deref().map(num), max.as_deref().map(num));
            all.iter()
                .filter(|p| p.prefix == letter(prefix))
                .find_map(|p| {
                    let v = num(&p.value);
                    let ok = v.is_finite()
                        && lo.is_none_or(|lo| v >= lo * (1.0 - 1e-9))
                        && hi.is_none_or(|hi| v <= hi * (1.0 + 1e-9));
                    (!ok).then(|| {
                        let range = match (min, max) {
                            (Some(a), Some(b)) => format!("{a}..{b}"),
                            (Some(a), None) => format!(">= {a}"),
                            (None, Some(b)) => format!("<= {b}"),
                            (None, None) => "any".into(),
                        };
                        format!("{} = {} is outside {range}", p.name, p.value)
                    })
                })
        }
        Constraint::ESeries { prefix, series } => all
            .iter()
            .filter(|p| p.prefix == letter(prefix))
            .find_map(|p| {
                let v = num(&p.value);
                let standard =
                    v.is_finite() && v > 0.0 && (snap(v, *series) / v - 1.0).abs() < 1e-6;
                (!standard).then(|| format!("{} = {} is not an {series:?} value", p.name, p.value))
            }),
        Constraint::LintClean => {
            let report = cx.lint?;
            report
                .findings
                .iter()
                .find(|f| f.severity == Severity::Warning)
                .map(|f| format!("lint warning [{}] {}", f.rule, f.message))
        }
        Constraint::FilesUnchanged => {
            let changes = FileState::capture(cx.root).changes_since(cx.before);
            (!changes.is_empty()).then(|| format!("files changed: {}", changes.join(", ")))
        }
        Constraint::ToolCalled { name } => {
            let ok = cx
                .transcript
                .tools
                .iter()
                .any(|t| t.name == *name && !t.is_error);
            (!ok).then(|| format!("{name} was never run successfully"))
        }
        Constraint::AnswerMentions { any } => {
            let text = cx.transcript.text.to_lowercase();
            let hit = any.iter().any(|w| text.contains(&w.to_lowercase()));
            (!hit).then(|| format!("the answer mentions none of: {}", any.join(", ")))
        }
        Constraint::AnswerNumber { value, tol_pct } => {
            let want = num(value);
            let found = numbers_in(&cx.transcript.text)
                .into_iter()
                .any(|n| (n - want).abs() <= want.abs() * tol_pct / 100.0);
            (!found).then(|| format!("the answer gives no number within {tol_pct}% of {value}"))
        }
        Constraint::AnswerReports { tool, pointer, tol } => {
            let reported: Vec<f64> = cx
                .transcript
                .tools
                .iter()
                .filter(|t| t.name == *tool && !t.is_error)
                .filter_map(|t| t.data.as_ref()?.pointer(pointer)?.as_f64())
                .collect();
            if reported.is_empty() {
                return Some(format!("{tool} reported no {pointer}"));
            }
            let numbers = numbers_in(&cx.transcript.text);
            let quoted = reported
                .iter()
                .any(|r| numbers.iter().any(|n| (n - r).abs() <= *tol));
            (!quoted).then(|| {
                format!(
                    "the answer does not report the {} {tool} found",
                    reported
                        .iter()
                        .map(|r| format!("{r}"))
                        .collect::<Vec<_>>()
                        .join(" or ")
                )
            })
        }
        Constraint::SubcktParam {
            subckt,
            param,
            min,
            max,
        } => {
            let sch = cx.sch?;
            let (built, _) = aispice_core::netlist::build(sch, cx.lib, "* judge");
            let (lo, hi) = (min.as_deref().map(num), max.as_deref().map(num));
            built
                .netlist
                .elements()
                .filter(|e| {
                    e.letter() == 'X'
                        && e.first_word()
                            .is_some_and(|w| w.eq_ignore_ascii_case(subckt))
                })
                .find_map(|e| {
                    let value = e.rest.split_whitespace().find_map(|t| {
                        let (k, v) = t.split_once('=')?;
                        k.eq_ignore_ascii_case(param).then(|| v.to_string())
                    });
                    let Some(value) = value else {
                        return Some(format!("{} does not set {param}", e.name));
                    };
                    let v = num(&value);
                    let ok = v.is_finite()
                        && lo.is_none_or(|lo| v >= lo * (1.0 - 1e-9))
                        && hi.is_none_or(|hi| v <= hi * (1.0 + 1e-9));
                    (!ok).then(|| format!("{} has {param}={value}, outside the limits", e.name))
                })
        }
    }
}

/// Every number written in `text`, scaled by an SI prefix when one is
/// attached (`1.59 kHz`, `1.6k`, `10 µF`, `1Meg`). Commas between groups of
/// three digits are thousands separators. A percentage is its number.
pub fn numbers_in(text: &str) -> Vec<f64> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let starts = chars[i].is_ascii_digit()
            && (i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '.'));
        if !starts {
            i += 1;
            continue;
        }
        let negative = i > 0 && chars[i - 1] == '-' && (i < 2 || !chars[i - 2].is_alphanumeric());
        let mut s = String::new();
        while i < chars.len() {
            let c = chars[i];
            let digit_next = chars.get(i + 1).is_some_and(|d| d.is_ascii_digit());
            if c.is_ascii_digit() || (c == '.' && digit_next) {
                s.push(c);
                i += 1;
            } else if c == ','
                && (1..=3).all(|k| chars.get(i + k).is_some_and(|d| d.is_ascii_digit()))
                && !chars.get(i + 4).is_some_and(|d| d.is_ascii_digit())
            {
                i += 1;
            } else {
                break;
            }
        }
        // An exponent: 1e-3, 2.5E6.
        if i < chars.len() && matches!(chars[i], 'e' | 'E') {
            let mut k = i + 1;
            let mut exp = String::from("e");
            if k < chars.len() && matches!(chars[k], '+' | '-') {
                exp.push(chars[k]);
                k += 1;
            }
            let digits = k;
            while k < chars.len() && chars[k].is_ascii_digit() {
                exp.push(chars[k]);
                k += 1;
            }
            if k > digits {
                s.push_str(&exp);
                i = k;
            }
        }
        let Ok(mut v) = s.parse::<f64>() else {
            continue;
        };
        // The word right after the number, past at most one space.
        let mut k = i;
        if chars.get(k) == Some(&' ') {
            k += 1;
        }
        let word: String = chars[k.min(chars.len())..]
            .iter()
            .take_while(|c| c.is_alphabetic())
            .collect();
        v *= scale_of(&word);
        out.push(if negative { -v } else { v });
    }
    out
}

const UNITS: &[&str] = &[
    "hz", "v", "a", "s", "f", "h", "w", "db", "ohm", "ohms", "\u{3a9}", "sec",
];

/// The multiplier an SI prefix in front of a unit gives (`k` in `kHz`), or 1.
fn scale_of(word: &str) -> f64 {
    let is_unit = |w: &str| w.is_empty() || UNITS.contains(&w.to_lowercase().as_str());
    if word.is_empty() || is_unit(word) {
        return 1.0;
    }
    for meg in ["Meg", "meg", "MEG"] {
        if let Some(rest) = word.strip_prefix(meg)
            && is_unit(rest)
        {
            return 1e6;
        }
    }
    let mut it = word.chars();
    let first = it.next().unwrap_or(' ');
    let rest: String = it.collect();
    if !is_unit(&rest) {
        return 1.0;
    }
    match first {
        'G' => 1e9,
        'M' => 1e6,
        'k' | 'K' => 1e3,
        'm' => 1e-3,
        'u' | '\u{b5}' | '\u{3bc}' => 1e-6,
        'n' => 1e-9,
        'p' => 1e-12,
        _ => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_numbers_like_people_write_them() {
        let n = numbers_in(
            "The corner is 1.59 kHz (1,592 Hz); R1 = 4.7k, C1 = 100nF, yield 56%, \
             gain -20 dB, 1Meg, 2.5e3 and 10 \u{b5}F. R2 stays. 3 more parts.",
        );
        let has = |x: f64| n.iter().any(|v| (v - x).abs() <= x.abs() * 1e-9 + 1e-15);
        for want in [
            1590.0, 1592.0, 4700.0, 100e-9, 56.0, -20.0, 1e6, 2500.0, 10e-6, 3.0,
        ] {
            assert!(has(want), "{want} not in {n:?}");
        }
        // Digits inside names are not numbers.
        assert!(!has(1.0) && !has(2.0), "{n:?}");
    }

    #[test]
    fn file_changes_ignore_specs_and_state() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.asc"), "x").unwrap();
        std::fs::write(root.join("notes.txt"), "n").unwrap();
        let before = FileState::capture(root);
        std::fs::write(root.join("a.specs"), "s").unwrap();
        std::fs::create_dir_all(root.join(".aispice/runs")).unwrap();
        std::fs::write(root.join(".aispice/runs/r.raw"), "r").unwrap();
        assert!(FileState::capture(root).changes_since(&before).is_empty());
        std::fs::write(root.join("a.asc"), "y").unwrap();
        std::fs::write(root.join("b.asc"), "z").unwrap();
        std::fs::remove_file(root.join("notes.txt")).unwrap();
        let changes = FileState::capture(root).changes_since(&before);
        assert_eq!(
            changes,
            vec![
                "a.asc was changed",
                "notes.txt was deleted",
                "b.asc was created"
            ]
        );
    }
}
