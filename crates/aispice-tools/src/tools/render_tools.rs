//! Drawing a schematic for people and for models that can see.

use super::blocking;
use super::schematic_tools::spec;
use crate::workspace::Workspace;
use aispice_agent::tool::parse_input;
use aispice_agent::{Tool, ToolContext, ToolOutput, ToolSpec};
use aispice_core::render::{RenderOptions, render_png, render_svg};
use async_trait::async_trait;
use base64::Engine as _;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Debug, Deserialize, JsonSchema, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    /// A PNG image, for models that read images.
    #[default]
    Png,
    /// SVG markup as text.
    Svg,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RenderInput {
    pub circuit: String,
    #[serde(default)]
    pub format: ImageFormat,
}

pub struct RenderSchematic {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for RenderSchematic {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        spec::<RenderInput>(
            "render_schematic",
            "Draw a schematic as an image (PNG by default) so you can look at its layout: overlapping parts, crossing wires, unreadable labels. For connectivity, read_schematic is more precise than looking.",
        )
    }

    async fn call(&self, _ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: RenderInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let ws = self.ws.clone();
        blocking(move || {
            let p = ws.project().map_err(|e| e.to_string())?;
            let (sch, _) = p.load(&input.circuit).map_err(|e| e.to_string())?;
            let out = render_svg(&sch, p.library(), &RenderOptions::default());
            let mut note = format!(
                "{} ({} x {} units)",
                input.circuit,
                out.bounds.width(),
                out.bounds.height()
            );
            for w in &out.warnings {
                note.push_str(&format!("\nnote: {w}"));
            }
            match input.format {
                ImageFormat::Svg => Ok(ToolOutput::text(format!("{note}\n{}", out.svg))
                    .with_data(json!({"kind": "plot", "svg": out.svg}))),
                ImageFormat::Png => {
                    // About 1.5 px per schematic unit keeps text legible without
                    // sending a huge image.
                    let longest = out.bounds.width().max(out.bounds.height()).max(1) as f32;
                    let scale = (1600.0 / longest).clamp(0.5, 2.5);
                    let png = render_png(&out.svg, scale).map_err(|e| e.to_string())?;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
                    Ok(ToolOutput::text(note)
                        .with_image("image/png", b64)
                        .with_data(json!({"kind": "plot", "svg": out.svg})))
                }
            }
        })
        .await
    }
}
