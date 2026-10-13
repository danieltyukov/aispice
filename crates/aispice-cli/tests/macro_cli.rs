//! `aispice macro check` with and without the PDK's libraries, as
//! ChipLeap's test_the_cli_reads_the_pdks_libraries runs `chipleap macro
//! check`.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn counter() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../aispice-core/tests/fixtures/macro/tiny_counter.macro")
}

#[test]
fn macro_check_reads_the_pdks_libraries() {
    let dir = tempfile::tempdir().unwrap();
    // A library whose cell is named like the counter's tap cell.
    let pdk = dir.path().join("ihp-sg13g2");
    let lef = pdk.join("libs.ref/sg13g2_pr/lef");
    std::fs::create_dir_all(&lef).unwrap();
    std::fs::write(
        lef.join("sg13g2_pr.lef"),
        "MACRO tiny_counter_tap\nEND tiny_counter_tap\n",
    )
    .unwrap();
    Command::cargo_bin("aispice")
        .unwrap()
        .args(["macro", "check"])
        .arg(counter())
        .assert()
        .code(0);
    Command::cargo_bin("aispice")
        .unwrap()
        .args(["macro", "check"])
        .arg(counter())
        .arg("--pdk-libs")
        .arg(&pdk)
        .assert()
        .code(1)
        .stdout(predicate::str::contains(
            r#"pdk.cell_name tiny_counter.gds: GDS cell "tiny_counter_tap" is a cell of PDK "ihp-sg13g2""#,
        ));
    Command::cargo_bin("aispice")
        .unwrap()
        .args(["macro", "check"])
        .arg(counter())
        .arg("--pdk-libs")
        .arg(dir.path().join("none"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--pdk-libs"));
}
