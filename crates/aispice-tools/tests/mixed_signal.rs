//! Mixed-signal work through the tools: a Verilog-A comparator (OpenVAF)
//! driving a co-simulated Verilog counter (Verilator and ngspice's
//! d_cosim), the same counter driven by the IHP StrongARM comparator
//! template, what a deck may and may not load, spec budgets, and trim
//! range checks. Tests that need a tool skip, with a note, when it is
//! missing: ngspice for all simulations, OpenVAF for Verilog-A, Verilator,
//! a C++ compiler and ngspice's d_cosim sources for co-simulation, and
//! `AISPICE_PDK_IHP` for the StrongARM demo.

use aispice_agent::{Tool, ToolContext, ToolOutput};
use aispice_tools::pdk::{PROJECT_FILE, Registry};
use aispice_tools::tools::{BehavioralModels, BudgetTool, Simulate, TrimCheck};
use aispice_tools::{Project, RunMods, RunnerConfig, Workspace};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

fn have(program: &str) -> bool {
    aispice_sim::backend::which(program).is_some()
}

/// Whether this machine can build and run both kinds of behavioural model.
fn toolchain() -> bool {
    let ngspice = aispice_sim::backend::which("ngspice");
    let ok = ngspice.is_some()
        && have("openvaf")
        && have("verilator")
        && aispice_sim::behavioral::verilog::cxx().is_some()
        && ngspice
            .as_deref()
            .and_then(aispice_sim::behavioral::verilog::shim_dir)
            .is_some();
    if !ok {
        eprintln!(
            "skipped: needs ngspice with its d_cosim sources, OpenVAF, Verilator and a C++ compiler"
        );
    }
    ok
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let target = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), &target).unwrap();
        }
    }
}

/// An example copied into a fresh project, with a runner on ngspice.
fn project(example: &str, cfg: RunnerConfig) -> (tempfile::TempDir, Arc<Workspace>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(example);
    copy_dir(&examples().join(example), &root);
    let ws = Workspace::new();
    ws.runner.set_config(RunnerConfig {
        embedded_models_only: true,
        ..cfg
    });
    ws.set_project(Project::open(&root, Default::default()).unwrap());
    (dir, Arc::new(ws))
}

async fn call(tool: &dyn Tool, input: Value) -> ToolOutput {
    tool.call(&ToolContext::default(), input).await
}

/// The counter's four bits, read from the dac_bridge outputs at `at`.
async fn count(ws: &Arc<Workspace>, circuit: &str, at: &str) -> u32 {
    let measures: Vec<String> = (0..4)
        .map(|b| format!("q{b} = value_at(V(vq{b}), {at})"))
        .collect();
    let out = call(
        &Simulate { ws: ws.clone() },
        json!({"circuit": circuit, "simulator": "ngspice", "measurements": measures}),
    )
    .await;
    let text = out.text_content();
    assert!(!out.is_error, "{text}");
    let data = out.data.unwrap()["run"].clone();
    let vdd = data["measurements"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["value"].as_f64())
        .fold(0.0f64, f64::max)
        .max(1.0);
    let mut n = 0;
    for b in 0..4 {
        let m = data["measurements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == format!("q{b}"))
            .unwrap_or_else(|| panic!("no q{b}: {text}"));
        if m["value"].as_f64().unwrap() > vdd / 2.0 {
            n |= 1 << b;
        }
    }
    n
}

#[tokio::test(flavor = "multi_thread")]
async fn a_verilog_a_comparator_drives_a_cosimulated_counter() {
    if !toolchain() {
        return;
    }
    let (dir, ws) = project("mixed_signal", RunnerConfig::default());
    let root = dir.path().join("mixed_signal");
    // A library planted where aispice keeps its record is never loaded:
    // every library comes from a build in this process.
    let planted = root.join(".aispice/build/cosim");
    std::fs::create_dir_all(&planted).unwrap();
    std::fs::write(planted.join("cosim_counter.so"), b"not a library").unwrap();

    assert_eq!(count(&ws, "ramp_counter.cir", "9.9u").await, 5);
    // The record is the library that was loaded, with its hash.
    let lib = std::fs::read(planted.join("cosim_counter.so")).unwrap();
    assert_ne!(lib, b"not a library");
    let sha = std::fs::read_to_string(planted.join("cosim_counter.so.sha256")).unwrap();
    assert!(
        sha.starts_with(&aispice_sim::behavioral::sha256_hex(&lib)),
        "{sha}"
    );
    assert!(
        root.join(".aispice/build/veriloga/comparator.osdi")
            .is_file()
    );

    // The tool lists the models with the instance line to use.
    let out = call(
        &BehavioralModels { ws: ws.clone() },
        json!({"action": "build"}),
    )
    .await;
    let text = out.text_content();
    assert!(!out.is_error, "{text}");
    assert!(
        text.contains("use: A1 [clk en] [q3 q2 q1 q0] null counter"),
        "{text}"
    );
    assert!(
        text.contains("comparator_va (inp in, inn in, out out)"),
        "{text}"
    );

    // Asked for another simulator by name: refused before anything builds.
    let p = ws.project().unwrap();
    let err = ws
        .runner
        .run_circuit(
            &p,
            "ramp_counter.cir",
            Some("xyce"),
            &RunMods::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("run on ngspice only") || err.contains("not available"),
        "{err}"
    );
}

/// A deck cannot choose compiled code: no d_cosim or d_process card of its
/// own, whatever the spelling, even in a project that declares models.
#[tokio::test(flavor = "multi_thread")]
async fn a_deck_cannot_name_a_library_or_a_program() {
    if !have("ngspice") {
        eprintln!("skipped: ngspice not installed");
        return;
    }
    let (dir, ws) = project("mixed_signal", RunnerConfig::default());
    let root = dir.path().join("mixed_signal");
    std::fs::write(root.join("lib.so"), b"").unwrap();
    for card in [
        ".model counter d_cosim simulation=\"./lib.so\"",
        ".model counter d_cosim,simulation=\"lib.so\"",
        ".model counter (d_cosim simulation=\"lib.so\")",
        ".model p d_process process_file=\"./lib.so\"",
    ] {
        std::fs::write(
            root.join("evil.cir"),
            format!("* evil\nV1 a 0 1\nAbr [a] [ad] tobit\n.model tobit adc_bridge\nA1 [ad] [q] null counter\n{card}\n.tran 1u 10u\n.end\n"),
        )
        .unwrap();
        let out = call(
            &Simulate { ws: ws.clone() },
            json!({"circuit": "evil.cir", "simulator": "ngspice"}),
        )
        .await;
        let text = out.text_content();
        assert!(
            out.is_error && text.contains("native code"),
            "{card}: {text}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_strongarm_template_drives_a_cosimulated_counter() {
    let Some(pdk) = std::env::var_os("AISPICE_PDK_IHP") else {
        eprintln!("skipped: AISPICE_PDK_IHP is not set");
        return;
    };
    if !toolchain() {
        return;
    }
    let mut pdks = Registry::default();
    pdks.add("ihp-sg13g2", Path::new(&pdk)).unwrap();
    let (dir, ws) = project(
        "mixed_signal_ihp",
        RunnerConfig {
            pdks,
            ..RunnerConfig::default()
        },
    );
    assert!(
        std::fs::read_to_string(dir.path().join("mixed_signal_ihp").join(PROJECT_FILE))
            .unwrap()
            .contains("pdk = \"ihp-sg13g2\"")
    );
    assert_eq!(count(&ws, "strongarm_counter.cir", "16.4n").await, 5);
}

#[tokio::test]
async fn a_budget_writes_block_specs_check_specs_reads() {
    let (dir, ws) = project("mixed_signal", RunnerConfig::default());
    let tool = BudgetTool { ws: ws.clone() };
    let out = call(
        &tool,
        json!({
            "kind": "adc_frontend",
            "adc_frontend": {"bits": 10, "enob": 9.5, "full_scale": 1.0, "sample_rate": 50e6, "beta": 0.5},
            "save": {"ota": "ota.specs"}
        }),
    )
    .await;
    let text = out.text_content();
    assert!(!out.is_error, "{text}");
    assert!(text.contains("OTA gain-bandwidth: (N + 1) ln 2"), "{text}");
    assert!(text.contains("Saved ota.specs."), "{text}");
    let specs = std::fs::read_to_string(dir.path().join("mixed_signal/ota.specs")).unwrap();
    let parsed = aispice_sim::spec::parse_specs(&specs).unwrap();
    assert_eq!(
        parsed.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["gain", "gbw", "pm", "vn"]
    );
    let out = call(
        &tool,
        json!({"kind": "ldo", "ldo": {"vin_min": 2.7, "vout": 1.2, "vref": 0.6, "iload_max": 0.01,
            "load_regulation": 0.01, "psrr_dc_db": 40, "psrr_db": 20, "psrr_freq": 1e5,
            "noise_rms": 1e-4, "iq": 5e-5}, "save": {"amp": "x.specs"}}),
    )
    .await;
    assert!(
        out.is_error && out.text_content().contains("error_amp, ldo"),
        "{}",
        out.text_content()
    );
    let bad = call(&tool, json!({"kind": "ldo"})).await;
    assert!(bad.is_error);
}

/// A divider trimmed by a resistor bank: R1 varies by 5% between samples,
/// the trim code sets R2 in 12.5 ohm steps. Codes 0 to 15 cover every
/// sample; 0 to 5 do not.
#[tokio::test(flavor = "multi_thread")]
async fn a_trim_range_covers_every_sample_or_says_which_not() {
    if !have("ngspice") {
        eprintln!("skipped: ngspice not installed");
        return;
    }
    let (dir, ws) = project("mixed_signal", RunnerConfig::default());
    std::fs::write(
        dir.path().join("mixed_signal/divider.cir"),
        "* trimmed divider\n.param trim=8\nV1 in 0 1\nR1 in out 1k\nR2 out 0 {900 + trim*12.5}\n.tran 1u 10u\n.end\n",
    )
    .unwrap();
    let tool = TrimCheck { ws: ws.clone() };
    let input = |last: i64| {
        json!({
            "circuit": "divider.cir", "param": "trim", "first": 0, "last": last,
            "specs": "v = avg(V(out)) in 0.497..0.503",
            "tolerances": [{"target": "R1", "tol_pct": 5}],
            "runs": 12, "seed": 3, "simulator": "ngspice"
        })
    };
    let out = call(&tool, input(15)).await;
    let text = out.text_content();
    assert!(!out.is_error, "{text}");
    let r = &out.data.unwrap()["value"];
    assert_eq!(r["all_covered"], true, "{text}");
    assert_eq!(r["samples"], 12);
    let lo = r["lowest_needed"].as_i64().unwrap();
    let hi = r["highest_needed"].as_i64().unwrap();
    assert!(
        (3..=13).contains(&lo) && (3..=13).contains(&hi) && lo < hi,
        "{text}"
    );
    assert!(
        r["nominal_pass"].as_u64().unwrap() < 12,
        "untrimmed some fail: {text}"
    );
    let out = call(&tool, input(5)).await;
    let r = &out.data.unwrap()["value"];
    assert_eq!(r["all_covered"], false);
    assert!(!r["uncovered"].as_array().unwrap().is_empty());
    // A parameter the circuit does not set is refused before anything runs.
    let mut bad = input(15);
    bad["param"] = json!("trimm");
    let out = call(&tool, bad).await;
    assert!(out.is_error, "{}", out.text_content());
}

/// The IHP bandgap template with a 5-bit trim of its output resistor R3 in
/// 0.6% steps: device mismatch spreads vref far beyond a 1% window, and at
/// every process corner and mismatch sample some code brings it back.
#[tokio::test(flavor = "multi_thread")]
async fn the_bandgap_trims_into_range_at_every_corner_and_mismatch_sample() {
    let Some(pdk) = std::env::var_os("AISPICE_PDK_IHP") else {
        eprintln!("skipped: AISPICE_PDK_IHP is not set");
        return;
    };
    if !have("ngspice") {
        eprintln!("skipped: ngspice not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut pdks = Registry::default();
    pdks.add("ihp", Path::new(&pdk)).unwrap();
    std::fs::write(dir.path().join(PROJECT_FILE), "pdk = \"ihp\"\n").unwrap();
    let asc = String::from_utf8(
        aispice_tools::templates::find("bandgap")
            .unwrap()
            .asc
            .to_vec(),
    )
    .unwrap()
    .replace(
        "SYMATTR SpiceLine w=0.5u l=26.7u b=0 m=1",
        "SYMATTR SpiceLine w=0.5u l={26.7u*(1+(trim-16)*0.006)} b=0 m=1",
    )
    .replace(
        "TEXT 48 672 Left 2 !.dc temp -40 125 5",
        "TEXT 48 672 Left 2 !.dc temp -40 125 5\nTEXT 48 712 Left 2 !.param trim=16",
    );
    assert!(asc.contains("(trim-16)") && asc.contains(".param trim=16"));
    std::fs::write(dir.path().join("bandgap.asc"), asc).unwrap();
    let ws = Workspace::new();
    ws.runner.set_config(RunnerConfig {
        pdks,
        embedded_models_only: true,
        ..RunnerConfig::default()
    });
    ws.set_project(Project::open(dir.path(), Default::default()).unwrap());
    let ws = Arc::new(ws);
    let out = call(
        &TrimCheck { ws },
        json!({
            "circuit": "bandgap.asc", "param": "trim", "first": 0, "last": 31,
            "specs": "vref = value_at(V(vref), 27) in 0.597..0.603",
            "corners": {"process": ["tt", "ff"]},
            "mismatch": "local", "runs": 4, "seed": 1, "simulator": "ngspice"
        }),
    )
    .await;
    let text = out.text_content();
    println!("{text}");
    assert!(!out.is_error, "{text}");
    let r = &out.data.unwrap()["value"];
    assert_eq!(r["samples"], 8, "{text}");
    assert_eq!(r["all_covered"], true, "{text}");
    assert_eq!(r["nominal_code"], 16);
    assert!(r["nominal_pass"].as_u64().unwrap() < 8, "{text}");
}

/// Every limit holds on the request before any work: a 32-bit trim range,
/// corners times samples over the cap (with the largest request that
/// fits), and a sample count that would overflow.
#[tokio::test]
async fn trim_limits_hold_before_anything_runs() {
    let (dir, ws) = project("mixed_signal", RunnerConfig::default());
    std::fs::write(
        dir.path().join("mixed_signal/divider.cir"),
        "* trimmed divider\n.param trim=8\nV1 in 0 1\nR1 in out 1k\nR2 out 0 {900 + trim*12.5}\n.tran 1u 10u\n.end\n",
    )
    .unwrap();
    let tool = TrimCheck { ws };
    let base = json!({
        "circuit": "divider.cir", "param": "trim", "first": 0, "last": 15,
        "specs": "v = avg(V(out)) in 0.497..0.503",
        "tolerances": [{"target": "R1", "tol_pct": 5}], "simulator": "ngspice"
    });
    let with = |k: &str, v: Value| {
        let mut i = base.clone();
        i[k] = v;
        i
    };
    let started = std::time::Instant::now();
    let out = call(&tool, with("last", json!(4294967295u64))).await;
    assert!(
        out.is_error && out.text_content().contains("more than the 64"),
        "{}",
        out.text_content()
    );
    let out = call(&tool, with("first", json!(i64::MIN))).await;
    assert!(out.is_error, "{}", out.text_content());
    let mut many = with("runs", json!(500));
    many["corners"] = json!({"temperatures": [-40, 0, 27, 85, 125, 150, 175, 200, 225]});
    let out = call(&tool, many).await;
    let text = out.text_content();
    assert!(
        out.is_error && text.contains("ask for at most 13 sample(s) per corner"),
        "{text}"
    );
    let out = call(&tool, with("runs", json!(usize::MAX))).await;
    assert!(
        out.is_error && out.text_content().contains("over the 2000"),
        "{}",
        out.text_content()
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

/// A co-simulation asked to run for a very long time at a tiny step, or a
/// counter clocked at every step, is refused before anything compiles.
#[tokio::test(flavor = "multi_thread")]
async fn an_endless_cosimulation_is_refused_before_it_builds() {
    let (dir, ws) = project("mixed_signal", RunnerConfig::default());
    let root = dir.path().join("mixed_signal");
    let deck = std::fs::read_to_string(root.join("ramp_counter.cir"))
        .unwrap()
        .replace(".tran 10n 10u", ".tran 1p 1");
    std::fs::write(root.join("long.cir"), deck).unwrap();
    std::fs::write(
        root.join("rtl/toggle.v"),
        "module toggle(input clk, output reg q);\n  initial q = 1'b0;\n  always @(clk) q <= ~q;\nendmodule\n",
    )
    .unwrap();
    let mut toml = std::fs::read_to_string(root.join(PROJECT_FILE)).unwrap();
    toml.push_str("\n[[cosim]]\nmodel = \"toggle\"\nsources = [\"rtl/toggle.v\"]\n");
    std::fs::write(root.join(PROJECT_FILE), toml).unwrap();
    std::fs::write(
        root.join("toggle.cir"),
        "* toggles at every step\nVclk c 0 PULSE(0 1 0 0.5p 0.5p 0.5p 2p)\nAb [c] [cd] tobit\n.model tobit adc_bridge(in_low=0.3 in_high=0.7)\nAt [cd] [q] null toggle\nAq [q] [vq] toana\n.model toana dac_bridge\n.tran 1p 10u\n.end\n",
    )
    .unwrap();
    let p = ws.project().unwrap();
    for (circuit, why) in [
        ("long.cir", "the stop time can be at most 2u"),
        ("toggle.cir", "at most 2e6"),
    ] {
        let started = std::time::Instant::now();
        let err = ws
            .runner
            .run_circuit(
                &p,
                circuit,
                Some("ngspice"),
                &RunMods::default(),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(why), "{circuit}: {err}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{circuit}"
        );
        assert!(
            !root.join(".aispice/build").exists(),
            "nothing was built for {circuit}"
        );
    }
}
