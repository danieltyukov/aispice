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

/// xschem runs Tcl from the schematic, so an import runs it only when the
/// user asks, and only inside the sandbox, so the stand-in xschem never
/// leaves its mark: it is blocked by the sandbox, or no sandbox starts and
/// it is not run.
#[cfg(unix)]
#[test]
fn import_runs_xschem_only_when_asked_and_only_in_a_sandbox() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let sentinel = dir.join("xschem-ran");
    let (program, fakebin) = fake_xschem(&dir, &sentinel);
    std::fs::write(dir.join("rc.sch"), RC).unwrap();
    let run = |args: &[&str], ask_env: bool| {
        let mut c = aispice(&dir);
        c.env("AISPICE_XSCHEM", &program)
            .env("PATH", path_with(&fakebin))
            .env_remove("AISPICE_XSCHEM_CHECK_IMPORTS");
        if ask_env {
            c.env("AISPICE_XSCHEM_CHECK_IMPORTS", "1");
        }
        let out = c.args(args).output().unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    // Not asked: aispice's own proof only.
    let (code, out) = run(&["xschem", "import", "rc.sch", "--effort", "1"], false);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("Proof, aispice: same circuit"), "{out}");
    assert!(out.contains("not checked against xschem"), "{out}");
    assert!(!sentinel.exists());
    // Asked, by flag or by environment: xschem is run only sandboxed, so
    // the stand-in's mark is never left (it is blocked where a sandbox
    // starts, or xschem is not run where none does). Either way the import
    // succeeds and no longer reports that xschem did not check it.
    for (args, env) in [
        (
            &[
                "xschem",
                "import",
                "rc.sch",
                "--force",
                "--effort",
                "1",
                "--xschem-check",
            ][..],
            false,
        ),
        (
            &["xschem", "import", "rc.sch", "--force", "--effort", "1"][..],
            true,
        ),
    ] {
        let (code, out) = run(args, env);
        assert_eq!(code, Some(0), "{out}");
        assert!(!out.contains("not checked against xschem"), "{out}");
        assert!(!sentinel.exists(), "{out}");
    }
    // The reviewer's payload: refused by the scan before any sandbox.
    std::fs::write(
        dir.join("evil.sch"),
        format!(
            "v {{xschem version=3.4.4 file_version=1.2}}\nC {{devices/res.sym}} 0 0 0 0 {{name=R1 value=\"tcleval(1k\\}} ; exec touch {} ; #\\{{)\"}}\nC {{devices/capa.sym}} 100 0 0 0 {{name=C1 value=1n}}\nC {{devices/gnd.sym}} 0 30 0 0 {{name=l1 lab=GND}}\nC {{devices/lab_pin.sym}} 0 -30 0 0 {{name=p1 lab=a}}\n",
            sentinel.display()
        ),
    )
    .unwrap();
    let (_, out) = run(
        &[
            "xschem",
            "import",
            "evil.sch",
            "--effort",
            "1",
            "--xschem-check",
        ],
        false,
    );
    assert!(out.contains("xschem was not run"), "{out}");
    assert!(!sentinel.exists(), "{out}");
}
