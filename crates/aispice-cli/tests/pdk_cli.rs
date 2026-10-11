//! `aispice pdk add|list|show|remove` against a temporary registry file and a
//! fake PDK folder, so the user's own registry is never touched.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;

/// The files `aispice pdk add` checks for, with nothing real in them.
fn fake_pdk(dir: &Path) -> std::path::PathBuf {
    let root = dir.join("IHP-Open-PDK/ihp-sg13g2");
    let models = root.join("libs.tech/ngspice/models");
    let osdi = root.join("libs.tech/ngspice/osdi");
    std::fs::create_dir_all(&models).unwrap();
    std::fs::create_dir_all(&osdi).unwrap();
    for (file, section) in [
        ("cornerMOSlv.lib", "mos_tt"),
        ("cornerMOShv.lib", "mos_tt"),
        ("cornerRES.lib", "res_typ"),
        ("cornerCAP.lib", "cap_typ"),
        ("cornerDIO.lib", "dio_tt"),
        ("cornerHBT.lib", "hbt_typ"),
    ] {
        std::fs::write(
            models.join(file),
            format!("* {file}\n.lib {section}\n.endl {section}\n"),
        )
        .unwrap();
    }
    std::fs::write(osdi.join("psp103.osdi"), b"").unwrap();
    root
}

fn aispice(registry: &Path) -> Command {
    let mut c = Command::cargo_bin("aispice").unwrap();
    c.env("AISPICE_PDK_REGISTRY", registry);
    c
}

#[test]
fn add_list_show_remove() {
    let dir = tempfile::tempdir().unwrap();
    let root = fake_pdk(dir.path());
    let reg = dir.path().join("config/pdks.toml");

    aispice(&reg)
        .args(["pdk", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No PDK is registered"));

    aispice(&reg)
        .args(["pdk", "add", "ihp-sg13g2"])
        .arg(dir.path().join("IHP-Open-PDK"))
        .assert()
        .success()
        .stdout(predicate::str::contains("Registered ihp-sg13g2"))
        .stdout(predicate::str::contains("pdk = \"ihp-sg13g2\""));
    let text = std::fs::read_to_string(&reg).unwrap();
    // The checkout was narrowed to its ihp-sg13g2 folder.
    assert!(text.contains("[pdks.ihp-sg13g2]"), "{text}");
    assert!(
        text.contains("IHP-Open-PDK") && text.matches("ihp-sg13g2").count() == 2,
        "{text}"
    );

    aispice(&reg)
        .args(["pdk", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ihp-sg13g2:"));
    aispice(&reg)
        .args(["pdk", "show", "ihp-sg13g2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cornerMOSlv.lib [mos]: mos_tt"))
        .stdout(predicate::str::contains("sg13g2\\sg13_lv_nmos"));
    aispice(&reg)
        .args(["--json", "pdk", "show", "ihp-sg13g2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"osdi\""));

    aispice(&reg)
        .args(["pdk", "remove", "ihp-sg13g2"])
        .assert()
        .success();
    aispice(&reg)
        .args(["pdk", "remove", "ihp-sg13g2"])
        .assert()
        .failure();
    assert!(!std::fs::read_to_string(&reg).unwrap().contains("ihp"));
    assert!(
        root.join("libs.tech").is_dir(),
        "the PDK itself is left alone"
    );
}

#[test]
fn bad_names_and_folders_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let reg = dir.path().join("pdks.toml");
    fake_pdk(dir.path());
    aispice(&reg)
        .args(["pdk", "add", "../evil"])
        .arg(dir.path().join("IHP-Open-PDK"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a usable PDK name"));
    aispice(&reg)
        .args(["pdk", "add", "ihp"])
        .arg(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("not an IHP SG13G2 PDK folder"));
    aispice(&reg)
        .args(["pdk", "add", "everything", "/"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("too broad"));
    aispice(&reg)
        .args(["pdk", "show", "ihp"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("none are registered"));
    assert!(!reg.exists());
}
