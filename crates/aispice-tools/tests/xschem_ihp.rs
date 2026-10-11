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
//!
//! xschem runs only inside aispice's sandbox (bubblewrap where it can start,
//! else Landlock), through the same `aispice_sim::xschem::netlist` the
//! product uses, and the product's own checks (`check_import` with the
//! user's opt-in, `check_export`) must agree too. On a machine with neither
//! sandbox, the xschem comparisons are skipped with a note and only
//! aispice's own proofs run; xschem is never run outside the sandbox.

use aispice_core::netlist::{CompareOptions, compare_with, parse};
use aispice_core::symbol::SymbolLibrary;
use aispice_core::xschem::import::clean;
use aispice_core::xschem::safety;
use aispice_sim::pdk::Pdk;
use aispice_sim::xschem::{Run, Sandbox};
use aispice_tools::templates;
use aispice_tools::xschem::{
    Effort, ImportCheck, Setup, XschemCheck, check_export, check_import, export_schematic,
    import_file,
};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

const QUICK: Effort = Effort {
    effort: 1,
    max_elements: None,
};

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

/// The sandbox to use here (bubblewrap where it can start, else Landlock),
/// or None when the machine has neither, which skips the comparisons that
/// run xschem. xschem is run only inside the sandbox, never directly.
async fn sandbox() -> Option<Sandbox> {
    match Sandbox::select(&CancellationToken::new()).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("note: no sandbox here ({e}); skipping the xschem comparisons");
            None
        }
    }
}

/// xschem's own netlist of a trusted file, inside the sandbox.
async fn xschem_netlist(
    setup: &Setup,
    sandbox: &Sandbox,
    sch: &Path,
    run: &Run,
    cancel: &CancellationToken,
) -> String {
    let x = setup.xschem.as_ref().unwrap();
    aispice_sim::xschem::netlist(x, sandbox, sch, run, cancel)
        .await
        .unwrap()
}

fn run_for(setup: &Setup, pdk: &Path, files: Vec<PathBuf>) -> Run {
    Run {
        library: setup.library.clone(),
        files,
        // IHP's benches live under the PDK, a trusted root, so nothing is
        // staged; the project roots are only used for files outside it.
        project_roots: Vec::new(),
        models_ngspice: Some(pdk.join("libs.tech/ngspice/models")),
        models_xyce: Some(pdk.join("libs.tech/xyce/models")),
    }
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
    let sandbox = sandbox().await;
    let lib = SymbolLibrary::builtin_only();
    let out = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let mut problems = Vec::new();
    let all = benches(&pdk);
    assert!(all.len() > 40, "{}", all.len());
    for sch in &all {
        let name = sch.file_stem().unwrap().to_string_lossy().to_string();
        let roots = vec![sch.parent().unwrap().to_path_buf()];
        let imported = match import_file(&setup, sch, roots.clone(), Some(out.path()), &lib, QUICK)
        {
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
        // The product never runs xschem on an import without the user's
        // opt-in, whether or not a sandbox exists.
        let check = check_import(
            &setup,
            sch,
            roots.clone(),
            &imported,
            Some(out.path()),
            ImportCheck::Off,
            &cancel,
        )
        .await;
        assert!(matches!(check, XschemCheck::Skipped { .. }), "{check:?}");
        // With a sandbox: aispice's xschem netlist is xschem's own, xschem's
        // netlist matches the .asc's, and the product's own opt-in check
        // agrees.
        let Some(sandbox) = &sandbox else { continue };
        let verdict = safety::scan(sch, &setup.resolver(roots.clone()), &setup.trusted);
        assert!(verdict.allowed(), "{name}: {:?}", verdict.refused);
        let run = run_for(&setup, &pdk, verdict.files.clone());
        let theirs = xschem_netlist(&setup, sandbox, sch, &run, &cancel).await;
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
        let mine = parse(&clean(&theirs, Some(out.path())).text);
        if let Err(m) = compare_with(&mine, &imported.asc_netlist, &imported.compare) {
            problems.push(format!("{name}: xschem against the .asc: {m:?}"));
        }
        let check = check_import(
            &setup,
            sch,
            roots,
            &imported,
            Some(out.path()),
            ImportCheck::On,
            &cancel,
        )
        .await;
        proven(&name, &check, &mut problems);
    }
    assert!(problems.is_empty(), "{}", problems.join("\n\n"));
}

#[tokio::test]
async fn templates_and_benches_export_and_xschem_agrees() {
    let Some((pdk, setup)) = gate() else { return };
    let sandbox = sandbox().await;
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
        match import_file(&setup, &sch, roots, Some(&dir), &builtin, QUICK) {
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
            QUICK,
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
        // No value in an export is one xschem would evaluate.
        assert!(
            !exported.text.to_ascii_lowercase().contains("tcleval"),
            "{name}"
        );
        // With a sandbox: xschem netlists the export to the same circuit,
        // and the product's own check agrees.
        let Some(sandbox) = &sandbox else { continue };
        let path = dir.join(&file);
        std::fs::write(&path, &exported.text).unwrap();
        let verdict = safety::scan(&path, &setup.resolver(vec![dir.clone()]), &setup.trusted);
        assert!(verdict.allowed(), "{name}: {:?}", verdict.refused);
        let run = run_for(&setup, &pdk, verdict.files.clone());
        let theirs = xschem_netlist(&setup, sandbox, &path, &run, &cancel).await;
        let theirs = parse(&clean(&theirs, None).text);
        if let Err(m) = compare_with(&exported.asc_netlist, &theirs, &exported.compare) {
            problems.push(format!("{name}: {m:?}"));
        }
        let check = check_export(&setup, &file, &exported, &cancel).await;
        proven(name, &check, &mut problems);
    }
    assert!(problems.is_empty(), "{}", problems.join("\n\n"));
}
