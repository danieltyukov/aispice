//! gm/Id sizing as a tool: characterise a PDK transistor once (cached), then
//! answer "what W and VGS give this gm/Id at this L and current".

use super::schematic_tools::spec;
use crate::pdk::ProjectConfig;
use crate::workspace::Workspace;
use aispice_agent::tool::parse_input;
use aispice_agent::{Tool, ToolContext, ToolOutput, ToolSpec};
use aispice_sim::backend::ngspice::Ngspice;
use aispice_sim::gmid::{self, Grid, Query, Table};
use aispice_sim::pdk::Pdk;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// A number, or text with an SI suffix (`0.5u`, `50u`, `1.2`).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Amount {
    Number(f64),
    Text(String),
}

impl Amount {
    fn value(&self, what: &str) -> Result<f64, String> {
        match self {
            Amount::Number(n) => Ok(*n),
            Amount::Text(t) => aispice_core::units::parse(t)
                .ok_or_else(|| format!("{what} `{t}` is not a number such as 0.5u or 50e-6")),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GmIdInput {
    /// The MOS transistor, by subcircuit name: sg13_lv_nmos, sg13_lv_pmos,
    /// sg13_hv_nmos or sg13_hv_pmos for IHP SG13G2.
    pub device: String,
    /// Target gm/Id in S/A (1/V): about 5 to 10 in strong inversion (fast,
    /// small), 10 to 18 in moderate inversion, 20 and up in weak inversion
    /// (efficient, slow). Leave out to see what the table covers.
    #[serde(default)]
    pub gm_id: Option<f64>,
    /// Channel length, e.g. "0.5u".
    #[serde(default)]
    pub l: Option<Amount>,
    /// Drain-source voltage magnitude. Default: half the device's supply.
    #[serde(default)]
    pub vds: Option<Amount>,
    /// The drain current to size the width for, e.g. "50u".
    #[serde(default)]
    pub id: Option<Amount>,
    /// Or a width, to get the current it carries at this gm/Id.
    #[serde(default)]
    pub w: Option<Amount>,
    /// A registered PDK. Default: the project's, or the only one registered.
    #[serde(default)]
    pub pdk: Option<String>,
    /// Rebuild the table even when a cached one matches the PDK.
    #[serde(default)]
    pub refresh: bool,
}

const DESCRIPTION: &str = r#"
Size a PDK transistor with the gm/Id method. Give the device, the gm/Id you want (S/A), the channel length, and the drain current (or a width); get back W, the number of fingers, VGS, Vth, gm, gds, intrinsic gain gm/gds, Cgg and fT at that bias, and the SpiceLine to give the transistor (`w=... l=... ng=... m=1`). VDS defaults to half the supply. A lookup table made by sweeping the device on ngspice over L, VDS and VGS with the PDK's own models (typical corner) gives the first answer; the first use of a device builds its table (a few seconds) and later uses read it from the cache. The answer is then checked by simulating the device at that size and corrected, since the models do not scale exactly with width. Without gm_id and l, it reports what the table covers. Check the whole circuit by simulating it.
"#;

pub struct GmIdTool {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for GmIdTool {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        spec::<GmIdInput>("gm_id", DESCRIPTION)
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: GmIdInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        match self.answer(ctx, input).await {
            Ok(out) => out,
            Err(e) => ToolOutput::error(e),
        }
    }
}

impl GmIdTool {
    /// The PDK to use: named, the project's, or the only registered one.
    fn pdk(&self, name: Option<String>) -> Result<Pdk, String> {
        let registry = self.ws.runner.config().pdks;
        let name = name
            .or_else(|| {
                let p = self.ws.project().ok()?;
                ProjectConfig::load(p.root()).ok()?.pdk
            })
            .or_else(|| {
                (registry.pdks.len() == 1)
                    .then(|| registry.pdks.keys().next().cloned())
                    .flatten()
            })
            .ok_or_else(|| {
                if registry.pdks.is_empty() {
                    "no PDK is registered; the user registers one with `aispice pdk add <name> <path>`".to_string()
                } else {
                    format!(
                        "name the PDK (pdk): {} are registered",
                        registry.pdks.keys().cloned().collect::<Vec<_>>().join(", ")
                    )
                }
            })?;
        registry.open(&name).map_err(|e| e.to_string())
    }

    async fn answer(&self, ctx: &ToolContext, input: GmIdInput) -> Result<ToolOutput, String> {
        let pdk = tokio::task::spawn_blocking({
            let tool = GmIdTool {
                ws: self.ws.clone(),
            };
            let name = input.pdk.clone();
            move || tool.pdk(name)
        })
        .await
        .map_err(|e| format!("internal error: {e}"))??;
        let device = pdk
            .kit
            .device(input.device.trim())
            .filter(|d| d.is_mosfet())
            .ok_or_else(|| {
                let mos: Vec<&str> = pdk
                    .kit
                    .devices
                    .iter()
                    .filter(|d| d.is_mosfet())
                    .map(|d| d.name)
                    .collect();
                format!(
                    "`{}` is not a MOS transistor of {}; use one of {}",
                    input.device,
                    pdk.name,
                    mos.join(", ")
                )
            })?;
        let grid = Grid::default_for(pdk.kit, device);
        let cache = self
            .ws
            .runner
            .config()
            .gmid_cache_dir
            .or_else(gmid::default_cache_dir)
            .ok_or("this system has no cache folder for gm/Id tables")?;
        ctx.progress(format!("gm/Id table for {}", device.name), None);
        let started = std::time::Instant::now();
        let (table, cached, mut warnings) = gmid::table(
            &pdk,
            device,
            &grid,
            &cache,
            input.refresh,
            &Ngspice::default(),
            &ctx.cancel,
        )
        .await
        .map_err(|e| e.to_string())?;
        let source = if cached {
            format!(
                "Table read from the cache ({}).",
                gmid::cache_path(&cache, &pdk.name, device.name).display()
            )
        } else {
            format!(
                "Table built on ngspice in {:.1} s and cached in {}.",
                started.elapsed().as_secs_f64(),
                gmid::cache_path(&cache, &pdk.name, device.name).display()
            )
        };
        let (Some(gm_id), Some(l)) = (input.gm_id, input.l.as_ref()) else {
            let source = with_warnings(source, &warnings);
            return Ok(ToolOutput::text(format!("{}{source}\n", coverage(&table)))
                .with_data(json!({"kind": "generic", "value": {"device": device.name, "grid": table.grid, "cached": cached}})));
        };
        let vds = match &input.vds {
            Some(v) => v.value("vds")?.abs(),
            None => gmid::supply(pdk.kit, device) / 2.0,
        };
        let query = Query {
            gm_id,
            l: l.value("l")?,
            vds,
            id: input
                .id
                .as_ref()
                .map(|a| a.value("id"))
                .transpose()?
                .map(f64::abs),
            w: input.w.as_ref().map(|a| a.value("w")).transpose()?,
        };
        let mut sizing = gmid::size(&table, &query).map_err(|e| e.to_string())?;
        if query.id.is_some() || query.w.is_some() {
            ctx.progress(
                format!("checking the sized {} on ngspice", device.name),
                None,
            );
            let (refined, ran) = gmid::refine(
                &pdk,
                device,
                &table,
                &query,
                &sizing,
                &Ngspice::default(),
                &ctx.cancel,
            )
            .await
            .map_err(|e| e.to_string())?;
            sizing = refined;
            for w in ran {
                if !warnings.contains(&w) {
                    warnings.push(w);
                }
            }
        }
        let source = with_warnings(source, &warnings);
        let text = format!("{}{source}\n", gmid::describe(&table, &sizing));
        Ok(ToolOutput::text(text).with_data(json!({
            "kind": "generic",
            "value": {"device": device.name, "pdk": pdk.name, "sizing": sizing, "cached": cached},
        })))
    }
}

/// `source`, then the warnings of the ngspice runs behind it (such as
/// that ngspice ran without a sandbox), at most ten.
fn with_warnings(source: String, warnings: &[String]) -> String {
    let mut s = source;
    for w in warnings.iter().take(10) {
        s.push_str(&format!("\nWarning: {w}"));
    }
    s
}

/// What a table covers, for a request without a target.
fn coverage(t: &Table) -> String {
    use aispice_core::units::format_with_unit as u;
    let ls: Vec<String> = t.grid.l.iter().map(|l| u(*l, "m")).collect();
    format!(
        "{} table: L {}; VDS {} to {} in {} steps; VGS 0 to {} in {} steps; W {} (one finger). Ask with gm_id and l, plus id (or w), for a size.\n",
        t.device,
        ls.join(", "),
        u(t.grid.vds_start, "V"),
        u(t.grid.vds_stop, "V"),
        u(t.grid.vds_step, "V"),
        u(t.grid.vgs_stop, "V"),
        u(t.grid.vgs_step, "V"),
        u(t.grid.w, "m")
    )
}
