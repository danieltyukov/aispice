//! The tools an agent (in the app, or any MCP client) uses to work on circuits.
//!
//! Every tool returns text written for a language model plus, in `data`, a
//! structured payload the desktop app renders as a card. Shapes of `data` are
//! the `ToolData` union in `app/src/ipc/types.ts`.

mod gmid_tools;
mod pdk_tools;
mod render_tools;
mod schematic_tools;
mod sim_tools;
mod template_tools;

use crate::workspace::Workspace;
use aispice_agent::{Registry, ToolOutput};
use std::sync::Arc;

pub use gmid_tools::*;
pub use pdk_tools::*;
pub use render_tools::*;
pub use schematic_tools::*;
pub use sim_tools::*;
pub use template_tools::*;

/// All tools, bound to one workspace.
pub fn registry(ws: Arc<Workspace>) -> Registry {
    let mut r = Registry::new();
    r.register(ListCircuits { ws: ws.clone() });
    r.register(ReadSchematic { ws: ws.clone() });
    r.register(EditSchematic { ws: ws.clone() });
    r.register(CreateSchematic { ws: ws.clone() });
    r.register(Lint { ws: ws.clone() });
    r.register(NetlistTool { ws: ws.clone() });
    r.register(History { ws: ws.clone() });
    r.register(RenderSchematic { ws: ws.clone() });
    r.register(Symbols { ws: ws.clone() });
    r.register(Simulators { ws: ws.clone() });
    r.register(Simulate { ws: ws.clone() });
    r.register(MeasureTool { ws: ws.clone() });
    r.register(CheckSpecs { ws: ws.clone() });
    r.register(Plot { ws: ws.clone() });
    r.register(ReadWaveform { ws: ws.clone() });
    r.register(PolesZeros { ws: ws.clone() });
    r.register(OperatingPoint { ws: ws.clone() });
    r.register(Sweep { ws: ws.clone() });
    r.register(MonteCarlo { ws: ws.clone() });
    r.register(Optimize { ws: ws.clone() });
    r.register(Templates { ws: ws.clone() });
    r.register(PdkTool { ws: ws.clone() });
    r.register(GmIdTool { ws });
    r
}

/// Run blocking file work off the async executor and turn a panic or error
/// into a tool error the model can read.
pub(crate) async fn blocking<F>(f: F) -> ToolOutput
where
    F: FnOnce() -> Result<ToolOutput, String> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(out)) => out,
        Ok(Err(msg)) => ToolOutput::error(msg),
        Err(e) => ToolOutput::error(format!("internal error: {e}")),
    }
}
