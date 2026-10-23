//! The macro package contract (`macro/v1`) shared with ChipLeap: aispice's
//! validator must report exactly what `tests/fixtures/macro/expected.json`
//! says, and the spec, schema and fixtures must not drift from ChipLeap's
//! copies.

use aispice_core::macropkg::gds::{GdsError, parse_gds};
use aispice_core::macropkg::{
    CheckOptions, Evidence, Manifest, PdkLibs, Status, check_package, parse_pdk_option,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

// The contract both repositories share. ChipLeap pins the same hashes in
// tests/test_macro.py; change both together (CONTRIBUTING.md).
const SPEC_SHA256: &str = "256a3cab336c8d629538ca2cc43023b08681335ce1bcadc2ce56497e0aaef24b";
const SCHEMA_SHA256: &str = "01fadd5205677b27ef3dcc5a00c086aaf00f2dba63d398d17fc046dd67254327";
const FIXTURES_SHA256: &str = "3170942c1f149c7e5781d5ee0571b9d86debd09ce7c19f647600481c9fc660ae";

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixtures() -> PathBuf {
    crate_dir().join("tests/fixtures/macro")
}

fn sha(path: &Path) -> String {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    Sha256::digest(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn expected() -> Value {
    let text = std::fs::read_to_string(fixtures().join("expected.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn files_under(dir: &Path, base: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(&path, base, out);
        } else {
            let rel = path.strip_prefix(base).unwrap().to_string_lossy();
            out.push(rel.replace('\\', "/"));
        }
    }
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

#[test]
fn every_case_reports_exactly_what_expected_json_says() {
    let cases = expected()["cases"].as_array().unwrap().clone();
    assert!(cases.len() >= 20);
    for case in cases {
        let package = case["package"].as_str().unwrap();
        // A case with pdk_libs gets that folder as the PDK's libraries.
        let libs = case["pdk_libs"].as_str();
        let opts = CheckOptions {
            pdk: case["pdk"].as_str().map(str::to_string),
            pdk_libs: libs.map(|l| PdkLibs::load(&fixtures().join(l)).unwrap()),
            ..Default::default()
        };
        let id = format!("{package} {:?} {libs:?}", opts.pdk);
        let report = check_package(&fixtures().join(package), &opts).unwrap();
        let json = serde_json::to_value(&report).unwrap();
        for key in ["name", "kind", "pass", "errors", "warnings"] {
            assert_eq!(json[key], case[key], "{id}: {key}");
        }
        let want = case["findings"].as_array().unwrap();
        let got = json["findings"].as_array().unwrap();
        assert_eq!(got.len(), want.len(), "{id}: {got:#?}");
        for (g, w) in got.iter().zip(want) {
            for key in ["rule", "severity", "file"] {
                assert_eq!(g[key], w[key], "{id}: {key} of {g}");
            }
            let message = g["message"].as_str().unwrap();
            match (w["message"].as_str(), w["message_prefix"].as_str()) {
                (Some(m), _) => assert_eq!(message, m, "{id}"),
                (None, Some(prefix)) => assert!(message.starts_with(prefix), "{id}: {message}"),
                _ => panic!("{id}: a finding needs message or message_prefix"),
            }
        }
    }
}

#[test]
fn every_fixture_package_has_a_case() {
    let mut cases: Vec<String> = expected()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["package"].as_str().unwrap().to_string())
        .collect();
    cases.sort();
    cases.dedup();
    let mut on_disk = vec![
        "tiny_counter.macro".to_string(),
        "tiny_ota.macro".to_string(),
    ];
    for case in std::fs::read_dir(fixtures().join("broken")).unwrap() {
        let case = case.unwrap().path();
        for pkg in std::fs::read_dir(&case).unwrap() {
            let pkg = pkg.unwrap().path();
            on_disk.push(format!(
                "broken/{}/{}",
                case.file_name().unwrap().to_string_lossy(),
                pkg.file_name().unwrap().to_string_lossy()
            ));
        }
    }
    on_disk.sort();
    assert_eq!(cases, on_disk);
}

#[test]
fn fixture_files_match_sha256sums() {
    let sums = std::fs::read_to_string(fixtures().join("SHA256SUMS")).unwrap();
    let mut listed: Vec<String> = Vec::new();
    for line in sums.lines() {
        let (digest, rel) = line.split_once("  ").unwrap();
        assert_eq!(
            sha(&fixtures().join(rel)),
            digest,
            "{rel} changed; regenerate the fixtures in ChipLeap and copy them here"
        );
        listed.push(rel.to_string());
    }
    let mut on_disk = Vec::new();
    files_under(&fixtures(), &fixtures(), &mut on_disk);
    on_disk.retain(|f| f != "SHA256SUMS");
    on_disk.sort();
    listed.sort();
    assert_eq!(listed, on_disk);
}

#[test]
fn the_shared_contract_has_not_drifted() {
    let root = crate_dir().join("../..");
    for (rel, digest) in [
        ("docs/macro-package.md", SPEC_SHA256),
        (
            "crates/aispice-core/src/macropkg/macro-v1.schema.json",
            SCHEMA_SHA256,
        ),
        (
            "crates/aispice-core/tests/fixtures/macro/SHA256SUMS",
            FIXTURES_SHA256,
        ),
    ] {
        assert_eq!(
            sha(&root.join(rel)),
            digest,
            "{rel} changed. The spec, the schema and the fixtures are shared with ChipLeap: \
             make the same change there and update the hash in both repositories."
        );
    }
}

#[test]
fn gds_bbox_through_the_hierarchy_matches_klayout() {
    let data = std::fs::read(fixtures().join("gds/hierarchy.gds")).unwrap();
    let lib = parse_gds(&data).unwrap();
    // Values from KLayout's dbbox() of the same file, as in ChipLeap's test.
    use std::f64::consts::{FRAC_1_SQRT_2, SQRT_2};
    type BBox = (f64, f64, f64, f64);
    let boxes: [(&str, BBox); 9] = [
        ("pair", (0.0, -0.2, 2.0, 1.2)),
        ("rot90", (3.8, 2.0, 5.2, 4.0)),
        ("mirror", (7.0, 4.8, 9.0, 6.2)),
        ("mirror_rot270", (-1.1, -1.9, 0.3, 0.1)),
        ("rot45", (-FRAC_1_SQRT_2, 0.0, SQRT_2, 3.0 * FRAC_1_SQRT_2)),
        ("arr", (1.0, 0.3, 8.0, 3.2)),
        ("path0", (0.0, 4.9, 4.1, 8.0)),
        ("path2", (-0.1, 4.9, 4.1, 8.1)),
        ("nested", (-5.19, -3.98, 0.2, 8.0)),
    ];
    for (name, want) in boxes {
        let got = lib.bbox(name).unwrap().unwrap();
        for (g, w) in [
            (got.0, want.0),
            (got.1, want.1),
            (got.2, want.2),
            (got.3, want.3),
        ] {
            assert!((g - w).abs() < 1e-4, "{name}: {got:?} vs {want:?}");
        }
    }
    assert_eq!(
        lib.top_cells(),
        [
            "mirror",
            "mirror_rot270",
            "rot45",
            "path0",
            "path2",
            "nested"
        ]
    );
}

#[test]
fn gds_rejects_broken_streams() {
    let good = std::fs::read(fixtures().join("tiny_ota.macro/tiny_ota.gds")).unwrap();
    let e = parse_gds(&good[..good.len() - 4]).unwrap_err().to_string();
    assert!(e.contains("truncated") || e.contains("ENDLIB"), "{e}");
    let e = parse_gds(&[0, 2, 0, 2]).unwrap_err().to_string();
    assert!(e.contains("record length"), "{e}");
    let mut short = good[..6].to_vec();
    short.extend([0, 6, 0x0F, 3, 0, 0]);
    short.extend(&good[6..]);
    let e = parse_gds(&short).unwrap_err().to_string();
    assert!(e.contains("too short"), "{e}");
    let looped = parse_gds(&std::fs::read(fixtures().join("gds/loop.gds")).unwrap());
    assert!(
        matches!(&looped, Err(GdsError::Cycle(m)) if m == r#"cell "a" contains itself: "a" -> "b" -> "a""#),
        "{looped:?}"
    );
}

#[test]
fn evidence_written_after_a_change_makes_the_package_pass_again() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("tiny_ota.macro");
    copy_dir(
        &fixtures().join("broken/stale-evidence/tiny_ota.macro"),
        &pkg,
    );
    let opts = CheckOptions::default();
    assert_eq!(
        check_package(&pkg, &opts).unwrap().rules(),
        ["evidence.stale", "evidence.stale"]
    );
    let m = Manifest::load(&pkg).unwrap();
    for (op, inputs) in [
        ("drc", vec!["tiny_ota.gds"]),
        ("lvs", vec!["tiny_ota.gds", "tiny_ota.spice"]),
    ] {
        let inputs: Vec<PathBuf> = inputs.iter().map(|f| pkg.join(f)).collect();
        let ev = Evidence::new(
            &pkg,
            op,
            Status::Pass,
            &format!("{op} rerun"),
            ("klayout", "0.30.4"),
            &m.pdk,
            &inputs,
        )
        .unwrap()
        .with_artifacts(&pkg, &[pkg.join("macro.toml")])
        .unwrap()
        .with_data(serde_json::json!({"violations": 0}));
        assert_eq!(ev.inputs[0].path, "tiny_ota.gds");
        ev.write(&pkg).unwrap();
    }
    assert!(check_package(&pkg, &opts).unwrap().findings.is_empty());
}

#[test]
fn package_level_problems() {
    let tmp = tempfile::tempdir().unwrap();
    let opts = CheckOptions::default();
    assert!(check_package(&tmp.path().join("nope"), &opts).is_err());
    assert_eq!(
        check_package(tmp.path(), &opts).unwrap().rules(),
        ["file.missing"]
    );
    let pkg = tmp.path().join("tiny_counter.macro");
    copy_dir(&fixtures().join("tiny_counter.macro"), &pkg);
    std::fs::remove_file(pkg.join("tiny_counter__ss.lib")).unwrap();
    std::fs::remove_dir_all(pkg.join("evidence")).unwrap();
    let toml = std::fs::read_to_string(pkg.join("macro.toml")).unwrap();
    std::fs::write(
        pkg.join("macro.toml"),
        toml.replace("schema = \"macro/v1\"", "schema = \"macro/v2\""),
    )
    .unwrap();
    assert_eq!(
        check_package(&pkg, &opts).unwrap().rules(),
        ["manifest.version"]
    );
    std::fs::write(pkg.join("macro.toml"), toml).unwrap();
    let report = check_package(&pkg, &opts).unwrap();
    assert_eq!(
        report.rules(),
        ["file.missing", "evidence.missing", "evidence.missing"]
    );
    assert_eq!(report.findings[0].file, "tiny_counter__ss.lib");
    assert!(parse_pdk_option("ihp@XYZ").is_err());
    assert_eq!(
        parse_pdk_option("ihp-sg13g2@22f2a25").unwrap(),
        ("ihp-sg13g2".to_string(), Some("22f2a25".to_string()))
    );
    let bad = CheckOptions {
        pdk: Some("bad name".into()),
        ..Default::default()
    };
    assert!(check_package(&pkg, &bad).is_err());
}

#[test]
fn text_report_matches_chipleap() {
    let pkg = fixtures().join("broken/wrong-size/tiny_counter.macro");
    let report = check_package(&pkg, &CheckOptions::default()).unwrap();
    let text = report.to_text();
    let mut lines = text.lines();
    assert_eq!(
        lines.next().unwrap(),
        "error   lef.size tiny_counter.lef: SIZE is 20 x 10 um, macro.toml says 30 x 10 um"
    );
    assert!(
        lines
            .next()
            .unwrap()
            .ends_with(": fail, 1 error, 0 warnings")
    );
}
