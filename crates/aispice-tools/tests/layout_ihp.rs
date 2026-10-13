//! The A3 acceptance flow on IHP SG13G2: a five-transistor OTA laid out by
//! the layout tool, DRC and LVS clean, extracted with Magic and simulated
//! after layout, with gain bandwidth and phase margin before and after.
//!
//! Needs `AISPICE_PDK_IHP` (with the PDK's OSDI models and the pcell
//! submodules), KLayout and Magic; passes with a note when any is missing.

use aispice_agent::ToolContext;
use aispice_tools::pdk::{PROJECT_FILE, Registry};
use aispice_tools::{Approver, Hooks, Project, Runner, RunnerConfig, Workspace};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A five-transistor OTA: NMOS pair, PMOS mirror load, NMOS tail biased from
/// a 20 uA diode, a MIM load capacitor. Open loop, inputs at 0.6 V, AC on
/// the non-inverting input.
const OTA: &str = "Version 4
SHEET 1 1200 900
FLAG 48 0 d1
FLAG 0 48 inp
FLAG 48 96 tail
FLAG 48 48 0
FLAG 208 0 out
FLAG 160 48 inm
FLAG 208 96 tail
FLAG 208 48 0
FLAG 48 -200 vdd
FLAG 0 -152 d1
FLAG 48 -104 d1
FLAG 48 -152 vdd
FLAG 208 -200 vdd
FLAG 160 -152 d1
FLAG 208 -104 out
FLAG 208 -152 vdd
FLAG 128 200 tail
FLAG 80 248 vbn
FLAG 128 296 0
FLAG 128 248 0
FLAG 348 200 vbn
FLAG 300 248 vbn
FLAG 348 296 0
FLAG 348 248 0
FLAG 480 0 vdd
FLAG 480 80 vbn
FLAG 576 0 out
FLAG 576 64 0
FLAG 700 16 vdd
FLAG 700 96 0
FLAG 800 16 inp
FLAG 800 96 0
FLAG 900 16 inm
FLAG 900 96 0
SYMBOL sg13g2\\sg13_lv_nmos 0 0 R0
SYMATTR InstName M1
SYMATTR SpiceLine w=2u l=0.5u ng=1 m=1
SYMBOL sg13g2\\sg13_lv_nmos 160 0 R0
SYMATTR InstName M2
SYMATTR SpiceLine w=2u l=0.5u ng=1 m=1
SYMBOL sg13g2\\sg13_lv_pmos 0 -200 R0
SYMATTR InstName M3
SYMATTR SpiceLine w=4u l=0.5u ng=1 m=1
SYMBOL sg13g2\\sg13_lv_pmos 160 -200 R0
SYMATTR InstName M4
SYMATTR SpiceLine w=4u l=0.5u ng=1 m=1
SYMBOL sg13g2\\sg13_lv_nmos 80 200 R0
SYMATTR InstName M5
SYMATTR SpiceLine w=4u l=1u ng=1 m=1
SYMBOL sg13g2\\sg13_lv_nmos 300 200 R0
SYMATTR InstName M6
SYMATTR SpiceLine w=4u l=1u ng=1 m=1
SYMBOL current 480 0 R0
SYMATTR InstName IB
SYMATTR Value 20u
SYMBOL sg13g2\\cap_cmim 560 0 R0
SYMATTR InstName C1
SYMATTR SpiceLine w=10u l=10u m=1
SYMBOL voltage 700 0 R0
SYMATTR InstName VDD
SYMATTR Value 1.2
SYMBOL voltage 800 0 R0
SYMATTR InstName VINP
SYMATTR Value 0.6
SYMATTR SpiceLine AC 1
SYMBOL voltage 900 0 R0
SYMATTR InstName VINM
SYMATTR Value 0.6
TEXT 0 400 Left 2 !.ac dec 20 1 10G
";

/// A two-stage Miller op-amp on the same bias, as a netlist: the OTA's first
/// stage, a PMOS second stage with its NMOS sink and a 10 x 10 um MIM
/// capacitor (15 x 15 um) from the first stage's output to the output, into 0.5 pF. Its gain is too
/// high to bias open loop, so a 1 GH inductor closes the loop at DC and a
/// 1 F capacitor grounds that input for AC. The second stage inverts, so
/// the loop closes on inp and the signal goes into inm.
const TWO_STAGE: &str = "* two-stage Miller op-amp\nVDD vdd 0 1.2\nVM inm 0 0.6 AC 1\nLFB out inp 1G\nCFB inp 0 1\nIB vdd vbn 20u\nCL out 0 0.5p\nXM1 x inp tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1\nXM2 out1 inm tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1\nXM3 x x vdd vdd sg13_lv_pmos w=4u l=0.5u ng=1 m=1\nXM4 out1 x vdd vdd sg13_lv_pmos w=4u l=0.5u ng=1 m=1\nXM5 tail vbn 0 0 sg13_lv_nmos w=4u l=1u ng=1 m=1\nXM8 vbn vbn 0 0 sg13_lv_nmos w=4u l=1u ng=1 m=1\nXM6 out out1 vdd vdd sg13_lv_pmos w=16u l=0.5u ng=1 m=1\nXM7 out vbn 0 0 sg13_lv_nmos w=8u l=1u ng=1 m=1\nXC1 out1 out cap_cmim w=15u l=15u m=1\n.ac dec 20 1 10G\n.end\n";

const SPECS: &str = "gain = gain_db_at(V(out), 10) >= 20\ngbw = unity_gain_freq(V(out)) >= 1Meg\npm = phase_margin(V(out)) >= 45\n";

fn workspace() -> Option<(tempfile::TempDir, Arc<Workspace>)> {
    let programs = aispice_layout::tools::Programs::default();
    if programs.magic().is_err() {
        eprintln!("skipped: Magic is needed");
        return None;
    }
    workspace_with_klayout()
}

/// The project, for tests that need KLayout but not Magic.
fn workspace_with_klayout() -> Option<(tempfile::TempDir, Arc<Workspace>)> {
    let Some(pdk) = std::env::var_os("AISPICE_PDK_IHP").map(PathBuf::from) else {
        eprintln!("skipped: AISPICE_PDK_IHP is not set");
        return None;
    };
    let programs = aispice_layout::tools::Programs::default();
    if programs.klayout().is_err() {
        eprintln!("skipped: KLayout is needed");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut pdks = Registry::default();
    pdks.add("ihp-sg13g2", &pdk).unwrap();
    let files =
        aispice_layout::tools::PdkLayoutFiles::of(&pdks.open("ihp-sg13g2").unwrap()).unwrap();
    if let Err(e) = files.check_pcells() {
        eprintln!("skipped: {e}");
        return None;
    }
    std::fs::write(dir.path().join(PROJECT_FILE), "pdk = \"ihp-sg13g2\"\n").unwrap();
    std::fs::write(dir.path().join("ota.asc"), OTA).unwrap();
    std::fs::write(dir.path().join("two_stage.cir"), TWO_STAGE).unwrap();
    let runner = Runner::new(RunnerConfig {
        pdks,
        embedded_models_only: true,
        ..RunnerConfig::default()
    });
    let ws = Workspace::new();
    ws.runner.set_config(runner.config());
    ws.set_project(Project::open(dir.path(), Default::default()).unwrap());
    Some((dir, Arc::new(ws)))
}

async fn call(ws: &Arc<Workspace>, tool: &str, input: Value) -> aispice_agent::ToolOutput {
    let tools = aispice_tools::registry(ws.clone());
    let started = std::time::Instant::now();
    let out = tools
        .get(tool)
        .unwrap_or_else(|| panic!("no tool {tool}"))
        .call(&ToolContext::default(), input)
        .await;
    eprintln!(
        "== {tool} ({:.1} s)\n{}",
        started.elapsed().as_secs_f64(),
        out.text_content()
    );
    assert!(!out.is_error, "{tool}: {}", out.text_content());
    out
}

fn row(report: &Value, name: &str) -> f64 {
    report["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .and_then(|r| r["value"].as_f64())
        .unwrap_or_else(|| panic!("no {name} in {report}"))
}

#[tokio::test]
async fn the_ota_is_laid_out_checked_extracted_and_simulated_after_layout() {
    let Some((dir, ws)) = workspace() else {
        return;
    };
    let out = call(
        &ws,
        "layout",
        json!({"action": "generate", "circuit": "ota.asc", "template": "ota5t"}),
    )
    .await;
    let v = &out.data.as_ref().unwrap()["value"];
    assert_eq!(v["layout"], "ota.gds");
    assert_eq!(v["drc"]["rules"], json!([]), "DRC clean");
    assert_eq!(v["lvs"]["matched"], json!(true), "LVS clean");
    assert!(dir.path().join("ota.gds").is_file());
    assert!(dir.path().join("ota.layout.json").is_file());

    let picture = call(
        &ws,
        "layout",
        json!({"action": "render", "layout": "ota.gds"}),
    )
    .await;
    assert!(
        picture
            .content
            .iter()
            .any(|c| matches!(c, aispice_agent::ContentBlock::Image { .. }))
    );

    for resistance in [false, true] {
        let out = call(
            &ws,
            "check_specs",
            json!({"circuit": "ota.asc", "specs": SPECS, "save": false,
                   "post_layout": {"layout": "ota.gds", "resistance": resistance}}),
        )
        .await;
        let text = out.text_content();
        assert!(text.contains("before and after layout"), "{text}");
        let v = &out.data.as_ref().unwrap()["value"];
        let (gbw0, gbw1) = (row(&v["pre"], "gbw"), row(&v["post"], "gbw"));
        let (pm0, pm1) = (row(&v["pre"], "pm"), row(&v["post"], "pm"));
        eprintln!(
            "resistance {resistance}: GBW {:.3} MHz -> {:.3} MHz, PM {:.2} -> {:.2} deg",
            gbw0 / 1e6,
            gbw1 / 1e6,
            pm0,
            pm1
        );
        // Layout adds capacitance, so the unity-gain frequency falls a
        // little and the amplifier still works.
        assert!(gbw1 < gbw0 && gbw1 > 0.7 * gbw0, "{gbw0} {gbw1}");
        assert!(pm1 > 45.0, "{pm1}");
    }

    // At corners, before and after layout: the worst case of each spec, and
    // every corner of the post-layout run.
    let out = call(
        &ws,
        "check_specs",
        json!({"circuit": "ota.asc", "specs": SPECS, "save": false,
               "corners": {"process": ["tt", "ss", "ff"], "temperatures": [-40, 125]},
               "post_layout": {"layout": "ota.gds"}}),
    )
    .await;
    let v = &out.data.as_ref().unwrap()["value"];
    assert_eq!(v["corners_post"]["corners"].as_array().unwrap().len(), 6);
    assert!(v["corners_post"]["failed"].as_array().unwrap().is_empty());
    let (gbw0, gbw1) = (row(&v["pre"], "gbw"), row(&v["post"], "gbw"));
    eprintln!(
        "worst corner GBW {:.3} MHz -> {:.3} MHz",
        gbw0 / 1e6,
        gbw1 / 1e6
    );
    assert!(gbw1 < gbw0, "{gbw0} {gbw1}");
}

/// Capacitance per net in an extracted netlist's text, fF: each capacitor
/// counts for both its nets.
fn net_caps(text: &str) -> BTreeMap<String, f64> {
    let mut out: BTreeMap<String, f64> = BTreeMap::new();
    for line in text.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        if w.len() < 4 || !w[0].starts_with('C') {
            continue;
        }
        let Some(v) = aispice_core::units::parse(w[3]) else {
            continue;
        };
        for n in &w[1..3] {
            *out.entry(n.to_string()).or_default() += v * 1e15;
        }
    }
    out
}

/// The comparator template laid out by its generator, which draws it
/// mirror-symmetric: DRC and LVS clean, the two outputs loaded alike after
/// extraction, both decisions at every corner of the template's set with its
/// table after layout, and an input-referred offset after layout within the
/// template's bound: a constant input difference of that size, either way,
/// decides the right way at tt and 27 C.
#[tokio::test]
async fn the_comparator_template_decides_both_ways_after_layout() {
    let Some((dir, ws)) = workspace() else {
        return;
    };
    let t = aispice_tools::templates::find("comparator").unwrap();
    let hint = t.layout.as_ref().unwrap();
    let after = hint.specs.as_deref().unwrap();
    let bound = hint.offset_mv.unwrap();
    let asc = String::from_utf8(t.asc.to_vec()).unwrap();
    std::fs::write(dir.path().join("comparator.asc"), &asc).unwrap();
    let out = call(
        &ws,
        "layout",
        json!({"action": "generate", "circuit": "comparator.asc", "template": "comparator"}),
    )
    .await;
    let v = &out.data.as_ref().unwrap()["value"];
    assert_eq!(v["drc"]["rules"], json!([]), "DRC clean");
    assert_eq!(v["lvs"]["matched"], json!(true), "LVS clean");

    let out = call(&ws, "extract", json!({"layout": "comparator.gds"})).await;
    let caps = net_caps(&out.text_content());
    let (n, p) = (caps["outn"], caps["outp"]);
    eprintln!("outputs after extraction: outn {n:.2} fF, outp {p:.2} fF");
    assert!((n - p).abs() / n.max(p) < 0.03, "outn {n} fF, outp {p} fF");

    let out = call(
        &ws,
        "check_specs",
        json!({"circuit": "comparator.asc", "specs": after, "save": false,
               "corners": t.corners, "post_layout": {"layout": "comparator.gds"}}),
    )
    .await;
    let v = &out.data.as_ref().unwrap()["value"];
    assert!(
        v["corners_post"]["failed"].as_array().unwrap().is_empty(),
        "{}",
        out.text_content()
    );

    // The bench with constant inputs, inp above inm by d (in mV); the first
    // decision only.
    let at = |d: f64| {
        let vp = format!("SYMATTR Value {:.6}", 0.7 + d / 2000.0);
        let vm = format!("SYMATTR Value {:.6}", 0.7 - d / 2000.0);
        asc.replace("SYMATTR Value PWL(0 0.7005 1.5n 0.7005 1.55n 0.6995)", &vp)
            .replace("SYMATTR Value PWL(0 0.6995 1.5n 0.6995 1.55n 0.7005)", &vm)
    };
    assert_ne!(
        at(1.0),
        asc,
        "the bench's inputs are where the test expects"
    );
    // The same devices, so the layout still stands for them.
    for d in [-bound, bound] {
        std::fs::write(dir.path().join("comparator.asc"), at(d)).unwrap();
        let spec = if d > 0.0 {
            "d1 = value_at(V(outp,outn), 0.95n) >= 1"
        } else {
            "d1 = value_at(V(outp,outn), 0.95n) <= -1"
        };
        let out = call(
            &ws,
            "check_specs",
            json!({"circuit": "comparator.asc", "specs": spec, "save": false,
                   "post_layout": {"layout": "comparator.gds"}}),
        )
        .await;
        let v = &out.data.as_ref().unwrap()["value"];
        eprintln!("{d:+} mV: {} V after layout", row(&v["post"], "d1"));
        assert_eq!(
            v["post"]["rows"][0]["pass"],
            json!(true),
            "an input {d:+} mV apart decides the wrong way after layout: {}",
            out.text_content()
        );
    }
}

/// Every file of the project outside aispice's own `.aispice/`, with its
/// contents.
fn user_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in std::fs::read_dir(dir).unwrap().map(Result::unwrap) {
            let path = e.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel == ".aispice" {
                continue;
            }
            if e.file_type().unwrap().is_dir() {
                walk(root, &path, out);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Approves every write and counts the requests.
#[derive(Default)]
struct Counting(AtomicUsize);

#[async_trait]
impl Approver for Counting {
    async fn approve(&self, _circuit: &str, _summary: &str, _diff: &str) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        true
    }
}

struct Decline;

#[async_trait]
impl Approver for Decline {
    async fn approve(&self, _circuit: &str, _summary: &str, _diff: &str) -> bool {
        false
    }
}

/// What each layout tool's access class promises, on real runs: `layout`
/// (Edit) writes the GDS and its record and nothing else, after one
/// approval, and nothing when declined; `drc`, `lvs`, `extract`, `layout
/// render` and `check_specs` with `post_layout` (and `save` off) write
/// nothing of the user's and ask for nothing, keeping their reports in
/// `.aispice/layout/`. Extraction is checked when Magic is installed.
#[tokio::test]
async fn each_layout_tool_writes_only_what_its_access_class_allows() {
    let Some((dir, ws)) = workspace_with_klayout() else {
        return;
    };
    let approvals = Arc::new(Counting::default());
    ws.set_hooks(Hooks {
        approver: Some(approvals.clone()),
        after_save: None,
    });
    let before = user_files(dir.path());

    let generate =
        json!({"action": "generate", "circuit": "ota.asc", "template": "ota5t", "check": false});
    call(&ws, "layout", generate.clone()).await;
    assert_eq!(approvals.0.load(Ordering::SeqCst), 1, "one approval");
    let after = user_files(dir.path());
    let mut expected = before.clone();
    expected.insert("ota.gds".into(), after["ota.gds"].clone());
    expected.insert("ota.layout.json".into(), after["ota.layout.json"].clone());
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>()
    );
    assert!(after == expected, "layout changed a file it did not name");

    let mut read_only = vec![
        ("drc", json!({"layout": "ota.gds"})),
        ("lvs", json!({"layout": "ota.gds", "circuit": "ota.asc"})),
        ("layout", json!({"action": "render", "layout": "ota.gds"})),
    ];
    if aispice_layout::tools::Programs::default().magic().is_ok() {
        read_only.push(("extract", json!({"layout": "ota.gds"})));
        read_only.push((
            "check_specs",
            json!({"circuit": "ota.asc", "specs": SPECS, "save": false,
                   "post_layout": {"layout": "ota.gds"}}),
        ));
    } else {
        eprintln!("extract and check_specs post_layout skipped: Magic is not installed");
    }
    for (tool, input) in read_only {
        call(&ws, tool, input).await;
        assert!(
            user_files(dir.path()) == after,
            "{tool} changed the user's files"
        );
        assert_eq!(
            approvals.0.load(Ordering::SeqCst),
            1,
            "{tool} asked to write"
        );
    }
    let runs = std::fs::read_dir(dir.path().join(".aispice/layout")).unwrap();
    assert!(runs.count() >= 3, "reports in .aispice/layout");

    // Declined, generating again leaves the layout as it was.
    ws.set_hooks(Hooks {
        approver: Some(Arc::new(Decline)),
        after_save: None,
    });
    let out = aispice_tools::registry(ws.clone())
        .get("layout")
        .unwrap()
        .call(&ToolContext::default(), generate)
        .await;
    assert!(out.is_error, "{}", out.text_content());
    assert!(
        out.text_content().contains("declined"),
        "{}",
        out.text_content()
    );
    assert!(
        user_files(dir.path()) == after,
        "a declined layout was written"
    );
}

#[tokio::test]
async fn the_two_stage_op_amp_is_laid_out_and_simulated_after_layout() {
    let Some((_dir, ws)) = workspace() else {
        return;
    };
    let out = call(
        &ws,
        "layout",
        json!({"action": "generate", "circuit": "two_stage.cir", "template": "two_stage"}),
    )
    .await;
    let v = &out.data.as_ref().unwrap()["value"];
    assert_eq!(v["drc"]["rules"], json!([]), "DRC clean");
    assert_eq!(v["lvs"]["matched"], json!(true), "LVS clean");
    let specs = "gain = gain_db_at(V(out), 10) >= 50\ngbw = unity_gain_freq(V(out)) >= 1Meg\npm = phase_margin(V(out)) >= 30\n";
    let out = call(
        &ws,
        "check_specs",
        json!({"circuit": "two_stage.cir", "specs": specs, "save": false,
               "post_layout": {"layout": "two_stage.gds"}}),
    )
    .await;
    let v = &out.data.as_ref().unwrap()["value"];
    let (gbw0, gbw1) = (row(&v["pre"], "gbw"), row(&v["post"], "gbw"));
    let (pm0, pm1) = (row(&v["pre"], "pm"), row(&v["post"], "pm"));
    eprintln!(
        "two-stage: GBW {:.3} MHz -> {:.3} MHz, PM {:.2} -> {:.2} deg",
        gbw0 / 1e6,
        gbw1 / 1e6,
        pm0,
        pm1
    );
    assert!(gbw1 > 0.5 * gbw0 && gbw1 < 1.5 * gbw0, "{gbw0} {gbw1}");
    assert!(pm1 > 30.0, "{pm1}");
}
