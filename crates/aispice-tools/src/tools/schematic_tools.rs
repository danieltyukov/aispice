//! Tools that read and change schematics.

use super::blocking;
use crate::workspace::Workspace;
use aispice_agent::tool::parse_input;
use aispice_agent::{Tool, ToolContext, ToolOutput, ToolSpec, schema_for};
use aispice_core::diff::diff;
use aispice_core::edit::{EditOp, apply, parse_edits};
use aispice_core::lint::{Severity, lint};
use aispice_core::schematic::Schematic;
use aispice_core::summary::summarize;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) fn spec<T: JsonSchema>(name: &str, description: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.trim().into(),
        input_schema: schema_for::<T>(),
    }
}

/// The edits field as an array of the flat edit schema (see
/// `aispice_core::edit::flat_edit_schema`).
fn edit_list_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let v = serde_json::json!({"type": "array", "items": aispice_core::edit::flat_edit_schema()});
    schemars::Schema::try_from(v).expect("valid schema")
}

/// The edit list as sent: a list of edit objects, or a JSON string holding
/// one (some models send it that way). Each edit is read on its own so a
/// mistake names the edit that has it.
fn read_edits(v: &Value) -> Result<(Vec<EditOp>, Vec<String>), String> {
    let not_a_list = || "`edits` must be a list of edit objects, each with an `op`".to_string();
    let list = match v {
        Value::Array(a) => a.clone(),
        Value::Null => Vec::new(),
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Array(a)) => a,
            _ => return Err(not_a_list()),
        },
        _ => return Err(not_a_list()),
    };
    parse_edits(&list).map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoInput {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CircuitInput {
    /// Path of the circuit relative to the project folder, e.g. `rc.asc`.
    pub circuit: String,
}

pub struct ListCircuits {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for ListCircuits {
    fn spec(&self) -> ToolSpec {
        spec::<NoInput>(
            "list_circuits",
            "List the circuit files in the open project (.asc schematics and SPICE netlists), newest first. Only needed when you do not know the file name: when the user names a file, read it directly.",
        )
    }

    async fn call(&self, _ctx: &ToolContext, _input: Value) -> ToolOutput {
        let ws = self.ws.clone();
        blocking(move || {
            let p = ws.project().map_err(|e| e.to_string())?;
            let circuits = p.circuits();
            let mut text = format!("{} circuit(s) in {}:\n", circuits.len(), p.name());
            for c in &circuits {
                text.push_str(&format!("  {}\n", c.path));
            }
            if circuits.is_empty() {
                text.push_str("  (none yet; create one with create_schematic)\n");
            }
            Ok(ToolOutput::text(text).with_data(json!({"kind": "generic", "value": circuits})))
        })
        .await
    }
}

pub struct ReadSchematic {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for ReadSchematic {
    fn spec(&self) -> ToolSpec {
        spec::<CircuitInput>(
            "read_schematic",
            "Describe a schematic in circuit terms: every component with its value, its attributes in brackets and the net on each pin, every net with its members, the directives, and electrical rule check results. Read a circuit before editing it. The value column is what set_value changes; for an op-amp or other subcircuit it is the subcircuit's name (opamp), and parameters such as [SpiceLine2: GBW=10Meg] are changed with set_attr on that attribute. The pin names listed for each part are the ones edit_schematic expects.",
        )
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: CircuitInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let ws = self.ws.clone();
        blocking(move || {
            let p = ws.project().map_err(|e| e.to_string())?;
            let (sch, warnings) = p.load(&input.circuit).map_err(|e| e.to_string())?;
            let summary = summarize(&sch, p.library());
            let mut text = format!("{}\n", input.circuit);
            text.push_str(&summary.to_text());
            for w in warnings {
                text.push_str(&format!("Parse note (line {}): {}\n", w.line, w.message));
            }
            Ok(ToolOutput::text(text).with_data(
                json!({"kind": "schematic", "circuit": input.circuit, "summary": summary}),
            ))
        })
        .await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditInput {
    /// Path of the schematic relative to the project folder.
    pub circuit: String,
    /// Edits, applied in order and atomically: if one fails, none are kept.
    /// Errors name the edit by its position in this list, counting from 0.
    #[schemars(schema_with = "edit_list_schema")]
    pub edits: Value,
    /// One short sentence saying why, shown in the history.
    #[serde(default)]
    pub reason: Option<String>,
}

pub struct EditSchematic {
    pub ws: Arc<Workspace>,
}

const EDIT_DESCRIPTION: &str = r#"
Change a schematic with circuit-level edits. Refer to parts by instance name (R1) and to pins as PART.PIN with the pin names read_schematic lists (R1.A, R1.2, Q1.B, V1.+, and U1.invin, U1.noninvin, U1.out for the built-in op-amp). Never compute coordinates: `connect` routes wires itself and only ever joins the two nets you name (it falls back to net labels when no clean route exists), `connect_to_net` attaches a pin to a named net or to ground (`0`), and `add_component` without `at` finds free space near `near`. Typical sequence for a new part: add_component, then connect or connect_to_net for each pin. Two-terminal parts are vertical by default; orient R90 makes them horizontal. Directives (.tran, .ac, .op, .param, .meas, .step) go in with add_directive.
The edit is saved immediately (the user can undo it) and LTspice reloads if it is open. The result lists what changed, a semantic diff, and any electrical rule problems the edit introduced; fix those before simulating.
"#;

#[async_trait]
impl Tool for EditSchematic {
    fn spec(&self) -> ToolSpec {
        spec::<EditInput>("edit_schematic", EDIT_DESCRIPTION)
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: EditInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let (edits, notes) = match read_edits(&input.edits) {
            Ok(v) => v,
            Err(e) => return ToolOutput::error(format!("No changes made. {e}")),
        };
        let ws = self.ws.clone();
        // Compute the edit off the executor, ask for approval if configured,
        // then save off the executor again.
        let planned = tokio::task::spawn_blocking({
            let ws = ws.clone();
            let circuit = input.circuit.clone();
            move || -> Result<(Schematic, Schematic, aispice_core::edit::EditReport), String> {
                let p = ws.project().map_err(|e| e.to_string())?;
                let (before, _) = p.load(&circuit).map_err(|e| e.to_string())?;
                let mut after = before.clone();
                let mut report =
                    apply(&mut after, p.library(), &edits).map_err(|e| e.to_string())?;
                report.warnings.extend(notes);
                Ok((before, after, report))
            }
        })
        .await;
        let (before, after, report) = match planned {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return ToolOutput::error(format!("No changes made. {e}")),
            Err(e) => return ToolOutput::error(format!("internal error: {e}")),
        };
        let p = match ws.project() {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let d = diff(&before, &after, p.library());
        if d.is_empty() {
            return ToolOutput::text(format!("No changes: {}", report.applied.join("; ")));
        }
        let summary = input.reason.clone().unwrap_or_else(|| {
            report
                .applied
                .first()
                .cloned()
                .unwrap_or_else(|| "Edit".into())
        });
        if let Some(approver) = ws.approver()
            && !approver.approve(&input.circuit, &summary, &d.text).await
        {
            return ToolOutput::text(
                "The user declined this edit; nothing was changed. Ask what they would prefer.",
            );
        }
        blocking(move || {
            // The card's undo restores the version before this edit.
            let (before_snapshot, after_snapshot) = p
                .save_tracked(&input.circuit, &after, &summary)
                .map_err(|e| e.to_string())?;
            let snapshot = before_snapshot.unwrap_or(after_snapshot);
            if let Ok(path) = p.resolve(&input.circuit) {
                ws.after_save(&path);
            }
            let lint_before = lint(&before, p.library()).findings;
            let lint_after = lint(&after, p.library()).findings;
            let new_problems: Vec<_> = lint_after
                .iter()
                .filter(|f| f.severity != Severity::Info && !lint_before.contains(f))
                .cloned()
                .collect();
            let mut text = format!("Saved {} (undo is available).\n", input.circuit);
            for a in &report.applied {
                text.push_str(&format!("  {a}\n"));
            }
            for w in &report.warnings {
                text.push_str(&format!("  note: {w}\n"));
            }
            let changes = d.to_text();
            if !changes.is_empty() {
                text.push_str("Changes:\n");
                for line in changes.lines() {
                    text.push_str(&format!("  {line}\n"));
                }
            }
            if new_problems.is_empty() {
                text.push_str("Checks: no new problems.\n");
            } else {
                text.push_str("New problems to fix:\n");
                for f in &new_problems {
                    text.push_str(&format!("  {:?} [{}] {}\n", f.severity, f.rule, f.message));
                }
            }
            let highlights: Vec<Value> = d
                .highlights()
                .into_iter()
                .map(|(inst, kind)| json!({"inst": inst, "kind": kind}))
                .collect();
            Ok(ToolOutput::text(text).with_data(json!({
                "kind": "edit",
                "circuit": input.circuit,
                "summary": summary,
                "diff": d.text,
                "applied": report.applied,
                "warnings": report.warnings,
                "highlights": highlights,
                "snapshot": snapshot,
            })))
        })
        .await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateInput {
    /// New file path relative to the project folder, ending in `.asc`.
    pub circuit: String,
    /// A SPICE netlist to draw as a readable schematic: elements, `.model`
    /// and `.subckt` definitions and analysis directives, one per line. Use
    /// it to turn a netlist, a textbook circuit or a circuit read from an
    /// image into a schematic. aispice places and wires every part and
    /// checks that the drawing netlists back to exactly this circuit.
    #[serde(default)]
    pub netlist: Option<String>,
    /// Edits to apply after the netlist is drawn, or to build the circuit
    /// from an empty sheet (same operations as edit_schematic).
    #[serde(default)]
    #[schemars(schema_with = "edit_list_schema")]
    pub edits: Value,
}

/// Netlists larger than this are refused rather than laid out: layout cost
/// grows quickly with the number of parts.
pub const MAX_NETLIST_BYTES: usize = 64 * 1024;
const MAX_LAYOUT_ELEMENTS: usize = 150;

/// SPICE treats the first line as a title. Models often leave it out, so a
/// first line that already reads as an element or a directive gets a title
/// in front of it instead of being swallowed.
fn with_title(netlist: &str, circuit: &str) -> String {
    let first = netlist.lines().map(str::trim).find(|l| !l.is_empty());
    let looks_like_content = first.is_some_and(|l| {
        l.starts_with('.')
            || (l.split_whitespace().count() >= 3 && l.as_bytes()[0].is_ascii_alphabetic())
    });
    if looks_like_content {
        format!("* {circuit}\n{netlist}")
    } else {
        netlist.to_string()
    }
}

/// Draw a netlist as a schematic, after checking it is safe and small enough.
/// Returns the drawing, notes about lines kept as SPICE text, and the layout
/// quality score. `circuit` names the result and titles an untitled netlist.
pub fn layout_netlist(
    text: &str,
    circuit: &str,
    lib: &aispice_core::symbol::SymbolLibrary,
) -> Result<(Schematic, Vec<String>, f64), String> {
    if text.len() > MAX_NETLIST_BYTES {
        return Err(format!(
            "the netlist is {} bytes; the limit is {MAX_NETLIST_BYTES}. Split it into subcircuits.",
            text.len()
        ));
    }
    let violations = aispice_core::netlist::policy::check_lexical(text);
    if !violations.is_empty() {
        let list: Vec<String> = violations
            .iter()
            .take(5)
            .map(|v| format!("`{}`: {}", v.line, v.reason))
            .collect();
        return Err(format!("the netlist is not allowed: {}", list.join("; ")));
    }
    let netlist = aispice_core::netlist::parse(&with_title(text, circuit));
    let elements = netlist
        .items
        .iter()
        .filter(|l| matches!(l, aispice_core::netlist::Line::Element(_)))
        .count();
    if elements > MAX_LAYOUT_ELEMENTS {
        return Err(format!(
            "the netlist has {elements} elements; layout handles up to {MAX_LAYOUT_ELEMENTS}. Put repeated blocks in subcircuits."
        ));
    }
    let result = aispice_core::layout::from_netlist(&netlist, lib, &Default::default())
        .map_err(|e| e.to_string())?;
    Ok((result.schematic, result.warnings, result.quality.score))
}

pub struct CreateSchematic {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for CreateSchematic {
    fn spec(&self) -> ToolSpec {
        spec::<CreateInput>(
            "create_schematic",
            "Create a new LTspice schematic in the project. Give a SPICE netlist to have it drawn as a readable schematic (parts placed, wires routed, checked to netlist back to the same circuit), or build it from an empty sheet with the edits edit_schematic accepts, or both (edits apply after the drawing). Refuses to overwrite an existing file.",
        )
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: CreateInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let edits = match read_edits(&input.edits) {
            Ok((edits, _)) => edits,
            Err(e) => return ToolOutput::error(format!("No file created. {e}")),
        };
        let ws = self.ws.clone();
        let planned = tokio::task::spawn_blocking({
            let ws = ws.clone();
            let circuit = input.circuit.clone();
            let netlist = input.netlist.clone();
            move || -> Result<(Schematic, Vec<String>, Option<f64>, Vec<String>), String> {
                let p = ws.project().map_err(|e| e.to_string())?;
                if !circuit.to_ascii_lowercase().ends_with(".asc") {
                    return Err(format!("{circuit} must end in .asc"));
                }
                if p.resolve(&circuit).map_err(|e| e.to_string())?.exists() {
                    return Err(format!("{circuit} already exists; edit it instead"));
                }
                let (mut sch, notes, score) = match netlist.as_deref().map(str::trim) {
                    Some(text) if !text.is_empty() => {
                        let (sch, notes, score) = layout_netlist(text, &circuit, p.library())?;
                        (sch, notes, Some(score))
                    }
                    _ => (Schematic::new(), Vec::new(), None),
                };
                let report = apply(&mut sch, p.library(), &edits).map_err(|e| e.to_string())?;
                Ok((sch, notes, score, report.applied))
            }
        })
        .await;
        let (sch, notes, score, applied) = match planned {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return ToolOutput::error(format!("No file created. {e}")),
            Err(e) => return ToolOutput::error(format!("internal error: {e}")),
        };
        let p = match ws.project() {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let d = diff(&Schematic::new(), &sch, p.library());
        if !ws
            .approve(
                &input.circuit,
                &format!("Create {}", input.circuit),
                &d.text,
            )
            .await
        {
            return ToolOutput::text(
                "The user declined creating this file; nothing was written. Ask what they would prefer.",
            );
        }
        blocking(move || {
            p.create(&input.circuit, &sch).map_err(|e| e.to_string())?;
            if let Ok(path) = p.resolve(&input.circuit) {
                ws.after_save(&path);
            }
            let summary = summarize(&sch, p.library());
            let mut text = format!(
                "Created {} with {} component(s).\n",
                input.circuit,
                summary.components.len()
            );
            if let Some(score) = score {
                text.push_str(&format!(
                    "Drawn from the netlist (layout quality {score:.0}/100) and checked to netlist back to it.\n"
                ));
            }
            for n in &notes {
                text.push_str(&format!("  note: {n}\n"));
            }
            for a in &applied {
                text.push_str(&format!("  {a}\n"));
            }
            text.push_str(&summary.to_text());
            Ok(ToolOutput::text(text).with_data(
                json!({"kind": "schematic", "circuit": input.circuit, "summary": summary}),
            ))
        })
        .await
    }
}

pub struct Lint {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for Lint {
    fn spec(&self) -> ToolSpec {
        spec::<CircuitInput>(
            "lint",
            "Run electrical rule checks on a schematic: missing ground, floating pins, dangling wires, duplicate names, missing values, shorted parts, parallel voltage sources, nets with no DC path to ground, missing analysis directive. Run it after edits and before simulating.",
        )
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: CircuitInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let ws = self.ws.clone();
        blocking(move || {
            let p = ws.project().map_err(|e| e.to_string())?;
            let (sch, _) = p.load(&input.circuit).map_err(|e| e.to_string())?;
            let report = lint(&sch, p.library());
            let mut text = if report.findings.is_empty() {
                format!("{}: no problems found.\n", input.circuit)
            } else {
                format!(
                    "{}: {} error(s), {} warning(s).\n",
                    input.circuit,
                    report.errors(),
                    report.warnings()
                )
            };
            for f in &report.findings {
                text.push_str(&format!("  {:?} [{}] {}\n", f.severity, f.rule, f.message));
            }
            Ok(ToolOutput::text(text)
                .with_data(json!({"kind": "lint", "findings": report.findings})))
        })
        .await
    }
}

pub struct NetlistTool {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for NetlistTool {
    fn spec(&self) -> ToolSpec {
        spec::<CircuitInput>(
            "netlist",
            "Show the SPICE netlist of a schematic, derived the way LTspice does it (instance names, node order, default models), with the PDK's model header in a PDK project. Useful to check exactly what will be simulated.",
        )
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: CircuitInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let ws = self.ws.clone();
        blocking(move || {
            let p = ws.project().map_err(|e| e.to_string())?;
            let (sch, _) = p.load(&input.circuit).map_err(|e| e.to_string())?;
            // The runner's netlist, which in a PDK project ends with the
            // PDK's model header; the bare schematic netlist when the PDK
            // the project names cannot be opened, with the reason.
            let (netlist, mut warnings) = match ws.runner.netlist_for(&p, &input.circuit) {
                Ok((n, _, w)) => (n, w),
                Err(e) => {
                    let (built, _) = aispice_core::netlist::build(
                        &sch,
                        p.library(),
                        &format!("* {}", input.circuit),
                    );
                    let mut w = built.warnings;
                    w.push(e.to_string());
                    (built.netlist, w)
                }
            };
            warnings.dedup();
            let mut text = aispice_core::netlist::write(&netlist);
            for w in &warnings {
                text.push_str(&format!("* warning: {w}\n"));
            }
            Ok(ToolOutput::text(text))
        })
        .await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HistoryAction {
    List,
    Undo,
    Redo,
    Restore,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HistoryInput {
    pub circuit: String,
    pub action: HistoryAction,
    /// Snapshot id for `restore`, from `list`.
    #[serde(default)]
    pub snapshot: Option<String>,
}

pub struct History {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for History {
    fn spec(&self) -> ToolSpec {
        spec::<HistoryInput>(
            "history",
            "List a schematic's saved versions, or undo, redo or restore one. Every edit made by aispice and every change made in LTspice in between is a version.",
        )
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: HistoryInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let ws = self.ws.clone();
        let action = match input.action {
            HistoryAction::List => None,
            HistoryAction::Undo => Some("Undo the last change"),
            HistoryAction::Redo => Some("Redo"),
            HistoryAction::Restore => Some("Restore an earlier version"),
        };
        if let Some(what) = action
            && !ws.approve(&input.circuit, what, "").await
        {
            return ToolOutput::text("The user declined; nothing was changed.");
        }
        blocking(move || {
            let p = ws.project().map_err(|e| e.to_string())?;
            let moved = match input.action {
                HistoryAction::List => None,
                HistoryAction::Undo => Some(p.undo(&input.circuit)),
                HistoryAction::Redo => Some(p.redo(&input.circuit)),
                HistoryAction::Restore => {
                    let id = input
                        .snapshot
                        .clone()
                        .ok_or("restore needs a snapshot id from the list action")?;
                    Some(p.restore(&input.circuit, &id))
                }
            };
            if let Some(m) = moved {
                let snap = m.map_err(|e| e.to_string())?;
                if let Ok(path) = p.resolve(&input.circuit) {
                    ws.after_save(&path);
                }
                return Ok(ToolOutput::text(format!(
                    "{} is now at version {} ({}).",
                    input.circuit, snap.id, snap.summary
                )));
            }
            let (snaps, current) = p.history(&input.circuit).map_err(|e| e.to_string())?;
            let mut text = format!("{} version(s) of {}:\n", snaps.len(), input.circuit);
            for (i, s) in snaps.iter().enumerate() {
                text.push_str(&format!(
                    "  {}{}  {}\n",
                    if i == current { "* " } else { "  " },
                    s.id,
                    s.summary
                ));
            }
            Ok(ToolOutput::text(text).with_data(
                json!({"kind": "generic", "value": {"snapshots": snaps, "current": current}}),
            ))
        })
        .await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SymbolsInput {
    /// Words to match in symbol names and descriptions, e.g. `opamp`,
    /// `schottky`, `LT1001`. Empty lists the common ones.
    #[serde(default)]
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    25
}

pub struct Symbols {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for Symbols {
    fn spec(&self) -> ToolSpec {
        spec::<SymbolsInput>(
            "symbols",
            "Search the symbols available for add_component: aispice's built-in set and, when installed, LTspice's library (thousands of vendor parts such as LT1001 or ADA4898). Shows each symbol's name, prefix and pin names in SPICE order.",
        )
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: SymbolsInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let ws = self.ws.clone();
        blocking(move || {
            let p = ws.project().map_err(|e| e.to_string())?;
            let hits = p.library().search(&input.query, input.limit.clamp(1, 200));
            let mut text = format!("{} symbol(s):\n", hits.len());
            for h in &hits {
                text.push_str(&format!(
                    "  {}  [{}] pins: {}  {}\n",
                    h.name,
                    h.prefix,
                    h.pins.join(", "),
                    h.description.as_deref().unwrap_or("")
                ));
            }
            Ok(ToolOutput::text(text).with_data(json!({"kind": "generic", "value": hits})))
        })
        .await
    }
}
