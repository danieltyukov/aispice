//! `chip.py`, the chip assembly script, on block files made to mislead it.
//! Needs KLayout and `AISPICE_PDK_IHP` (the script loads IHP's pcell
//! library); passes with a note when either is missing.

use aispice_layout::chip::assemble;
use aispice_layout::gds::testwrite::{TestCell, library};
use aispice_layout::tools::{self, PdkLayoutFiles, Programs};
use aispice_sim::pdk::Pdk;
use serde_json::{Value, json};
use std::path::Path;
use tokio_util::sync::CancellationToken;

fn setup() -> Option<(Programs, PdkLayoutFiles)> {
    let Some(path) = std::env::var_os("AISPICE_PDK_IHP") else {
        eprintln!("skipped: AISPICE_PDK_IHP is not set");
        return None;
    };
    let programs = Programs::default();
    if programs.klayout().is_err() {
        eprintln!("skipped: KLayout is not installed");
        return None;
    }
    let pdk = Pdk::open("ihp-sg13g2", Path::new(&path)).unwrap();
    let files = PdkLayoutFiles::of(&pdk).unwrap();
    if files.check_pcells().is_err() {
        eprintln!("skipped: the PDK's pcell submodules are not checked out");
        return None;
    }
    Some((programs, files))
}

/// A plan of blocks only: each (file, its top cell) read in order, every
/// top placed once.
fn plan(dir: &Path, blocks: &[(&Path, &str)]) -> Value {
    let libraries: Vec<Value> = blocks
        .iter()
        .map(|(f, c)| json!({"file": f, "cells": [c]}))
        .collect();
    let instances: Vec<Value> = blocks
        .iter()
        .enumerate()
        .map(|(i, (_, c))| json!({"cell": c, "rot": 0, "mirror": false, "x": i as i64 * 100_000, "y": 0}))
        .collect();
    json!({
        "top": "t",
        "libraries": libraries,
        "instances": instances,
        "pcells": [],
        "shapes": [],
        "texts": [],
        "out": dir.join("t.gds"),
        "lvs_out": dir.join("t_lvs.gds"),
        "lvs_drop": [],
    })
}

fn cell(name: &str, layer: i16, refs: &[&str]) -> TestCell {
    TestCell {
        name: name.into(),
        rects: vec![(layer, 0, 0, 0, 1000, 1000)],
        refs: refs.iter().map(|r| (r.to_string(), 2000, 0)).collect(),
        ..Default::default()
    }
}

/// Block A, read first, has a subcell `ota`; block B's top is `ota`.
/// KLayout renames the later file's cell, so by name alone the chip would
/// place A's subcell where B belongs.
#[tokio::test]
async fn a_block_top_named_like_an_earlier_files_cell_is_refused() {
    let Some((programs, files)) = setup() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("blka.gds");
    let b = dir.path().join("ota.gds");
    std::fs::write(
        &a,
        library(&[cell("blka", 8, &["ota"]), cell("ota", 8, &[])]),
    )
    .unwrap();
    std::fs::write(&b, library(&[cell("ota", 10, &[])])).unwrap();
    let cancel = CancellationToken::new();
    let e = assemble::draw(
        &programs,
        &files,
        &plan(dir.path(), &[(&a, "blka"), (&b, "ota")]),
        dir.path(),
        &cancel,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("has no cell ota of its own"), "{e}");
    assert!(!dir.path().join("t.gds").exists());

    // The same blocks with distinct names assemble.
    std::fs::write(&b, library(&[cell("otb", 10, &[])])).unwrap();
    assemble::draw(
        &programs,
        &files,
        &plan(dir.path(), &[(&a, "blka"), (&b, "otb")]),
        dir.path(),
        &cancel,
    )
    .await
    .unwrap();
    assert!(dir.path().join("t.gds").is_file());
}

/// A block written by KLayout with its context information: KLayout would
/// rebuild its pcell from the parameters stored there, after IHP's pcell
/// library is loaded.
#[tokio::test]
async fn a_block_with_pcells_in_its_context_information_is_refused() {
    let Some((programs, files)) = setup() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let blk = dir.path().join("blk.gds");
    let run = tools::RunDir::new().unwrap();
    let script = run
        .script(
            "make.py",
            "import sys\n\
             for p in (pdk_python + '/pycell4klayout-api/source/python', pdk_python):\n    \
                 sys.path.insert(0, p)\n\
             import pya\n\
             import sg13g2_pycell_lib\n\
             ly = pya.Layout()\n\
             ly.dbu = 0.001\n\
             top = ly.create_cell('blk')\n\
             pc = ly.create_cell('bondpad', 'SG13_dev', {'shape': 'square', 'diameter': '70u'})\n\
             top.insert(pya.CellInstArray(pc.cell_index(), pya.Trans(0, 0)))\n\
             ly.write(out)\n",
        )
        .unwrap();
    let cancel = CancellationToken::new();
    let out = tools::klayout(
        &programs,
        run.path(),
        &script,
        &[
            ("pdk_python", files.python.display().to_string()),
            ("out", blk.display().to_string()),
        ],
        &cancel,
    )
    .await
    .unwrap();
    assert!(out.success(), "{}", out.combined());
    let lib = aispice_layout::gds::read(&blk).unwrap();
    assert!(
        lib.cells
            .keys()
            .any(|c| c.starts_with("$$$CONTEXT_INFO$$$")),
        "the block carries KLayout's context information"
    );
    let e = assemble::draw(
        &programs,
        &files,
        &plan(dir.path(), &[(&blk, "blk")]),
        dir.path(),
        &cancel,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("pcell or library cell"), "{e}");
    assert!(!dir.path().join("t.gds").exists());
}
