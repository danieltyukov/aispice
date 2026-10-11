//! Lay out every netlist in `tests/fixtures/layout/` (or the files given on
//! the command line) and write the schematic, an SVG and a PNG of each under
//! `target/layout-check/`, with a table of quality metrics, for checking the
//! drawings by eye.
//!
//! ```text
//! cargo run -p aispice-core --example layout_gallery
//! cargo run -p aispice-core --example layout_gallery -- path/to/deck.cir ...
//! cargo run -p aispice-core --example layout_gallery -- --compare <netlist dir>
//! ```
//!
//! `--compare` checks netlists written by another netlister (LTspice's
//! `-netlist`, run on the generated `.asc` files) against the fixtures:
//! for every `<name>.net` in the folder, `tests/fixtures/layout/<name>.cir`
//! must describe the same circuit.

use aispice_core::layout::{LayoutOptions, LayoutResult, compare_netlists, from_netlist, quality};
use aispice_core::netlist::parse;
use aispice_core::render::{RenderOptions, render_svg};
use aispice_core::schematic::write;
use aispice_core::symbol::SymbolLibrary;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn inputs() -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if !args.is_empty() {
        return Ok(args);
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/layout");
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "cir"))
        .collect();
    files.sort();
    Ok(files)
}

/// Compare every `<name>.net` in `dir` with the fixture of the same name.
fn compare_dir(dir: &Path) -> Result<(), Box<dyn Error>> {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/layout");
    let mut nets: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "net"))
        .collect();
    nets.sort();
    let mut failed = 0;
    for net in nets {
        let stem = net.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let Ok(input) = std::fs::read_to_string(fixtures.join(format!("{stem}.cir"))) else {
            continue;
        };
        let bytes = std::fs::read(&net)?;
        let (other, _) = aispice_core::encoding::decode(&bytes);
        match compare_netlists(&parse(&input), &parse(&other)) {
            Ok(()) => println!("{stem:<24} matches"),
            Err(p) => {
                failed += 1;
                println!("{stem:<24} DIFFERS: {}", p.join("; "));
            }
        }
    }
    if failed > 0 {
        return Err(format!("{failed} netlist(s) differ").into());
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--compare") {
        let dir = args.get(1).ok_or("--compare needs a folder")?;
        return compare_dir(Path::new(dir));
    }
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/layout-check");
    std::fs::create_dir_all(&out)?;
    let lib = SymbolLibrary::builtin_only();
    println!(
        "{:<24} {:>6} {:>5} {:>5} {:>5} {:>5} {:>5} {:>5} {:>7} {:>8}",
        "circuit", "score", "cross", "body", "parts", "text", "label", "bend", "wire", "ms"
    );
    for path in inputs()? {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("circuit")
            .to_string();
        let bytes = std::fs::read(&path)?;
        let started = Instant::now();
        // A schematic is measured and drawn as it is; a netlist is laid out.
        let is_asc = path
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("asc"));
        let r = if is_asc {
            let (sch, _) = aispice_core::schematic::parse_bytes(&bytes);
            LayoutResult {
                quality: quality(&sch, &lib),
                schematic: sch,
                warnings: Vec::new(),
                renames: Vec::new(),
            }
        } else {
            let (text, _) = aispice_core::encoding::decode(&bytes);
            match from_netlist(&parse(&text), &lib, &LayoutOptions::default()) {
                Ok(r) => r,
                Err(e) => {
                    println!("{stem:<24} FAILED: {e}");
                    continue;
                }
            }
        };
        let ms = started.elapsed().as_millis();
        let q = &r.quality;
        println!(
            "{:<24} {:>6.1} {:>5} {:>5} {:>5} {:>5} {:>5} {:>5} {:>7} {:>8}",
            stem,
            q.score,
            q.crossings,
            q.wires_through_bodies,
            q.overlapping_parts,
            q.overlapping_text,
            q.labels,
            q.bends,
            q.wire_length,
            ms
        );
        for w in &r.warnings {
            println!("    note: {w}");
        }
        if !is_asc {
            std::fs::write(out.join(format!("{stem}.asc")), write(&r.schematic))?;
        }
        let svg = render_svg(&r.schematic, &lib, &RenderOptions::default()).svg;
        std::fs::write(out.join(format!("{stem}.svg")), &svg)?;
        #[cfg(feature = "raster")]
        std::fs::write(
            out.join(format!("{stem}.png")),
            aispice_core::render::render_png(&svg, 1.5)?,
        )?;
    }
    println!("written to {}", out.display());
    Ok(())
}
