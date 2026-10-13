//! macro/v1's validator against packages built to get a wrong answer from
//! it: files reached through a symbolic link, files that are not regular, and
//! cells named like the PDK's own. ChipLeap runs the same cases in
//! tests/test_macro_security.py and tests/test_macro.py.

use aispice_core::macropkg::{
    CheckOptions, Evidence, Manifest, Report, Status, check_package, read_package_file,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/macro")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let dest = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_dir(&path, &dest);
        } else {
            std::fs::copy(&path, &dest).unwrap();
        }
    }
}

fn check(pkg: &Path) -> Report {
    check_package(pkg, &CheckOptions::default()).unwrap()
}

fn found(report: &Report, rule: &str) -> Vec<(String, String)> {
    report
        .findings
        .iter()
        .filter(|f| f.rule == rule)
        .map(|f| (f.file.clone(), f.message.clone()))
        .collect()
}

fn pair(file: &str, message: &str) -> (String, String) {
    (file.into(), message.into())
}

// Files reached through a link

#[cfg(unix)]
fn link(target: &Path, at: &Path) -> bool {
    std::os::unix::fs::symlink(target, at).unwrap();
    true
}

/// Creating a link on Windows needs a privilege (or developer mode); without
/// it the test has nothing to show.
#[cfg(windows)]
fn link(target: &Path, at: &Path) -> bool {
    let dir = at
        .parent()
        .map(|p| p.join(target))
        .is_some_and(|p| p.is_dir());
    let made = if dir {
        std::os::windows::fs::symlink_dir(target, at)
    } else {
        std::os::windows::fs::symlink_file(target, at)
    };
    if let Err(e) = &made {
        eprintln!("skipped: cannot create a symbolic link here: {e}");
    }
    made.is_ok()
}

/// A valid package beside a copy of it whose `linked` entries are symbolic
/// links into the valid one, its sibling. None when links cannot be made.
#[cfg(any(unix, windows))]
fn sibling_pair(tmp: &Path, linked: &[&str]) -> Option<(PathBuf, PathBuf)> {
    let sibling = tmp.join("pkgs/other.macro");
    copy_dir(&fixtures().join("tiny_counter.macro"), &sibling);
    let pkg = tmp.join("pkgs/tiny_counter.macro");
    std::fs::create_dir(&pkg).unwrap();
    for entry in std::fs::read_dir(&sibling).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap();
        if linked.contains(&name.to_str().unwrap()) {
            let target = Path::new("..").join("other.macro").join(name);
            if !link(&target, &pkg.join(name)) {
                return None;
            }
        } else if path.is_dir() {
            copy_dir(&path, &pkg.join(name));
        } else {
            std::fs::copy(&path, pkg.join(name)).unwrap();
        }
    }
    Some((sibling, pkg))
}

#[cfg(any(unix, windows))]
#[test]
fn files_in_a_sibling_directory_are_not_read_through_a_link() {
    let tmp = tempfile::tempdir().unwrap();
    let Some((sibling, pkg)) = sibling_pair(tmp.path(), &["evidence", "tiny_counter.gds"]) else {
        return;
    };
    // Unchecked, the links reach the sibling's files, and those pass every check.
    assert_eq!(
        std::fs::read(pkg.join("evidence/drc.json")).unwrap(),
        std::fs::read(sibling.join("evidence/drc.json")).unwrap()
    );
    assert!(check(&sibling).passed);
    let report = check(&pkg);
    assert!(!report.passed);
    assert_eq!(
        found(&report, "file.unreadable"),
        [
            pair(
                "tiny_counter.gds",
                "cannot read the file: it is a symbolic link"
            ),
            pair("evidence", "cannot read the folder: it is a symbolic link"),
        ]
    );
    assert_eq!(
        found(&report, "evidence.missing"),
        [
            pair("evidence", "no summary for drc"),
            pair("evidence", "no summary for lvs"),
        ]
    );
}

#[cfg(any(unix, windows))]
#[test]
fn a_summary_that_links_elsewhere_is_not_read() {
    let tmp = tempfile::tempdir().unwrap();
    let Some((sibling, pkg)) = sibling_pair(tmp.path(), &[]) else {
        return;
    };
    std::fs::remove_file(pkg.join("evidence/drc.json")).unwrap();
    if !link(
        &sibling.join("evidence/drc.json"),
        &pkg.join("evidence/drc.json"),
    ) {
        return;
    }
    let report = check(&pkg);
    assert_eq!(
        found(&report, "file.unreadable"),
        [pair(
            "evidence/drc.json",
            "cannot read the file: it is a symbolic link"
        )]
    );
    assert_eq!(
        found(&report, "evidence.missing"),
        [pair("evidence", "no summary for drc")]
    );
}

/// A summary naming a package file that is a link to a file elsewhere:
/// hashing through it would read that file, and the stale-evidence message
/// would print the start of its SHA-256.
#[cfg(any(unix, windows))]
#[test]
fn an_evidence_input_that_links_out_of_the_package_is_not_hashed() {
    let tmp = tempfile::tempdir().unwrap();
    let Some((_, pkg)) = sibling_pair(tmp.path(), &[]) else {
        return;
    };
    let secret = tmp.path().join("secret.txt");
    std::fs::write(&secret, "not the package's\n").unwrap();
    if !link(&secret, &pkg.join("notes.txt")) {
        return;
    }
    let drc = pkg.join("evidence/drc.json");
    let mut summary: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&drc).unwrap()).unwrap();
    summary["inputs"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"path": "notes.txt", "sha256": "0".repeat(64)}));
    std::fs::write(&drc, summary.to_string()).unwrap();
    // What the link reaches, unchecked.
    assert_eq!(
        std::fs::read_to_string(pkg.join("notes.txt")).unwrap(),
        "not the package's\n"
    );
    let report = check(&pkg);
    assert_eq!(
        found(&report, "file.unreadable"),
        [pair(
            "notes.txt",
            "cannot read the file: it is a symbolic link"
        )]
    );
    assert_eq!(found(&report, "evidence.stale"), []);
}

#[cfg(unix)]
#[test]
fn a_fifo_does_not_stall_the_check() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("tiny_counter.macro");
    copy_dir(&fixtures().join("tiny_counter.macro"), &pkg);
    let v = pkg.join("tiny_counter.v");
    std::fs::remove_file(&v).unwrap();
    let made = std::process::Command::new("mkfifo").arg(&v).status();
    if !made.is_ok_and(|s| s.success()) {
        eprintln!("skipped: mkfifo is not available");
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let dir = pkg.clone();
    std::thread::spawn(move || tx.send(check(&dir)).unwrap());
    let report = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the check blocked on a FIFO");
    assert_eq!(
        found(&report, "file.unreadable"),
        [pair(
            "tiny_counter.v",
            "cannot read the file: it is not a regular file"
        )]
    );
}

/// The directory given to the validator may itself be a link; only what is
/// inside the package is read without following links.
#[cfg(any(unix, windows))]
#[test]
fn a_linked_package_directory_is_still_checked() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real/tiny_counter.macro");
    copy_dir(&fixtures().join("tiny_counter.macro"), &real);
    let at = tmp.path().join("tiny_counter.macro");
    if !link(&real, &at) {
        return;
    }
    let report = check(&at);
    assert!(report.passed, "{}", report.to_text());
    assert_eq!(
        read_package_file(&at, "evidence/drc.json", 1 << 20).unwrap(),
        std::fs::read(real.join("evidence/drc.json")).unwrap()
    );
}

#[test]
fn a_package_file_is_read_only_inside_the_package() {
    let pkg = fixtures().join("tiny_counter.macro");
    for rel in ["", "evidence", "missing.v", "evidence/missing.json"] {
        assert!(read_package_file(&pkg, rel, 1 << 20).is_err(), "{rel:?}");
    }
    assert!(read_package_file(&pkg, "macro.toml", 1 << 20).is_ok());
}

// pdk.cell_name: cells named like the PDK's own

/// `data` with every GDS structure name (STRNAME, SNAME) mapped by `rename`.
fn rename_gds_cells(data: &[u8], rename: impl Fn(&str) -> String) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let len = usize::from(u16::from_be_bytes([data[pos], data[pos + 1]]));
        let record = &data[pos..pos + len];
        if matches!(record[2], 0x06 | 0x12) {
            let old = String::from_utf8_lossy(&record[4..]);
            let mut name = rename(old.trim_end_matches('\0')).into_bytes();
            if name.len() % 2 == 1 {
                name.push(0);
            }
            out.extend_from_slice(&u16::try_from(name.len() + 4).unwrap().to_be_bytes());
            out.extend_from_slice(&record[2..4]);
            out.extend_from_slice(&name);
        } else {
            out.extend_from_slice(record);
        }
        pos += len;
    }
    out
}

/// tiny_counter renamed to `name`, its tap cell named `tap`, valid otherwise,
/// as ChipLeap's tests make it.
fn counter_named(tmp: &Path, name: &str, tap: Option<&str>) -> PathBuf {
    let from = fixtures().join("tiny_counter.macro");
    let pkg = tmp.join(format!("{name}.macro"));
    std::fs::create_dir_all(&pkg).unwrap();
    for entry in std::fs::read_dir(&from).unwrap() {
        let path = entry.unwrap().path();
        let file = path.file_name().unwrap().to_str().unwrap();
        if path.is_dir() || file.ends_with(".gds") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            pkg.join(file.replace("tiny_counter", name)),
            text.replace("tiny_counter", name),
        )
        .unwrap();
    }
    let tap = tap.map_or_else(|| format!("{name}_tap"), str::to_string);
    let gds = std::fs::read(from.join("tiny_counter.gds")).unwrap();
    let gds = rename_gds_cells(&gds, |c| match c {
        "tiny_counter" => name.to_string(),
        "tiny_counter_tap" => tap.clone(),
        other => other.to_string(),
    });
    std::fs::write(pkg.join(format!("{name}.gds")), gds).unwrap();
    let m = Manifest::load(&pkg).unwrap();
    for (op, inputs) in [("drc", vec!["gds"]), ("lvs", vec!["gds", "spice"])] {
        let inputs: Vec<PathBuf> = inputs.iter().map(|e| pkg.join(m.file(e))).collect();
        let summary = format!("Synthetic test fixture: no {op} was run.");
        Evidence::new(
            &pkg,
            op,
            Status::Pass,
            &summary,
            ("synthetic", "1"),
            &m.pdk,
            &inputs,
        )
        .unwrap()
        .write(&pkg)
        .unwrap();
    }
    pkg
}

fn with_cells(cells: &[&str]) -> CheckOptions {
    CheckOptions {
        pdk_cells: cells.iter().map(|c| c.to_string()).collect::<BTreeSet<_>>(),
        ..Default::default()
    }
}

#[test]
fn a_renamed_counter_passes() {
    let tmp = tempfile::tempdir().unwrap();
    let report = check(&counter_named(tmp.path(), "counter", None));
    assert!(report.findings.is_empty(), "{}", report.to_text());
}

#[test]
fn a_package_named_in_the_pdks_name_space_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let report = check(&counter_named(tmp.path(), "sg13g2_counter", None));
    let rules: Vec<(&str, &str)> = report
        .findings
        .iter()
        .map(|f| (f.rule.as_str(), f.file.as_str()))
        .collect();
    // The second is its tap cell, sg13g2_counter_tap.
    assert_eq!(
        rules,
        [
            ("pdk.cell_name", "macro.toml"),
            ("pdk.cell_name", "sg13g2_counter.gds")
        ]
    );
    assert_eq!(
        report.findings[0].message,
        r#"name "sg13g2_counter" begins with "sg13g2", as the cells of PDK "ihp-sg13g2" do"#
    );
}

#[test]
fn case_does_not_hide_a_pdk_cell_name() {
    let tmp = tempfile::tempdir().unwrap();
    let report = check(&counter_named(
        tmp.path(),
        "counter",
        Some("RM_IHPSG13_tap"),
    ));
    let messages: Vec<&str> = report.findings.iter().map(|f| f.message.as_str()).collect();
    assert_eq!(
        messages,
        [
            r#"GDS cell "RM_IHPSG13_tap" begins with "rm_ihpsg13", as the cells of PDK "ihp-sg13g2" do"#
        ]
    );
}

#[test]
fn a_cell_of_the_pdks_libraries_is_refused_when_the_validator_has_them() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = counter_named(tmp.path(), "counter", Some("nmos"));
    assert!(check(&pkg).passed);
    let report = check_package(&pkg, &with_cells(&["nmos", "pmos"])).unwrap();
    let messages: Vec<&str> = report.findings.iter().map(|f| f.message.as_str()).collect();
    assert_eq!(
        messages,
        [r#"GDS cell "nmos" is a cell of PDK "ihp-sg13g2""#]
    );
    assert_eq!(
        check_package(&pkg, &with_cells(&["NMOS"])).unwrap().errors,
        1
    );
    // The package's own name and its subcircuits are compared too.
    let report = check_package(&pkg, &with_cells(&["Counter", "nmos"])).unwrap();
    let rows: Vec<(&str, &str)> = report
        .findings
        .iter()
        .map(|f| (f.file.as_str(), f.message.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            (
                "macro.toml",
                r#"name "counter" is a cell of PDK "ihp-sg13g2""#
            ),
            (
                "counter.gds",
                r#"GDS cell "nmos" is a cell of PDK "ihp-sg13g2""#
            ),
        ]
    );
}

#[test]
fn other_pdks_have_no_name_space_of_ihps() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = counter_named(tmp.path(), "sg13g2_counter", None);
    let toml = std::fs::read_to_string(pkg.join("macro.toml")).unwrap();
    std::fs::write(
        pkg.join("macro.toml"),
        toml.replace("name = \"ihp-sg13g2\"", "name = \"other-pdk\""),
    )
    .unwrap();
    let report = check(&pkg);
    assert!(
        !report.rules().contains(&"pdk.cell_name"),
        "{}",
        report.to_text()
    );
}

/// The rule runs after every other, with or without `--pdk`.
#[test]
fn pdk_cell_names_come_last() {
    let pkg = fixtures().join("broken/pdk-cell-name/tiny_counter.macro");
    let opts = CheckOptions {
        pdk: Some("ihp-sg13g2@0000000".into()),
        ..Default::default()
    };
    let report = check_package(&pkg, &opts).unwrap();
    assert_eq!(
        report.rules(),
        ["pdk.expected", "pdk.cell_name", "pdk.cell_name"]
    );
}
