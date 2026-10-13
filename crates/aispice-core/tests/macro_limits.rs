//! Macro packages from someone else: every limit ends in a finding, quickly,
//! and no input makes a reader panic.
//!
//! ChipLeap runs the same cases in tests/test_macro_limits.py. Files built to
//! be large are made here, never committed; most limits are lowered so small
//! files reach them.

use aispice_core::macropkg::gds::{GdsError, parse_gds, parse_gds_with};
use aispice_core::macropkg::lef::{LefError, parse_lef, parse_lef_with};
use aispice_core::macropkg::liberty::{LibertyError, parse_liberty, parse_liberty_with};
use aispice_core::macropkg::spice::{spice_subckt, spice_subckt_with};
use aispice_core::macropkg::verilog::{VerilogError, verilog_ports, verilog_ports_with};
use aispice_core::macropkg::{CheckOptions, LimitError, Limits, Manifest, Report, check_package};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SMALL: Limits = Limits {
    tokens: 50,
    pins: 3,
    nesting: 4,
    ..Limits::DEFAULT
};

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

fn package(tmp: &Path, name: &str) -> PathBuf {
    let pkg = tmp.join(name);
    copy_dir(&fixtures().join(name), &pkg);
    pkg
}

fn check(pkg: &Path, limits: Limits) -> Report {
    let opts = CheckOptions {
        limits,
        ..Default::default()
    };
    check_package(pkg, &opts).unwrap()
}

fn findings(r: &Report) -> Vec<(String, String, String)> {
    r.findings
        .iter()
        .map(|f| (f.rule.clone(), f.file.clone(), f.message.clone()))
        .collect()
}

fn row(rule: &str, file: &str, message: &str) -> (String, String, String) {
    (rule.into(), file.into(), message.into())
}

fn limit_message<T: std::fmt::Debug>(r: Result<T, impl Into<Anything>>) -> String {
    match r.map_err(Into::into) {
        Err(Anything::Limit(m)) => m,
        Err(Anything::Other(m)) => panic!("expected a limit, got {m}"),
        Ok(v) => panic!("expected a limit, got {v:?}"),
    }
}

/// The readers' errors, to tell a limit from a syntax error in one place.
enum Anything {
    Limit(String),
    Other(String),
}

impl From<LimitError> for Anything {
    fn from(e: LimitError) -> Self {
        Anything::Limit(e.0)
    }
}

impl From<LefError> for Anything {
    fn from(e: LefError) -> Self {
        match e {
            LefError::Limit(e) => Anything::Limit(e.0),
            LefError::Syntax(m) => Anything::Other(m),
        }
    }
}

impl From<VerilogError> for Anything {
    fn from(e: VerilogError) -> Self {
        match e {
            VerilogError::Limit(e) => Anything::Limit(e.0),
            VerilogError::Duplicate(m) => Anything::Other(m),
        }
    }
}

impl From<LibertyError> for Anything {
    fn from(e: LibertyError) -> Self {
        match e {
            LibertyError::Limit(e) => Anything::Limit(e.0),
            LibertyError::Syntax(m) => Anything::Other(m),
        }
    }
}

impl From<GdsError> for Anything {
    fn from(e: GdsError) -> Self {
        match e {
            GdsError::Limit(e) => Anything::Limit(e.0),
            other => Anything::Other(other.to_string()),
        }
    }
}

// A GDSII writer with the records these tests need, as in ChipLeap's
// tests/fixtures/macro/generate.py.

fn rec(rtype: u8, dtype: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = u16::try_from(4 + payload.len())
        .unwrap()
        .to_be_bytes()
        .to_vec();
    out.extend([rtype, dtype]);
    out.extend(payload);
    out
}

fn i32s(v: &[i32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_be_bytes()).collect()
}

fn text(s: &str) -> Vec<u8> {
    let mut b = s.as_bytes().to_vec();
    if b.len() % 2 == 1 {
        b.push(0);
    }
    b
}

fn real8(x: f64) -> [u8; 8] {
    if x == 0.0 {
        return [0; 8];
    }
    let sign = if x < 0.0 { 0x80u8 } else { 0 };
    let (mut x, mut exp) = (x.abs(), 64i32);
    while x >= 1.0 {
        x /= 16.0;
        exp += 1;
    }
    while x < 1.0 / 16.0 {
        x *= 16.0;
        exp -= 1;
    }
    let mut mant = (x * 2f64.powi(56)).round() as u64;
    if mant >= 1 << 56 {
        mant /= 16;
        exp += 1;
    }
    let mut out = [0u8; 8];
    out[0] = sign | u8::try_from(exp).unwrap();
    out[1..].copy_from_slice(&mant.to_be_bytes()[1..]);
    out
}

struct Cell {
    name: String,
    body: Vec<u8>,
}

impl Cell {
    fn new(name: &str) -> Cell {
        Cell {
            name: name.into(),
            body: Vec::new(),
        }
    }

    fn rect(mut self, x0: i32, y0: i32, x1: i32, y1: i32) -> Cell {
        self.body.extend(rec(0x08, 0, &[]));
        self.body.extend(rec(0x0D, 2, &8i16.to_be_bytes()));
        self.body.extend(rec(0x0E, 2, &0i16.to_be_bytes()));
        self.body.extend(rec(
            0x10,
            3,
            &i32s(&[x0, y0, x1, y0, x1, y1, x0, y1, x0, y0]),
        ));
        self.body.extend(rec(0x11, 0, &[]));
        self
    }

    fn sref(mut self, child: &str, x: i32, y: i32, angle: f64, mag: f64) -> Cell {
        self.body.extend(rec(0x0A, 0, &[]));
        self.body.extend(rec(0x12, 6, &text(child)));
        if angle != 0.0 || mag != 1.0 {
            self.body.extend(rec(0x1A, 1, &0u16.to_be_bytes()));
            if mag != 1.0 {
                self.body.extend(rec(0x1B, 5, &real8(mag)));
            }
            if angle != 0.0 {
                self.body.extend(rec(0x1C, 5, &real8(angle)));
            }
        }
        self.body.extend(rec(0x10, 3, &i32s(&[x, y])));
        self.body.extend(rec(0x11, 0, &[]));
        self
    }

    fn at(self, child: &str, x: i32, y: i32) -> Cell {
        self.sref(child, x, y, 0.0, 1.0)
    }

    fn aref(mut self, child: &str, cols: i16, rows: i16, dx: i32, dy: i32) -> Cell {
        let (c, r) = (i32::from(cols), i32::from(rows));
        self.body.extend(rec(0x0B, 0, &[]));
        self.body.extend(rec(0x12, 6, &text(child)));
        let mut colrow = cols.to_be_bytes().to_vec();
        colrow.extend(rows.to_be_bytes());
        self.body.extend(rec(0x13, 2, &colrow));
        self.body
            .extend(rec(0x10, 3, &i32s(&[0, 0, c * dx, 0, 0, r * dy])));
        self.body.extend(rec(0x11, 0, &[]));
        self
    }
}

fn gds(cells: Vec<Cell>) -> Vec<u8> {
    let mut out = rec(0x00, 2, &600i16.to_be_bytes());
    out.extend(rec(0x01, 2, &[0; 24]));
    out.extend(rec(0x02, 6, &text("t")));
    let mut units = real8(1e-3).to_vec();
    units.extend(real8(1e-9));
    out.extend(rec(0x03, 5, &units));
    for c in cells {
        out.extend(rec(0x05, 2, &[0; 24]));
        out.extend(rec(0x06, 6, &text(&c.name)));
        out.extend(c.body);
        out.extend(rec(0x07, 0, &[]));
    }
    out.extend(rec(0x04, 0, &[]));
    out
}

fn close(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> bool {
    [(a.0, b.0), (a.1, b.1), (a.2, b.2), (a.3, b.3)]
        .iter()
        .all(|(x, y)| (x - y).abs() < 1e-9)
}

// GDS hierarchies

#[test]
fn gds_cycles_are_errors() {
    let one = gds(vec![
        Cell::new("top").at("a", 0, 0),
        Cell::new("a").at("a", 0, 0),
    ]);
    let two = gds(vec![
        Cell::new("top").at("a", 0, 0),
        Cell::new("a").at("b", 0, 0),
        Cell::new("b").at("a", 0, 0),
    ]);
    for (data, want) in [
        (one, r#"cell "a" contains itself: "a" -> "a""#),
        (two, r#"cell "a" contains itself: "a" -> "b" -> "a""#),
    ] {
        match parse_gds(&data) {
            Err(GdsError::Cycle(m)) => assert_eq!(m, want),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn gds_huge_array_is_measured_from_its_corners() {
    let start = Instant::now();
    let lib = parse_gds(&gds(vec![
        Cell::new("top").aref("u", 32767, 32767, 1, 1),
        Cell::new("u").rect(0, 0, 10, 10),
    ]))
    .unwrap();
    let b = lib.bbox("top").unwrap().unwrap();
    assert!(close(b, (0.0, 0.0, 32.776, 32.776)), "{b:?}");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn gds_shared_cells_are_measured_once() {
    // 40 levels, each placing the next twice: 2**40 paths, 41 cells.
    for angle in [0.0, 45.0] {
        let mut cells: Vec<Cell> = (0..40)
            .map(|i| {
                let next = format!("c{}", i + 1);
                Cell::new(&format!("c{i}"))
                    .sref(&next, 0, 0, angle, 1.0)
                    .at(&next, 1, 1)
            })
            .collect();
        cells.push(Cell::new("c40").rect(0, 0, 1, 1));
        let lib = parse_gds(&gds(cells)).unwrap();
        let start = Instant::now();
        assert!(lib.bbox("c0").unwrap().is_some());
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}

#[test]
fn gds_reference_depth() {
    let chain = |n: usize| {
        let mut cells: Vec<Cell> = (0..n)
            .map(|i| Cell::new(&format!("d{i}")).at(&format!("d{}", i + 1), 0, 0))
            .collect();
        cells.push(Cell::new(&format!("d{n}")).rect(0, 0, 1, 1));
        gds(cells)
    };
    let b = parse_gds(&chain(64)).unwrap().bbox("d0").unwrap().unwrap();
    assert!(close(b, (0.0, 0.0, 0.001, 0.001)), "{b:?}");
    assert_eq!(
        limit_message(parse_gds(&chain(65))),
        r#"references nest more than 64 deep below cell "d0""#
    );
}

#[test]
fn gds_overflowing_magnification() {
    let mut cells: Vec<Cell> = (0..6)
        .map(|i| Cell::new(&format!("m{i}")).sref(&format!("m{}", i + 1), 1, 1, 0.0, 1e70))
        .collect();
    cells.push(Cell::new("m6").rect(0, 0, 1, 1));
    let lib = parse_gds(&gds(cells)).unwrap();
    assert_eq!(
        limit_message(lib.bbox("m0")),
        "a reference's magnification takes coordinates out of range"
    );
}

#[test]
fn gds_counts() {
    let many = || {
        gds((0..11)
            .map(|i| Cell::new(&format!("c{i}")).rect(0, 0, 1, 1))
            .collect())
    };
    for (limits, want) in [
        (
            Limits {
                gds_records: 10,
                ..Limits::DEFAULT
            },
            "the GDS has more than 10 records",
        ),
        (
            Limits {
                gds_elements: 10,
                ..Limits::DEFAULT
            },
            "the GDS has more than 10 elements",
        ),
        (
            Limits {
                gds_cells: 10,
                ..Limits::DEFAULT
            },
            "the GDS has more than 10 cells",
        ),
    ] {
        assert_eq!(limit_message(parse_gds_with(&many(), &limits)), want);
    }
    let rotated = gds(vec![
        Cell::new("top").sref("u", 0, 0, 45.0, 1.0),
        Cell::new("u").rect(0, 0, 1, 1),
    ]);
    let few = Limits {
        gds_points: 4,
        ..Limits::DEFAULT
    };
    let lib = parse_gds_with(&rotated, &few).unwrap();
    assert_eq!(
        limit_message(lib.bbox("top")),
        "references at angles other than quarter turns need more than 4 points to measure"
    );
}

// Text views

#[test]
fn truncated_views_are_errors() {
    for text in [
        "MACRO m\n PIN a\n DIRECTION INPUT",
        "MACRO m\n OBS\n LAYER M1",
    ] {
        let e = parse_lef(text).unwrap_err().to_string();
        assert!(e.contains("end of file"), "{e}");
    }
    let e = parse_liberty("library (l) { cell (m) { pin (a) { direction : input ;");
    assert!(
        e.unwrap_err()
            .to_string()
            .contains("end of file inside a group")
    );
    let e = parse_liberty("library (l) { cell (m) { pin (a) { direction : \"input");
    assert!(e.unwrap_err().to_string().contains("unterminated string"));
    let ports = verilog_ports("module m (input a, output", "m")
        .unwrap()
        .unwrap();
    assert_eq!(ports.len(), 1);
    assert_eq!(ports[0].name, "a");
    let sub = spice_subckt(".subckt m a b\n+ c", "m").unwrap().unwrap();
    assert_eq!(sub.ports, ["a", "b", "c"]);
}

#[test]
fn view_limits() {
    let many = vec!["x"; 51].join(" ");
    assert_eq!(
        limit_message(parse_lef_with(&many, &SMALL)),
        "the LEF has more than 50 tokens"
    );
    let pins: String = (0..4).map(|i| format!("PIN a{i}\nEND a{i}\n")).collect();
    let pins = format!("MACRO m\n{pins}END m\n");
    assert_eq!(
        limit_message(parse_lef_with(&pins, &SMALL)),
        r#"MACRO "m" has more than 3 pins"#
    );
    assert_eq!(
        limit_message(parse_liberty_with(&";".repeat(51), &SMALL, None)),
        "the liberty file has more than 50 tokens"
    );
    let deep = format!("library (l) {{{}{}", "g () {".repeat(4), "}".repeat(5));
    assert_eq!(
        limit_message(parse_liberty_with(&deep, &SMALL, None)),
        "groups nest more than 4 deep"
    );
    let cell = "library (l) { cell (m) { pin (a, b, c, d) { } } }";
    assert_eq!(
        limit_message(parse_liberty_with(cell, &SMALL, None)),
        r#"cell "m" has more than 3 pins"#
    );
    let huge_bus = "library (l) { type (t) { bit_from : 0 ; bit_to : 4611686018427387904 ; } \
                    cell (m) { bus (d) { bus_type : t ; } } }";
    assert_eq!(
        limit_message(parse_liberty(huge_bus)),
        r#"cell "m" has more than 10000 pins"#
    );
    // Only the macro's cell is read, so another cell's pins cost nothing.
    assert!(
        parse_liberty_with(huge_bus, &Limits::DEFAULT, Some("other"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        limit_message(verilog_ports_with(&many, "m", &SMALL)),
        "the Verilog file has more than 50 tokens"
    );
    assert_eq!(
        limit_message(verilog_ports_with(
            "module m (a, b, c, d); endmodule",
            "m",
            &SMALL
        )),
        r#"module "m" has more than 3 ports"#
    );
    assert_eq!(
        limit_message(spice_subckt_with(&"\n".repeat(50), "m", &SMALL)),
        "the SPICE file has more than 50 lines"
    );
    assert_eq!(
        limit_message(spice_subckt_with(".subckt m a b c d\n.ends\n", "m", &SMALL)),
        r#".subckt "m" has more than 3 ports"#
    );
}

#[test]
fn views_stay_linear() {
    let start = Instant::now();
    let text = format!(
        "module m (a);\n{}endmodule",
        "input [3:0 x ;\n".repeat(200_000)
    );
    assert_eq!(verilog_ports(&text, "m").unwrap().unwrap().len(), 1);
    let text = format!(".subckt m a\n{}.ends\n", "+ b\n".repeat(1_000_000));
    let wide = Limits {
        pins: 10_000_000,
        ..Limits::DEFAULT
    };
    let sub = spice_subckt_with(&text, "m", &wide).unwrap().unwrap();
    assert_eq!(sub.ports.len(), 1_000_001);
    assert!(start.elapsed() < Duration::from_secs(30));
}

// Whole packages

#[test]
fn oversized_files_are_not_read() {
    let d = Limits::DEFAULT;
    for (rel, limit) in [
        ("macro.toml", d.manifest_bytes),
        ("tiny_counter.gds", d.gds_bytes),
        ("tiny_counter.lef", d.view_bytes),
        ("tiny_counter.lib", d.liberty_bytes),
        ("tiny_counter.v", d.view_bytes),
        ("tiny_counter.spice", d.view_bytes),
        ("evidence/lvs.json", d.evidence_bytes),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = package(tmp.path(), "tiny_counter.macro");
        // Sparse: the size is there for the size check to see, the bytes are
        // never written or read.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(pkg.join(rel))
            .unwrap();
        file.set_len(limit + 1).unwrap();
        let start = Instant::now();
        let report = check(&pkg, Limits::DEFAULT);
        assert!(start.elapsed() < Duration::from_secs(5), "{rel}");
        let message = format!(
            "file is {} bytes, larger than the limit of {limit} bytes",
            limit + 1
        );
        assert!(
            findings(&report).contains(&row("file.too_large", rel, &message)),
            "{rel}: {:?}",
            report.findings
        );
        // Reported once, though evidence names the GDS and SPICE files as inputs too.
        let n = report
            .rules()
            .iter()
            .filter(|r| **r == "file.too_large")
            .count();
        assert_eq!(n, 1, "{rel}");
    }
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package(tmp.path(), "tiny_ota.macro");
    std::fs::OpenOptions::new()
        .write(true)
        .open(pkg.join("tiny_ota.specs.toml"))
        .unwrap()
        .set_len(d.specs_bytes + 1)
        .unwrap();
    assert_eq!(check(&pkg, Limits::DEFAULT).rules(), ["file.too_large"]);
}

#[test]
fn manifest_limits() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package(tmp.path(), "tiny_counter.macro");
    let toml = std::fs::read_to_string(pkg.join("macro.toml")).unwrap();
    let dotted = vec!["k"; 300_000].join(".");
    std::fs::write(
        pkg.join("macro.toml"),
        format!("{toml}\n[extra]\n{dotted} = 1\n"),
    )
    .unwrap();
    let start = Instant::now();
    assert_eq!(
        findings(&check(&pkg, Limits::DEFAULT)),
        [row(
            "manifest.limit",
            "macro.toml",
            "tables and arrays nest more than 64 deep"
        )]
    );
    assert!(start.elapsed() < Duration::from_secs(5));
    let corners: String = (0..65)
        .map(|i| format!("\n[[corners]]\nname = \"c{i}\"\nprocess = \"tt\"\ntemperature = 27\n"))
        .collect();
    std::fs::write(
        pkg.join("macro.toml"),
        toml.replace("width = 2\n", "width = 9999\n") + &corners,
    )
    .unwrap();
    let report = check(&pkg, Limits::DEFAULT);
    assert_eq!(report.name.as_deref(), Some("tiny_counter"));
    assert_eq!(
        findings(&report),
        [
            row(
                "manifest.limit",
                "macro.toml",
                "the pins have more than 10000 bits"
            ),
            row("manifest.limit", "macro.toml", "more than 64 corners"),
        ]
    );
    // TOML that only TOML 1.1 allows, or numbers past 64 bits, are syntax errors.
    for extra in [
        "x = 99999999999999999999",
        "x = 1e400",
        "x = \"\\e\"",
        "x = {a = 1,}",
    ] {
        let e = Manifest::from_toml_str(&format!("{toml}\n[extra]\n{extra}\n"));
        assert!(e.is_err(), "{extra}");
    }
}

#[test]
fn evidence_limits() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package(tmp.path(), "tiny_counter.macro");
    let d = Limits::DEFAULT;
    for i in 0..d.evidence_files {
        std::fs::copy(
            pkg.join("evidence/drc.json"),
            pkg.join(format!("evidence/extra{i}.json")),
        )
        .unwrap();
    }
    assert_eq!(
        findings(&check(&pkg, d)),
        [row("evidence.limit", "evidence", "more than 64 summaries")]
    );
    for i in 0..d.evidence_files {
        std::fs::remove_file(pkg.join(format!("evidence/extra{i}.json"))).unwrap();
    }
    let few = Limits {
        hashed_files: 1,
        ..d
    };
    assert_eq!(
        findings(&check(&pkg, few)),
        [row(
            "evidence.limit",
            "evidence/lvs.json",
            "the summaries name more than 1 package files as inputs"
        )]
    );
}

#[test]
fn findings_past_the_limit_are_counted() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package(tmp.path(), "tiny_counter.macro");
    let lef = std::fs::read_to_string(pkg.join("tiny_counter.lef")).unwrap();
    let extra: String = (0..7)
        .map(|i| format!("  PIN x{i}\n    USE SIGNAL ;\n  END x{i}\n"))
        .collect();
    std::fs::write(
        pkg.join("tiny_counter.lef"),
        lef.replacen("  OBS", &(extra + "  OBS"), 1),
    )
    .unwrap();
    let toml = std::fs::read_to_string(pkg.join("macro.toml")).unwrap();
    std::fs::write(
        pkg.join("macro.toml"),
        toml.replace("kind = \"clock\"", "kind = \"signal\""),
    )
    .unwrap();
    let report = check(
        &pkg,
        Limits {
            findings: 5,
            ..Limits::DEFAULT
        },
    );
    let mut want = vec!["lef.use"];
    want.extend(["lef.extra_pin"; 4]);
    want.push("check.limit");
    assert_eq!(report.rules(), want);
    let last = report.findings.last().unwrap();
    assert_eq!(
        (
            last.severity.as_str(),
            last.file.as_str(),
            last.message.as_str()
        ),
        (
            "warning",
            ".",
            "3 more findings not listed: 3 errors, 0 warnings"
        )
    );
    assert_eq!(
        (report.errors, report.warnings, report.passed),
        (7, 2, false)
    );
}

#[cfg(unix)]
#[test]
fn unreadable_file() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package(tmp.path(), "tiny_counter.macro");
    let v = pkg.join("tiny_counter.v");
    std::fs::set_permissions(&v, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&v).is_ok() {
        return; // root reads it anyway
    }
    let report = check(&pkg, Limits::DEFAULT);
    assert_eq!(report.rules(), ["file.unreadable"]);
    assert!(
        report.findings[0]
            .message
            .starts_with("cannot read the file: ")
    );
}

// Random bytes: whatever a file holds, a check returns a report and no
// reader panics.

/// xorshift64*: a small seeded generator, so a failure repeats.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

#[test]
fn random_bytes_never_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package(tmp.path(), "tiny_counter.macro");
    for (seed, rel) in [
        "macro.toml",
        "tiny_counter.gds",
        "tiny_counter.lef",
        "tiny_counter.lib",
        "tiny_counter.v",
        "tiny_counter.spice",
        "evidence/drc.json",
    ]
    .iter()
    .enumerate()
    {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ (seed as u64 + 1));
        let good = std::fs::read(pkg.join(rel)).unwrap();
        for _ in 0..60 {
            let data: Vec<u8> = match rng.below(4) {
                0 => (0..rng.below(400)).map(|_| rng.next() as u8).collect(),
                1 => good[..rng.below(good.len() + 1)].to_vec(),
                _ => {
                    let mut d = good.clone();
                    for _ in 0..1 + rng.below(7) {
                        let at = rng.below(d.len());
                        d[at] = rng.next() as u8;
                    }
                    d
                }
            };
            std::fs::write(pkg.join(rel), &data).unwrap();
            check(&pkg, Limits::DEFAULT);
        }
        std::fs::write(pkg.join(rel), &good).unwrap();
    }
    // The readers on their own, with random text over the characters that
    // matter to them.
    let alphabet: Vec<char> = "ab ;{}()[]:,\"'\\/*#\n+.=`$01-é".chars().collect();
    let mut rng = Rng(0x5eed);
    for _ in 0..2000 {
        let text: String = (0..rng.below(300))
            .map(|_| alphabet[rng.below(alphabet.len())])
            .collect();
        let _ = parse_lef_with(&text, &SMALL);
        let _ = parse_liberty_with(&text, &SMALL, None);
        let _ = verilog_ports_with(&text, "a", &SMALL);
        let _ = spice_subckt_with(&text, "a", &SMALL);
        let _ = parse_gds_with(text.as_bytes(), &SMALL);
        let _ = Manifest::from_toml_str(&text);
    }
}
