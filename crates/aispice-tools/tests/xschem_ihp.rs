//! xschem import and export against the real thing: IHP's own test benches
//! and xschem itself.
//!
//! Needs the PDK (`AISPICE_PDK_IHP=<path to IHP-Open-PDK/ihp-sg13g2>`) and
//! xschem (`AISPICE_XSCHEM`, else `xschem` on PATH, with its library).
//! Every test passes, with a note on stderr, when either is missing.
//!
//! - every `sg13g2_tests` bench imports; aispice's xschem netlist of it is
//!   xschem's own, line for line; and xschem's netlist matches the imported
//!   `.asc`'s;
//! - every template and every imported bench exports, and xschem's netlist
//!   of each `.sch` matches the `.asc`'s.
//!
//! The built-in symbol definitions are checked against the installed files
//! in `aispice_core::xschem::symbol` (same gate).

use aispice_core::netlist::{CompareOptions, compare_with, parse};
use aispice_core::symbol::SymbolLibrary;
use aispice_core::xschem::import::clean;
use aispice_core::xschem::safety;
use aispice_sim::pdk::Pdk;
use aispice_sim::xschem::Run;
use aispice_tools::templates;
use aispice_tools::xschem::{
    Setup, XschemCheck, check_export, check_import, export_schematic, import_file,
};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

fn gate() -> Option<(PathBuf, Setup)> {
    let Some(pdk) = std::env::var_os("AISPICE_PDK_IHP").map(PathBuf::from) else {
        eprintln!("skipped: AISPICE_PDK_IHP is not set");
        return None;
    };
    let setup = Setup::new(Some(Pdk::open("ihp-sg13g2", &pdk).ok()?));
    if setup.xschem.is_none() || setup.library.len() < 3 {
        eprintln!("skipped: xschem or its library is not installed");
        return None;
    }
    Some((setup.pdk.as_ref()?.root.clone(), setup))
}

fn benches(pdk: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(pdk.join("libs.tech/xschem/sg13g2_tests"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "sch"))
        .collect();
    out.sort();
    out
}

/// Netlist lines without xschem's path comments and with white space
/// collapsed.
fn lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.starts_with("** sch_path") && !l.starts_with("** sym_path"))
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect()
}

fn proven(what: &str, check: &XschemCheck, problems: &mut Vec<String>) {
    match check {
        XschemCheck::Ran { proof } if proof.equivalent => {}
        XschemCheck::Ran { proof } => problems.push(format!("{what}: {:?}", proof.differences)),
        XschemCheck::Skipped { reason } => {
            problems.push(format!("{what}: xschem skipped: {reason}"))
        }
    }
}

#[tokio::test]
async fn every_test_bench_imports_and_xschem_agrees() {
    let Some((pdk, setup)) = gate() else { return };
    let lib = SymbolLibrary::builtin_only();
    let out = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let mut problems = Vec::new();
    let all = benches(&pdk);
    assert!(all.len() > 40, "{}", all.len());
    for sch in &all {
        let name = sch.file_stem().unwrap().to_string_lossy().to_string();
        let roots = vec![sch.parent().unwrap().to_path_buf()];
        let imported = match import_file(&setup, sch, roots.clone(), Some(out.path()), &lib, 1) {
            Ok(i) => i,
            Err(e) => {
                problems.push(format!("{name}: {e}"));
                continue;
            }
        };
        if !imported.proof.equivalent {
            problems.push(format!(
                "{name}: aispice proof {:?}",
                imported.proof.differences
            ));
        }
        // aispice's xschem netlist is xschem's own.
        let verdict = safety::scan(sch, &setup.resolver(roots.clone()), &setup.trusted);
        assert!(verdict.allowed(), "{name}: {:?}", verdict.refused);
        let run = Run {
            library: setup.library.clone(),
            files: verdict.files.clone(),
            models_ngspice: Some(pdk.join("libs.tech/ngspice/models")),
            models_xyce: Some(pdk.join("libs.tech/xyce/models")),
        };
        let theirs =
            aispice_sim::xschem::netlist(setup.xschem.as_ref().unwrap(), sch, &run, &cancel)
                .await
                .unwrap();
        if lines(&theirs) != lines(&imported.xschem.text) {
            problems.push(format!(
                "{name}: aispice's xschem netlist differs from xschem's:\n{}",
                similar::TextDiff::from_lines(
                    lines(&imported.xschem.text).join("\n").as_str(),
                    lines(&theirs).join("\n").as_str()
                )
                .unified_diff()
            ));
        }
        let (a, b) = (
            parse(&clean(&theirs, None).text),
            parse(&clean(&imported.xschem.text, None).text),
        );
        if let Err(m) = compare_with(&a, &b, &CompareOptions::xschem()) {
            problems.push(format!("{name}: {m:?}"));
        }
        // And xschem's netlist matches the .asc's.
        let check = check_import(&setup, sch, roots, &imported, Some(out.path()), &cancel).await;
        proven(&name, &check, &mut problems);
    }
    assert!(problems.is_empty(), "{}", problems.join("\n\n"));
}

#[tokio::test]
async fn templates_and_benches_export_and_xschem_agrees() {
    let Some((pdk, setup)) = gate() else { return };
    let builtin = SymbolLibrary::builtin_only();
    let out = tempfile::tempdir().unwrap();
    let dir = out.path().canonicalize().unwrap();
    let cancel = CancellationToken::new();
    let mut problems = Vec::new();
    let mut circuits: Vec<(String, aispice_core::schematic::Schematic)> = templates::all()
        .iter()
        .map(|t| (t.id.to_string(), t.schematic()))
        .collect();
    for sch in benches(&pdk) {
        let name = sch.file_stem().unwrap().to_string_lossy().to_string();
        let roots = vec![sch.parent().unwrap().to_path_buf()];
        match import_file(&setup, &sch, roots, Some(&dir), &builtin, 1) {
            Ok(i) => circuits.push((format!("{name} (imported)"), i.schematic)),
            Err(e) => problems.push(format!("{name}: {e}")),
        }
    }
    for (name, asc) in &circuits {
        let file = format!("{}.sch", name.replace([' ', '(', ')'], "_"));
        let exported = match export_schematic(
            &setup,
            asc,
            &builtin,
            &format!("* {name}"),
            &dir.join(&file),
            vec![dir.clone()],
            1,
        ) {
            Ok(e) => e,
            Err(e) => {
                problems.push(format!("{name}: {e}"));
                continue;
            }
        };
        if !exported.proof.equivalent {
            problems.push(format!(
                "{name}: aispice proof {:?}",
                exported.proof.differences
            ));
            continue;
        }
        let check = check_export(&setup, &file, &exported, &cancel).await;
        proven(name, &check, &mut problems);
    }
    assert!(problems.is_empty(), "{}", problems.join("\n\n"));
}
