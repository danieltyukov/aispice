//! Process design kits as a tool: what is registered, what a PDK holds, its
//! devices, and making a project use one. Registering is not here: that is
//! the user's call (`aispice pdk add`), since it makes aispice load compiled
//! code from the folder.

use super::blocking;
use super::schematic_tools::spec;
use crate::pdk::{PROJECT_FILE, ProjectConfig, Registry, with_pdk};
use crate::workspace::Workspace;
use aispice_agent::tool::parse_input;
use aispice_agent::{Tool, ToolContext, ToolOutput, ToolSpec};
use aispice_core::pdk::Kit;
use aispice_sim::dialect::Target;
use aispice_sim::pdk::Pdk;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PdkAction {
    /// The registered PDKs, and the one the open project uses.
    List,
    /// One PDK in full: folder, commit, compiled models, corner libraries
    /// with their sections, and the model header aispice adds.
    Show,
    /// The PDK's devices: the symbol to place, pins, default parameters.
    Devices,
    /// Make the open project use this PDK (writes `aispice.toml`).
    Use,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PdkInput {
    pub action: PdkAction,
    /// The registered name, e.g. `ihp-sg13g2`. Default for show and
    /// devices: the project's PDK.
    #[serde(default)]
    pub name: Option<String>,
}

const DESCRIPTION: &str = r#"
Process design kits (PDKs) for transistor-level IC design. `list` shows the PDKs the user has registered and the one the open project uses. `show` describes one: its folder and commit, the compiled device models it loads, its corner libraries with their sections, and the model header aispice adds to every netlist (`.lib <corner library> mos_tt` and the like). `devices` lists the devices with the symbol to place (for example sg13g2\sg13_lv_nmos), the pins in order and the default parameters; set W, L, fingers (ng) and multiplier (m) through the SpiceLine attribute, e.g. `w=2u l=0.5u ng=2 m=1`. `use` makes the open project use a registered PDK by writing aispice.toml. Only the user can register a PDK (with `aispice pdk add`). A PDK project simulates on ngspice or Xyce; LTspice cannot load PDK models.
"#;

pub struct PdkTool {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for PdkTool {
    fn spec(&self) -> ToolSpec {
        spec::<PdkInput>("pdk", DESCRIPTION)
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: PdkInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let registry = self.ws.runner.config().pdks;
        match input.action {
            PdkAction::Use => self.use_pdk(registry, input.name).await,
            action => {
                let ws = self.ws.clone();
                blocking(move || {
                    let project_pdk = ws
                        .project()
                        .ok()
                        .and_then(|p| ProjectConfig::load(p.root()).ok())
                        .and_then(|c| c.pdk);
                    match action {
                        PdkAction::List => Ok(list(&registry, project_pdk.as_deref())),
                        PdkAction::Show => {
                            let name = input.name.or(project_pdk).ok_or(
                                "show needs a PDK name; call pdk with action list to see them",
                            )?;
                            let pdk = registry.open(&name).map_err(|e| e.to_string())?;
                            Ok(show(&pdk))
                        }
                        PdkAction::Devices => {
                            let kit = match input.name.or(project_pdk) {
                                Some(name) => registry.open(&name).map_err(|e| e.to_string())?.kit,
                                None => &aispice_core::pdk::IHP_SG13G2,
                            };
                            Ok(devices(kit))
                        }
                        PdkAction::Use => unreachable!("handled above"),
                    }
                })
                .await
            }
        }
    }
}

fn list(registry: &Registry, project_pdk: Option<&str>) -> ToolOutput {
    let mut text = String::new();
    let mut items = Vec::new();
    if registry.pdks.is_empty() {
        text.push_str("No PDK is registered. The user registers one with `aispice pdk add <name> <path>`, for example `aispice pdk add ihp-sg13g2 ~/IHP-Open-PDK/ihp-sg13g2`.\n");
    }
    for (name, entry) in &registry.pdks {
        match Pdk::open(name, &entry.path) {
            Ok(p) => {
                text.push_str(&format!("{}\n", summary_line(&p)));
                items.push(json!({"name": name, "path": p.root, "commit": p.commit, "ok": true}));
            }
            Err(e) => {
                text.push_str(&format!(
                    "{name}: {} (unusable: {e})\n",
                    entry.path.display()
                ));
                items.push(
                    json!({"name": name, "path": entry.path, "ok": false, "error": e.to_string()}),
                );
            }
        }
    }
    match project_pdk {
        Some(p) => text.push_str(&format!("The open project uses `{p}` ({PROJECT_FILE}).\n")),
        None => text.push_str(&format!(
            "The open project uses no PDK (board level). `use` with a registered name sets one in {PROJECT_FILE}.\n"
        )),
    }
    ToolOutput::text(text)
        .with_data(json!({"kind": "generic", "value": {"pdks": items, "project_pdk": project_pdk}}))
}

/// One line about a registered PDK: name, folder, commit, what it can run on.
pub fn summary_line(p: &Pdk) -> String {
    let commit = p
        .commit
        .as_deref()
        .map(|c| format!(", commit {}", &c[..c.len().min(12)]))
        .unwrap_or_default();
    let xyce = if p.xyce_models && !p.xyce_plugins.is_empty() {
        "ngspice and Xyce"
    } else {
        "ngspice"
    };
    format!(
        "{}: {} ({}{commit}); simulates on {xyce}",
        p.name,
        p.root.display(),
        p.kit.title
    )
}

/// Everything about a registered PDK, for `show`.
pub fn describe(p: &Pdk) -> String {
    let mut t = format!("{}\n", summary_line(p));
    t.push_str(&format!(
        "Compiled models for ngspice (OSDI): {}\n",
        names(&p.osdi)
    ));
    t.push_str(&format!(
        "Xyce: {}; plugins: {}\n",
        if p.xyce_models {
            "model libraries present"
        } else {
            "no model libraries"
        },
        if p.xyce_plugins.is_empty() {
            "none".to_string()
        } else {
            names(&p.xyce_plugins)
        }
    ));
    for n in &p.notes {
        t.push_str(&format!("Note: {n}\n"));
    }
    t.push_str("Corner libraries (sections):\n");
    for lib in p.kit.libraries {
        t.push_str(&format!(
            "  {} [{}]: {}\n",
            lib.file,
            lib.family.prefix(),
            p.sections(lib.file, Target::Ngspice).join(", ")
        ));
    }
    t.push_str("Model header aispice adds for the device classes a circuit uses:\n");
    for lib in p.kit.libraries {
        t.push_str(&format!(
            "  {}\n",
            aispice_sim::pdk::lib_line(
                &p.models_dir(Target::Ngspice).join(lib.file),
                &lib.family.default_section()
            )
        ));
    }
    t
}

fn names(paths: &[std::path::PathBuf]) -> String {
    paths
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn show(p: &Pdk) -> ToolOutput {
    let libraries: Vec<Value> = p
        .kit
        .libraries
        .iter()
        .map(|l| {
            json!({
                "file": l.file,
                "family": l.family,
                "sections": p.sections(l.file, Target::Ngspice),
            })
        })
        .collect();
    ToolOutput::text(describe(p))
        .with_data(json!({"kind": "generic", "value": {"pdk": p, "libraries": libraries}}))
}

/// The devices of a kit as text: what to place and how to size it.
pub fn devices_text(kit: &Kit) -> String {
    let mut t = format!(
        "{} devices. Place the symbol, wire every pin (the body or substrate pin too), and set sizes in SpiceLine.\n",
        kit.title
    );
    let lib = aispice_core::symbol::SymbolLibrary::builtin_only();
    for d in kit.devices {
        let symbol = d
            .symbol
            .map(|s| s.replace('/', "\\"))
            .unwrap_or_else(|| "(no symbol: use in a netlist)".into());
        // The symbol's pin names, which edits refer to (M1.D, R1.BN); the
        // subcircuit's own node names when there is no symbol.
        let pins: Vec<String> = match d.symbol.and_then(|s| lib.resolve(s).ok()) {
            Some((def, _)) => def
                .pins_in_spice_order()
                .iter()
                .map(|p| p.name.clone())
                .collect(),
            None => d.pins.iter().map(|p| p.to_string()).collect(),
        };
        t.push_str(&format!(
            "  {} [{}] symbol {symbol}; pins {}; {}{}\n",
            d.name,
            d.family.prefix(),
            pins.join(" "),
            if d.params.is_empty() {
                String::new()
            } else {
                format!("defaults {}; ", d.params)
            },
            d.description
        ));
    }
    t.push_str(&format!(
        "Supplies: {} V core devices, {} V thick-oxide (hv) devices.\n",
        kit.vdd, kit.vdd_hv
    ));
    t
}

fn devices(kit: &'static Kit) -> ToolOutput {
    ToolOutput::text(devices_text(kit))
        .with_data(json!({"kind": "generic", "value": {"kind": kit.kind, "devices": kit.devices}}))
}

impl PdkTool {
    async fn use_pdk(&self, registry: Registry, name: Option<String>) -> ToolOutput {
        let Some(name) = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()) else {
            return ToolOutput::error(
                "use needs the name of a registered PDK; call pdk with action list to see them",
            );
        };
        let pdk = match registry.open(&name) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let project = match self.ws.project() {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let path = match project.resolve(PROJECT_FILE) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let existing = std::fs::read_to_string(&path).ok();
        let text = match with_pdk(existing.as_deref(), &name) {
            Ok(t) => t,
            Err(e) => return ToolOutput::error(e),
        };
        let diff = format!(
            "--- {PROJECT_FILE}\n{}\n+++ {PROJECT_FILE}\n{text}",
            existing.as_deref().unwrap_or("(new file)")
        );
        if !self
            .ws
            .approve(
                PROJECT_FILE,
                &format!("Use PDK {name} in this project"),
                &diff,
            )
            .await
        {
            return ToolOutput::text(format!(
                "The user declined; {PROJECT_FILE} was not changed."
            ));
        }
        if let Err(e) = crate::project::atomic_write(&path, text.as_bytes()) {
            return ToolOutput::error(format!("could not write {PROJECT_FILE}: {e}"));
        }
        ToolOutput::text(format!(
            "The project now uses PDK {name}: {PROJECT_FILE} says `pdk = \"{name}\"`. Every netlist gets the PDK's model header, and simulations run on ngspice or Xyce.\n{}{}",
            summary_line(&pdk),
            "\nCall pdk with action devices to see what to place.\n"
        ))
        .with_data(json!({"kind": "generic", "value": {"project_pdk": name}}))
    }
}
