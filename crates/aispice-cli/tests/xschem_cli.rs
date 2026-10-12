//! `aispice xschem import|export` on files in a temporary folder, with an
//! empty PDK registry so the user's own is never read.

mod common;

use assert_cmd::Command;
use common::RC;
#[cfg(unix)]
use common::{fake_xschem, path_with};
use predicates::prelude::*;
use std::path::Path;

fn aispice(dir: &Path) -> Command {
    let mut c = Command::cargo_bin("aispice").unwrap();
    c.current_dir(dir)
        .env("AISPICE_PDK_REGISTRY", dir.join("pdks.toml"));
    c
}

#[test]
fn import_then_export_with_proofs() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("rc.sch"), RC).unwrap();
    aispice(dir)
        .args(["xschem", "import", "rc.sch", "--effort", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("wrote rc.asc"))
        .stdout(predicate::str::contains("Proof, aispice: same circuit"));
    assert!(dir.join("rc.asc").is_file());
    // No overwrite without --force.
    aispice(dir)
        .args(["xschem", "import", "rc.sch"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--force"));
    aispice(dir)
        .args(["xschem", "import", "rc.sch", "--force", "--effort", "1"])
        .assert()
        .success();
    let out = aispice(dir)
        .args(["--json", "xschem", "export", "rc.asc", "-o", "back.sch"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["equivalent"], serde_json::json!(true), "{v}");
    assert_eq!(
        v["proofs"]["aispice"]["equivalent"],
        serde_json::json!(true)
    );
    let sch = std::fs::read_to_string(dir.join("back.sch")).unwrap();
    assert!(sch.contains("C {devices/capa.sym}"), "{sch}");
}

#[test]
fn unreadable_input_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("bad.sch"), "C {res.sym 0 0 0 0 {}").unwrap();
    aispice(dir)
        .args(["xschem", "import", "bad.sch"])
        .assert()
        .code(2);
    assert!(!dir.join("bad.asc").exists());
    aispice(dir)
        .args(["xschem", "export", "missing.asc"])
        .assert()
        .code(2);
}

/// xschem runs Tcl from the schematic, so an import never runs it: not by
/// default, not with the environment variable an earlier version honoured,
/// and the `--xschem-check` flag that asked for it is gone. The stand-in in
/// xschem's place never leaves its mark, even on the reviewer's payload.
#[cfg(unix)]
#[test]
fn import_never_runs_xschem() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let sentinel = dir.join("xschem-ran");
    let (program, fakebin) = fake_xschem(&dir, &sentinel);
    std::fs::write(dir.join("rc.sch"), RC).unwrap();
    let run = |args: &[&str], old_switch: bool| {
        let mut c = aispice(&dir);
        c.env("AISPICE_XSCHEM", &program)
            .env("PATH", path_with(&fakebin))
            .env_remove("AISPICE_XSCHEM_CHECK_IMPORTS");
        if old_switch {
            c.env("AISPICE_XSCHEM_CHECK_IMPORTS", "1");
        }
        let out = c.args(args).output().unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    for old_switch in [false, true] {
        let (code, out) = run(
            &["xschem", "import", "rc.sch", "--force", "--effort", "1"],
            old_switch,
        );
        assert_eq!(code, Some(0), "{out}");
        assert!(out.contains("Proof, aispice: same circuit"), "{out}");
        assert!(out.contains("not checked against xschem"), "{out}");
        assert!(!sentinel.exists(), "{out}");
    }
    // The flag is no longer accepted.
    let (code, _) = run(
        &["xschem", "import", "rc.sch", "--force", "--xschem-check"],
        false,
    );
    assert_eq!(code, Some(2));
    // The reviewer's payload imports or is refused by aispice's own reader,
    // and xschem is never run on it.
    std::fs::write(
        dir.join("evil.sch"),
        format!(
            "v {{xschem version=3.4.4 file_version=1.2}}\nC {{devices/res.sym}} 0 0 0 0 {{name=R1 value=\"tcleval(1k\\}} ; exec touch {} ; #\\{{)\"}}\nC {{devices/capa.sym}} 100 0 0 0 {{name=C1 value=1n}}\nC {{devices/gnd.sym}} 0 30 0 0 {{name=l1 lab=GND}}\nC {{devices/lab_pin.sym}} 0 -30 0 0 {{name=p1 lab=a}}\n",
            sentinel.display()
        ),
    )
    .unwrap();
    let (_, out) = run(&["xschem", "import", "evil.sch", "--effort", "1"], false);
    assert!(!out.contains("Proof, xschem: same"), "{out}");
    assert!(!sentinel.exists(), "{out}");
}
