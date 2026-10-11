//! `aispice xschem import|export` on files in a temporary folder, with an
//! empty PDK registry so the user's own is never read.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;

const RC: &str = r#"v {xschem version=3.4.4 file_version=1.2}
N 100 -200 200 -200 {lab=in}
N 260 -200 300 -200 {lab=out}
N 300 -200 300 -160 {lab=out}
N 100 -200 100 -170 {lab=in}
C {devices/vsource.sym} 100 -140 0 0 {name=V1 value="dc 0 ac 1"}
C {devices/res.sym} 230 -200 1 0 {name=R1 value=1k}
C {devices/capa.sym} 300 -130 0 0 {name=C1 value=1n}
C {devices/gnd.sym} 100 -110 0 0 {name=l1 lab=GND}
C {devices/gnd.sym} 300 -100 0 0 {name=l2 lab=GND}
C {devices/lab_pin.sym} 300 -200 0 0 {name=p1 lab=out}
C {devices/lab_pin.sym} 100 -200 0 0 {name=p2 lab=in}
C {devices/code_shown.sym} 400 -200 0 0 {name=s1 only_toplevel=false value=".ac dec 10 1 1meg"}
"#;

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
