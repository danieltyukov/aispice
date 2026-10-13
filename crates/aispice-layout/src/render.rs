//! Layout pictures: a cell drawn to PNG by KLayout in batch mode with the
//! PDK's layer colours.

use crate::tools::{self, LayoutError, PdkLayoutFiles, Programs, RunDir};
use std::path::Path;
use tokio_util::sync::CancellationToken;

const SCRIPT: &str = include_str!("../klayout/render.py");
/// PNGs larger than this are not read back.
const MAX_PNG: u64 = 32 << 20;

/// Draw `cell` of `gds` as a PNG of the given size, in pixels.
pub async fn png(
    programs: &Programs,
    files: &PdkLayoutFiles,
    gds: &Path,
    cell: &str,
    size: (u32, u32),
    cancel: &CancellationToken,
) -> Result<Vec<u8>, LayoutError> {
    tools::check_path(gds)?;
    tools::check_name("cell", cell)?;
    let (w, h) = (size.0.clamp(64, 4096), size.1.clamp(64, 4096));
    let run = RunDir::new()?;
    let script = run.script("render.py", SCRIPT)?;
    let out = run.path().join("layout.png");
    let lyp = if files.lyp.is_file() {
        files.lyp.display().to_string()
    } else {
        String::new()
    };
    let res = tools::klayout(
        programs,
        &run,
        &script,
        &[
            ("gds", gds.display().to_string()),
            ("cell", cell.to_string()),
            ("lyp", lyp),
            ("out", out.display().to_string()),
            ("width", w.to_string()),
            ("height", h.to_string()),
        ],
        &tools::Access::new().read(&files.root).read(gds),
        cancel,
    )
    .await?;
    if !res.success() || !tools::is_output(&out) {
        return Err(LayoutError::Failed {
            program: "KLayout (rendering)".into(),
            message: tools::tail(&res.combined(), 8),
        });
    }
    let png = tools::read_output(&out, MAX_PNG)?;
    if png.is_empty() {
        return Err(LayoutError::Failed {
            program: "KLayout (rendering)".into(),
            message: "it wrote an empty picture".into(),
        });
    }
    Ok(png)
}
