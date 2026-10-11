//! Helpers shared by the simulator-dependent tests.
//!
//! ngspice and Xyce tests run whenever the binary is found and skip (with a
//! message) otherwise. LTspice tests also need `AISPICE_LTSPICE_TESTS=1`,
//! because each run under Wine takes seconds and needs a virtual display.
//! Spectre runs on a remote server and needs `AISPICE_SPECTRE_HOST` and
//! `AISPICE_SPECTRE_DIR`, plus `AISPICE_SPECTRE_SETUP` for a command that
//! puts Spectre on the path; nothing about the server is in the repository.

#![allow(dead_code)]

use aispice_core::netlist::{Netlist, build::build};
use aispice_core::symbol::SymbolLibrary;
use aispice_sim::backend::spectre::{Spectre, SpectreConfig};
use aispice_sim::backend::{Ltspice, Ngspice, SimError, SimId, SimJob, SimOutput, Simulator, Xyce};
use aispice_sim::dialect::{Translated, translate};
use aispice_sim::models;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn deck(name: &str) -> String {
    let p = crate_dir()
        .join("tests/fixtures/decks")
        .join(format!("{name}.cir"));
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

pub fn schematic(name: &str) -> PathBuf {
    crate_dir()
        .join("tests/schematics")
        .join(format!("{name}.asc"))
}

pub async fn ngspice() -> Option<Ngspice> {
    let ng = Ngspice::default();
    if ng.detect().await.found {
        Some(ng)
    } else {
        eprintln!("skipped: ngspice not found");
        None
    }
}

pub async fn xyce() -> Option<Xyce> {
    let x = Xyce::default();
    if x.detect().await.found {
        Some(x)
    } else {
        eprintln!("skipped: Xyce not found");
        None
    }
}

pub fn ltspice_enabled() -> bool {
    std::env::var("AISPICE_LTSPICE_TESTS").ok().as_deref() == Some("1")
}

pub async fn ltspice() -> Option<Ltspice> {
    if !ltspice_enabled() {
        eprintln!("skipped: set AISPICE_LTSPICE_TESTS=1 to run LTspice");
        return None;
    }
    let lt = Ltspice {
        headless: Some(true),
        ..Default::default()
    };
    if lt.detect().await.found {
        Some(lt)
    } else {
        eprintln!("skipped: LTspice not found");
        None
    }
}

pub async fn spectre() -> Option<Spectre> {
    let (Ok(host), Ok(dir)) = (
        std::env::var("AISPICE_SPECTRE_HOST"),
        std::env::var("AISPICE_SPECTRE_DIR"),
    ) else {
        eprintln!("skipped: set AISPICE_SPECTRE_HOST and AISPICE_SPECTRE_DIR to run Spectre");
        return None;
    };
    let mut cfg = SpectreConfig::new(host, dir);
    if let Ok(setup) = std::env::var("AISPICE_SPECTRE_SETUP") {
        cfg.command = format!("{setup} && {}", cfg.command);
    }
    let sp = Spectre::new(cfg);
    if sp.detect().await.found {
        Some(sp)
    } else {
        eprintln!("skipped: Spectre host not reachable");
        None
    }
}

pub fn job(text: &str, run_dir: &Path, name: &str) -> SimJob {
    SimJob {
        netlist_text: text.to_string(),
        run_dir: run_dir.to_path_buf(),
        deck_name: name.to_string(),
        timeout: Duration::from_secs(120),
        pdk: None,
    }
}

pub async fn run(sim: &dyn Simulator, text: &str, name: &str) -> Result<SimOutput, SimError> {
    let dir = tempfile::tempdir().unwrap();
    // Keep the run folder for inspection when a test fails.
    let dir = dir.keep();
    sim.run(&job(text, &dir, name), &CancellationToken::new())
        .await
}

/// aispice's netlist of an authored schematic. LTspice writes library
/// symbols with doubled backslashes, which aispice-core's built-in lookup
/// misses (reported), so they are reduced to one first.
pub fn netlist_of(asc: &Path) -> (Netlist, Vec<String>) {
    let (text, _) = aispice_core::encoding::decode(&std::fs::read(asc).unwrap());
    let fixed: String = text
        .lines()
        .map(|l| {
            if l.starts_with("SYMBOL ") {
                l.replace("\\\\", "\\")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let (sch, _) = aispice_core::schematic::parse(&fixed);
    let title = format!("* {}", asc.file_name().unwrap().to_string_lossy());
    let (built, _) = build(&sch, &SymbolLibrary::builtin_only(), &title);
    assert!(built.warnings.is_empty(), "{:?}", built.warnings);
    (built.netlist, built.standard_libs)
}

/// The deck a simulator gets for a schematic: models resolved (from the
/// LTspice library when installed, else aispice's generic set) and the
/// netlist translated to the simulator's dialect.
pub fn deck_for(asc: &Path, sim: SimId) -> Translated {
    let (netlist, standard_libs) = netlist_of(asc);
    if sim == SimId::Ltspice {
        return translate(&netlist, sim.target());
    }
    let lib_dirs = Ltspice::default().lib_dirs();
    let resolved = models::resolve_with(&netlist, &standard_libs, &lib_dirs);
    assert!(
        resolved.missing.is_empty(),
        "{}: missing models {:?}",
        asc.display(),
        resolved.missing
    );
    translate(&resolved.netlist, sim.target())
}

/// Linear interpolation of where `y` first crosses `level` against `x`.
pub fn crossing(x: &[f64], y: &[f64], level: f64) -> Option<f64> {
    for i in 1..x.len() {
        let (a, b) = (y[i - 1] - level, y[i] - level);
        if a == 0.0 {
            return Some(x[i - 1]);
        }
        if a * b < 0.0 {
            return Some(x[i - 1] + (x[i] - x[i - 1]) * a / (a - b));
        }
    }
    None
}

/// Linear interpolation of `y` at `x0`.
pub fn value_at(x: &[f64], y: &[f64], x0: f64) -> f64 {
    for i in 1..x.len() {
        if x[i] >= x0 {
            let t = (x0 - x[i - 1]) / (x[i] - x[i - 1]);
            return y[i - 1] + t * (y[i] - y[i - 1]);
        }
    }
    *y.last().unwrap()
}

pub fn rel(a: f64, b: f64) -> f64 {
    (a - b).abs() / b.abs()
}
