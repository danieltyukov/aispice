//! The verified template library as a tool.

use super::blocking;
use super::schematic_tools::spec;
use super::sim_tools::specs_file;
use crate::templates::{self, CATEGORIES, Template};
use crate::workspace::Workspace;
use aispice_agent::tool::parse_input;
use aispice_agent::{Tool, ToolContext, ToolOutput, ToolSpec};
use aispice_core::summary::summarize;
use aispice_core::symbol::SymbolLibrary;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TemplatesAction {
    /// The library: id, title, category and a description of each.
    List,
    /// One template in full: design equations, the parts to change, the
    /// spec table and the schematic.
    Show,
    /// Copy a template into the project as a new circuit, with its specs.
    Use,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TemplatesInput {
    pub action: TemplatesAction,
    /// Template id for `show` and `use`, from `list`, e.g. `sallen_key_lowpass`.
    #[serde(default)]
    pub id: Option<String>,
    /// For `list`: only this category (filters, amplifiers, references,
    /// power, oscillators, digital, sensors, ic).
    #[serde(default)]
    pub category: Option<String>,
    /// For `list`: words that must all appear in the id, title or
    /// description, e.g. `low-pass` or `photodiode`.
    #[serde(default)]
    pub query: Option<String>,
    /// For `use`: path of the new schematic relative to the project folder,
    /// ending in `.asc`. The spec table is written beside it as
    /// `<name>.specs`, where check_specs, optimize and monte_carlo find it.
    #[serde(default)]
    pub circuit: Option<String>,
}

const DESCRIPTION: &str = r#"
Verified reference circuits to start a design from: passive and active filters, op-amp and transistor gain stages, voltage references, regulators and power switching, an oscillator, a logic level shifter and a photodiode front end, plus transistor-level IC blocks (category ic) on a PDK: current mirror bias, 5-transistor OTA, two-stage Miller op-amp, StrongARM comparator, bandgap reference and LDO on IHP SG13G2. Every template is simulated in aispice's test suite and meets its own spec table; an IC template meets it at every corner of the set it names, which you pass to check_specs as `corners`. `list` shows the library (filter by category or by words), `show` gives a template's design equations, the parts usually changed, its spec table, corners and layout generator, and the parts and nets of the schematic, and `use` copies it into the project as a new circuit with its specs saved beside it, ready for check_specs and then optimize. An IC template needs a project that uses its PDK (pdk tool, action use). `use` refuses to overwrite existing files.
"#;

pub struct Templates {
    pub ws: Arc<Workspace>,
}

/// The metadata the desktop app shows, without the schematic bytes.
fn template_json(t: &Template) -> Value {
    json!({
        "id": t.id,
        "title": t.title,
        "category": t.category,
        "description": t.description,
        "analysis": t.analysis,
        "specs": t.specs.trim(),
        "parameters": t.parameters,
        "pdk": t.pdk,
        "corners": t.corners,
        "simulators": t.simulators,
        "layout": t.layout,
    })
}

/// What an IC template needs and promises: its PDK, the corners and
/// simulators its specs hold on, and the layout generator for its block.
fn ic_text(t: &Template) -> String {
    let mut s = String::new();
    if let Some(pdk) = &t.pdk {
        s.push_str(&format!(
            "PDK: {pdk}. The project must use it (pdk tool, action use).\n"
        ));
    }
    if let Some(set) = &t.corners {
        let on = if t.simulators.is_empty() {
            String::new()
        } else {
            format!(" on {}", t.simulators.join(" and "))
        };
        s.push_str(&format!(
            "The specs hold at every corner of this set{on}; pass it to check_specs, optimize and monte_carlo as corners: {}\n",
            serde_json::to_string(set).unwrap_or_default()
        ));
    }
    if let Some(layout) = &t.layout {
        s.push_str(&format!(
            "Layout: the {} generator draws this block, with the roles {}.\n",
            layout.generator.name(),
            serde_json::to_string(&layout.roles).unwrap_or_default()
        ));
    }
    s
}

/// Design notes for people and models: what to change and why.
fn parameters_text(t: &Template) -> String {
    let mut s = String::from("Parameters to change:\n");
    for p in &t.parameters {
        s.push_str(&format!("  {}: {}. {}\n", p.part, p.controls, p.equation));
    }
    s
}

fn specs_text(t: &Template) -> String {
    let mut s = String::from("Specs (check_specs format):\n");
    for line in t.specs.lines().filter(|l| !l.trim().is_empty()) {
        s.push_str(&format!("  {}\n", line.trim()));
    }
    s
}

fn list(input: &TemplatesInput) -> Result<ToolOutput, String> {
    if let Some(c) = input.category.as_deref().map(str::trim)
        && !c.is_empty()
        && !CATEGORIES.iter().any(|k| k.eq_ignore_ascii_case(c))
    {
        return Err(format!(
            "no category `{c}`; categories are {}",
            CATEGORIES.join(", ")
        ));
    }
    let hits = templates::search(input.category.as_deref(), input.query.as_deref());
    let mut text = format!("{} template(s):\n", hits.len());
    for t in &hits {
        let tag = match &t.pdk {
            Some(pdk) => format!("{}, {pdk}", t.category),
            None => t.category.clone(),
        };
        text.push_str(&format!(
            "  {} [{tag}] {}: {}\n",
            t.id, t.title, t.description
        ));
    }
    if hits.is_empty() {
        text.push_str(&format!(
            "  (nothing matches; categories are {})\n",
            CATEGORIES.join(", ")
        ));
    } else {
        text.push_str(
            "Use show for the design equations and specs, or use to copy one into the project.\n",
        );
    }
    let items: Vec<Value> = hits
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "title": t.title,
                "category": t.category,
                "description": t.description,
                "pdk": t.pdk,
            })
        })
        .collect();
    Ok(ToolOutput::text(text).with_data(json!({"kind": "templates", "items": items})))
}

fn show(t: &Template) -> ToolOutput {
    let summary = summarize(&t.schematic(), &SymbolLibrary::builtin_only());
    let text = format!(
        "{}: {} [{}]\n{}\nAnalysis: {}\n{}{}{}Schematic:\n{}To start from it, call templates with action use, this id and a new circuit file name.\n",
        t.id,
        t.title,
        t.category,
        t.description,
        t.analysis,
        parameters_text(t),
        specs_text(t),
        ic_text(t),
        summary.to_text()
    );
    ToolOutput::text(text).with_data(json!({
        "kind": "template",
        "action": "show",
        "template": template_json(t),
        "summary": summary,
    }))
}

#[async_trait]
impl Tool for Templates {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::Edit
    }

    fn spec(&self) -> ToolSpec {
        spec::<TemplatesInput>("templates", DESCRIPTION)
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: TemplatesInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let result = match input.action {
            TemplatesAction::List => list(&input),
            TemplatesAction::Show => find(input.id.as_deref()).map(show),
            TemplatesAction::Use => match find(input.id.as_deref()) {
                Ok(t) => Ok(self.use_template(t, input.circuit).await),
                Err(e) => Err(e),
            },
        };
        result.unwrap_or_else(ToolOutput::error)
    }
}

/// The template `show` and `use` name.
fn find(id: Option<&str>) -> Result<&'static Template, String> {
    let id =
        id.ok_or("show and use need a template id; call templates with action list to see them")?;
    templates::find(id).ok_or_else(|| {
        let ids: Vec<&str> = templates::all().iter().map(|t| t.id.as_str()).collect();
        format!("no template `{id}`; the library has: {}", ids.join(", "))
    })
}

impl Templates {
    /// Whether the open project uses a PDK of this kind; why not otherwise.
    fn project_uses(&self, kind: &str) -> Result<(), String> {
        let p = self.ws.project().map_err(|e| e.to_string())?;
        let registry = self.ws.runner.config().pdks;
        let mut candidates = registry.names_of_kind(kind);
        let hint = match candidates.len() {
            0 => format!(
                "no {kind} PDK is registered (the user registers one with `aispice pdk add`)"
            ),
            1 => format!(
                "make it use the registered {} first (pdk tool, action use)",
                candidates.remove(0)
            ),
            _ => format!(
                "make it use one of the registered {} first (pdk tool, action use)",
                candidates.join(", ")
            ),
        };
        match self.ws.runner.project_pdk(&p) {
            Ok(Some(pdk)) if pdk.kit.kind.eq_ignore_ascii_case(kind) => Ok(()),
            Ok(Some(pdk)) => Err(format!(
                "this project uses {} ({}); {hint}",
                pdk.name, pdk.kit.kind
            )),
            Ok(None) => Err(format!("this project uses no PDK; {hint}")),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn use_template(&self, t: &'static Template, circuit: Option<String>) -> ToolOutput {
        let Some(circuit) = circuit
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
        else {
            return ToolOutput::error(
                "use needs circuit: the new schematic's file name, e.g. filter.asc",
            );
        };
        if !circuit.to_ascii_lowercase().ends_with(".asc") {
            return ToolOutput::error(format!("`{circuit}` must end in .asc"));
        }
        let specs = specs_file(&circuit);
        if let Some(kind) = &t.pdk
            && let Err(e) = self.project_uses(kind)
        {
            return ToolOutput::error(format!(
                "{} is drawn in the {kind} PDK's devices, and {e}; nothing was written",
                t.id
            ));
        }
        // Refuse before asking for approval: both files must be new and
        // inside the project.
        let checked = tokio::task::spawn_blocking({
            let ws = self.ws.clone();
            let (circuit, specs) = (circuit.clone(), specs.clone());
            move || -> Result<(), String> {
                let p = ws.project().map_err(|e| e.to_string())?;
                for f in [&circuit, &specs] {
                    if p.resolve(f).map_err(|e| e.to_string())?.exists() {
                        return Err(format!(
                            "{f} already exists; choose another name (nothing was written)"
                        ));
                    }
                }
                Ok(())
            }
        })
        .await;
        match checked {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return ToolOutput::error(e),
            Err(e) => return ToolOutput::error(format!("internal error: {e}")),
        }
        let summary_line = format!("Create {circuit} from the {} template", t.id);
        let preview = format!(
            "{}\nNew files: {circuit} and {specs}\n{}",
            t.title,
            t.specs_file_text()
        );
        if !self.ws.approve(&circuit, &summary_line, &preview).await {
            return ToolOutput::text(
                "The user declined creating this circuit; nothing was written. Ask what they would prefer.",
            );
        }
        let ws = self.ws.clone();
        blocking(move || {
            let p = ws.project().map_err(|e| e.to_string())?;
            let sch = t.schematic();
            p.create_as(&circuit, &sch, &format!("Created from template {}", t.id))
                .map_err(|e| format!("No file created. {e}"))?;
            if let Ok(path) = p.resolve(&circuit) {
                ws.after_save(&path);
            }
            // The schematic exists now; the specs file was checked above and
            // is only ever `<circuit>.specs`, resolved inside the project.
            let specs_path = p.resolve(&specs).map_err(|e| e.to_string())?;
            if specs_path.exists() {
                return Err(format!(
                    "Created {circuit}, but {specs} appeared meanwhile and was left as it is"
                ));
            }
            crate::project::atomic_write(&specs_path, t.specs_file_text().as_bytes())
                .map_err(|e| format!("Created {circuit}, but could not write {specs}: {e}"))?;
            let summary = summarize(&sch, p.library());
            let text = format!(
                "Created {circuit} from template {} ({}, {} parts); specs saved to {specs}.\n{}{}{}{}Next: run check_specs on {circuit} to confirm it passes here{}, then edit the specs to your requirements and size the parameters with optimize.\n",
                t.id,
                t.title,
                summary.components.len(),
                summary.to_text(),
                parameters_text(t),
                specs_text(t),
                ic_text(t),
                if t.corners.is_some() { " (with the corners above)" } else { "" },
            );
            Ok(ToolOutput::text(text).with_data(json!({
                "kind": "template",
                "action": "use",
                "circuit": circuit,
                "specs_file": specs,
                "template": template_json(t),
                "summary": summary,
            })))
        })
        .await
    }
}
