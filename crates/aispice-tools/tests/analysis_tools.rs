//! Noise, PSRR and CMRR, corners and mismatch Monte Carlo through the tools,
//! on ngspice (and Xyce where a test says so), with small libraries written
//! for the tests. Skipped when the simulator is missing.

use aispice_agent::{ToolContext, ToolOutput};
use aispice_tools::{Project, ProjectOptions, Workspace, registry};
use serde_json::{Value, json};
use std::sync::Arc;

fn have(program: &str, arg: &str) -> bool {
    std::process::Command::new(program)
        .arg(arg)
        .output()
        .is_ok()
}

fn have_ngspice() -> bool {
    let ok = have("ngspice", "--version");
    if !ok {
        eprintln!("skipped: ngspice not installed");
    }
    ok
}

/// A project holding the given files.
fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Arc<Workspace>) {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    let project = Project::open(dir.path(), ProjectOptions::default()).unwrap();
    (dir, Arc::new(Workspace::with_project(project)))
}

async fn call(ws: &Arc<Workspace>, tool: &str, input: Value) -> ToolOutput {
    let reg = registry(ws.clone());
    reg.get(tool)
        .unwrap()
        .call(&ToolContext::default(), input)
        .await
}

fn ok(out: &ToolOutput) -> String {
    let text = out.text_content();
    assert!(!out.is_error, "{text}");
    text
}

/// Was a bug: model resolution dropped an `.include` of a file beside the
/// circuit, because it kept only absolute paths, so the run on ngspice
/// failed for want of the model.
#[tokio::test(flavor = "multi_thread")]
async fn includes_beside_the_circuit_reach_ngspice() {
    if !have_ngspice() {
        return;
    }
    let (_dir, ws) = project(&[
        ("toy.lib", "* toy diode\n.model dtoy D(IS=1e-14)\n"),
        (
            "d.cir",
            "* include test\n.include toy.lib\nI1 0 a 1m\nD1 a 0 dtoy\n.op\n.end\n",
        ),
    ]);
    let out = call(
        &ws,
        "simulate",
        json!({"circuit": "d.cir", "simulator": "ngspice", "measurements": ["v = value_at(V(a), 0)"]}),
    )
    .await;
    let text = ok(&out);
    assert!(text.contains("v = 655"), "{text}");
}

fn have_xyce() -> bool {
    let ok = have("Xyce", "-v");
    if !ok {
        eprintln!("skipped: Xyce not installed");
    }
    ok
}

fn data(out: &ToolOutput) -> &Value {
    out.data.as_ref().expect("structured data")
}

const NOISE_DIVIDER: &str = "* noise divider\nV1 in 0 DC 0 AC 1\nR1 in out 1k\nR2 out 0 1k\n.noise V(out) V1 dec 10 1 1Meg\n.end\n";

#[tokio::test(flavor = "multi_thread")]
async fn noise_through_simulate_and_check_specs() {
    if !have_ngspice() {
        return;
    }
    let (_dir, ws) = project(&[("n.cir", NOISE_DIVIDER)]);
    let out = call(
        &ws,
        "simulate",
        json!({"circuit": "n.cir", "measurements": ["vn = output_noise_at(1k)", "rms = output_noise_rms(10, 100k)"]}),
    )
    .await;
    let text = ok(&out);
    assert!(text.contains("vn = 2.879nV/√Hz"), "{text}");
    assert!(text.contains("rms = 910.3nV"), "{text}");
    let out = call(
        &ws,
        "check_specs",
        json!({"circuit": "n.cir", "specs": "vin = input_noise_at(1k) <= 6n\nvout = output_noise_at(1k) <= 2.5n", "save": false}),
    )
    .await;
    let report = &data(&out)["report"];
    assert_eq!(report["rows"][0]["pass"], true, "{report}");
    assert_eq!(report["rows"][1]["pass"], false, "{report}");
}

const PSRR: &str = "* psrr of a controlled source\nVDD vdd 0 DC 1.8\nVin inp 0 DC 0 AC 1\nE1 out x inp 0 100\nE2 x 0 vdd 0 0.01\nRl out 0 10k\n.ac dec 10 1 1Meg\n.end\n";

const CMRR: &str = "* cmrr of a controlled source\nVcm cm 0 DC 1\nVp inp cm AC 0.5\nVn inn cm AC 0.5 180\nR1 inp avg 1k\nR2 inn avg 1k\nE1 out x inp inn 1000\nE2 x 0 avg 0 0.1\nRl out 0 10k\n.ac dec 10 1 1Meg\n.end\n";

#[tokio::test(flavor = "multi_thread")]
async fn psrr_and_cmrr_have_known_answers() {
    if !have_ngspice() {
        return;
    }
    let mut sims = vec!["ngspice"];
    if have_xyce() {
        sims.push("xyce");
    }
    let (_dir, ws) = project(&[("psrr.cir", PSRR), ("cmrr.cir", CMRR)]);
    for sim in sims {
        let out = call(
            &ws,
            "check_specs",
            json!({"circuit": "psrr.cir", "simulator": sim, "save": false,
                   "specs": "psrr = psrr_db_at(V(out), VDD, 1k) = 80 +- 0.01\ngain = gain_db_at(V(out), 1k) = 40 +- 0.01"}),
        )
        .await;
        let report = &data(&out)["report"];
        assert_eq!(report["all_pass"], true, "{sim}: {}", out.text_content());
        let out = call(
            &ws,
            "check_specs",
            json!({"circuit": "cmrr.cir", "simulator": sim, "save": false,
                   "specs": "cmrr = cmrr_db_at(V(out), Vcm, 10k) = 80 +- 0.01"}),
        )
        .await;
        assert_eq!(
            data(&out)["report"]["all_pass"],
            true,
            "{sim}: {}",
            out.text_content()
        );
    }
    // measure on a stored run adds the derived run too.
    let out = call(
        &ws,
        "simulate",
        json!({"circuit": "psrr.cir", "measurements": ["p = psrr_db_at(V(out), VDD, 100)"]}),
    )
    .await;
    assert!(ok(&out).contains("p = 80dB"), "{}", out.text_content());
    let out = call(
        &ws,
        "measure",
        json!({"circuit": "psrr.cir", "measurements": ["p = psrr_db_at(V(out), VDD, 10k)"]}),
    )
    .await;
    assert!(ok(&out).contains("p = 80dB"), "{}", out.text_content());
    // A source that is not there fails before anything runs.
    let out = call(
        &ws,
        "check_specs",
        json!({"circuit": "psrr.cir", "save": false, "specs": "p = psrr_db_at(V(out), VCC, 1k) >= 60"}),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.text_content().contains("its sources are VDD, Vin"),
        "{}",
        out.text_content()
    );
}

/// A library with three resistance corners, as a PDK writes them.
const CORNER_LIB: &str = "* toy resistor corners\n.lib res_typ\n.param rsh=1k\n.endl res_typ\n.lib res_wcs\n.param rsh=1.2k\n.endl res_wcs\n.lib res_bcs\n.param rsh=800\n.endl res_bcs\n";

/// An RC lowpass whose resistor follows the corner and warms by 0.2%/C.
const CORNER_RC: &str = "* corner rc\n.lib toy.lib res_typ\nV1 in 0 DC {vdd} AC 1\n.param vdd=1.8\nR1 in out {rsh} tc1=2m\nC1 out 0 100n\n.ac dec 50 10 1Meg\n.end\n";

#[tokio::test(flavor = "multi_thread")]
async fn corners_name_the_worst_corner() {
    if !have_ngspice() {
        return;
    }
    let mut sims = vec!["ngspice"];
    if have_xyce() {
        sims.push("xyce");
    }
    let (_dir, ws) = project(&[("toy.lib", CORNER_LIB), ("rc.cir", CORNER_RC)]);
    for sim in sims {
        let out = call(
            &ws,
            "check_specs",
            json!({"circuit": "rc.cir", "simulator": sim, "save": false,
                   "specs": "bw = bandwidth_3db(V(out)) >= 1.2k",
                   "corners": {"process": ["tt", "ss", "ff"], "temperatures": [27, 125],
                               "supplies": [{"target": "vdd", "values": ["1.62", 1.98]}]}}),
        )
        .await;
        let text = ok(&out);
        let corners = &data(&out)["corners"];
        assert_eq!(corners["corners"].as_array().unwrap().len(), 12, "{text}");
        let row = &corners["rows"][0];
        // Slow resistors, hot: 1.2k x 1.196.
        let want = 1.0 / (2.0 * std::f64::consts::PI * 1200.0 * 1.196 * 100e-9);
        let worst = row["worst_value"].as_f64().unwrap();
        assert!(
            (worst - want).abs() < 0.01 * want,
            "{sim}: {worst} vs {want}\n{text}"
        );
        assert!(
            row["worst_corner"]
                .as_str()
                .unwrap()
                .starts_with("ss/125C/vdd="),
            "{sim}: {text}"
        );
        assert_eq!(row["pass"], false);
        assert_eq!(data(&out)["report"]["all_pass"], false);
        // The other corners pass: tt hot and ss at room temperature.
        let values: Vec<f64> = row["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect();
        assert_eq!(
            values.iter().filter(|v| **v < 1.2e3).count(),
            2,
            "{values:?}"
        );
        assert!(text.contains("Every corner:"), "{text}");
    }
    // A corner the library does not have is named before anything runs.
    let out = call(
        &ws,
        "check_specs",
        json!({"circuit": "rc.cir", "save": false, "specs": "bw = bandwidth_3db(V(out)) >= 1k",
               "corners": {"process": ["res=xyz"]}}),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.text_content()
            .contains("has no section `res_xyz`; it has res_typ, res_wcs, res_bcs"),
        "{}",
        out.text_content()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn optimize_meets_specs_at_every_corner() {
    if !have_ngspice() {
        return;
    }
    let (_dir, ws) = project(&[("toy.lib", CORNER_LIB), ("rc.cir", CORNER_RC)]);
    // 1.2 kHz at ss/125C needs C1 <= 92n; 2.5 kHz at ff/27C needs C1 >= 80n.
    let out = call(
        &ws,
        "optimize",
        json!({"circuit": "rc.cir", "save_specs": false,
               "params": [{"name": "C1", "min": "10n", "max": "1u", "snap": "E24"}],
               "specs": "lo = bandwidth_3db(V(out)) >= 1.2k\nhi = bandwidth_3db(V(out)) <= 2.5k",
               "corners": {"process": ["ss", "ff"], "temperatures": [27, 125]},
               "max_evals": 60}),
    )
    .await;
    let text = ok(&out);
    assert!(text.starts_with("All specs pass at every corner"), "{text}");
    let c = data(&out)["best"]["C1"].as_str().unwrap().to_string();
    assert!(c == "82n" || c == "91n", "{c}\n{text}");
    assert_eq!(data(&out)["report"]["all_pass"], true, "{text}");
    assert_eq!(
        data(&out)["corners"]["corners"].as_array().unwrap().len(),
        4
    );
}

/// Each instance of `rmm` gets its own 1% (sigma) deviation in the mismatch
/// section; the statistical section adds a 2% deviation shared by all.
const STAT_LIB: &str = "* toy statistical library\n.lib res_typ\n.param rnom=1k\n.subckt rmm a b\nR1 a b {rnom}\n.ends rmm\n.endl res_typ\n.lib res_typ_mismatch\n.param rnom=1k\n.subckt rmm a b\nR1 a b {rnom*(1+agauss(0, 0.03, 3))}\n.ends rmm\n.endl res_typ_mismatch\n.lib res_typ_stat\n.param rnom={1k*(1+agauss(0, 0.06, 3))}\n.subckt rmm a b\nR1 a b {rnom*(1+agauss(0, 0.03, 3))}\n.ends rmm\n.endl res_typ_stat\n";

const DIVIDER: &str = "* mismatched divider\n.lib stat.lib res_typ\nV1 in 0 1\nX1 in out rmm\nX2 out 0 rmm\n.op\n.end\n";

const SINGLE: &str = "* one resistor\n.lib stat.lib res_typ\nV1 in 0 1\nX1 in 0 rmm\n.op\n.end\n";

fn spec_stats(out: &ToolOutput, name: &str) -> (f64, f64) {
    let specs = data(out)["specs"].as_array().unwrap();
    let s = specs.iter().find(|s| s["name"] == name).unwrap();
    (s["mean"].as_f64().unwrap(), s["std"].as_f64().unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn mismatch_monte_carlo_spreads_as_the_library_says() {
    if !have_ngspice() {
        return;
    }
    let (_dir, ws) = project(&[
        ("stat.lib", STAT_LIB),
        ("div.cir", DIVIDER),
        ("one.cir", SINGLE),
    ]);
    let specs = "v = value_at(V(out), 0) in 0.495..0.505\ni = value_at(-I(V1), 0) >= 0";
    let run = |circuit: &str, sim: &str, mode: &str, runs: usize, seed: u64| {
        let ws = ws.clone();
        let input = json!({"circuit": circuit, "simulator": sim, "mismatch": mode,
                           "runs": runs, "seed": seed, "specs": specs});
        async move { call(&ws, "monte_carlo", input).await }
    };
    // Xyce is slower to start, so it gets fewer runs and a wider margin
    // for the sampling error of the standard deviation (about 9% at 60).
    let mut sims = vec![("ngspice", 120, 0.25)];
    if have_xyce() {
        sims.push(("xyce", 60, 0.3));
    }
    for (sim, runs, tol) in sims {
        // Two resistors with 1% sigma each: the divider's output spreads by
        // 1/sqrt(2) of that, around 0.5.
        let out = run("div.cir", sim, "local", runs, 3).await;
        let text = ok(&out);
        let (mean, std) = spec_stats(&out, "v");
        assert!((mean - 0.5).abs() < 0.001, "{sim}: mean {mean}\n{text}");
        let want = 0.5 * 0.01 / 2f64.sqrt();
        assert!(
            (std / want - 1.0).abs() < tol,
            "{sim}: std {std} vs {want}\n{text}"
        );
    }
    // The same seed gives the same circuits.
    let a = run("div.cir", "ngspice", "local", 30, 11).await;
    let b = run("div.cir", "ngspice", "local", 30, 11).await;
    assert_eq!(spec_stats(&a, "v"), spec_stats(&b, "v"));
    // Global variation moves every resistor together: one resistor's
    // current spreads by sqrt(2^2 + 1^2) percent.
    let out = run("one.cir", "ngspice", "global", 120, 5).await;
    let text = ok(&out);
    let (mean, std) = spec_stats(&out, "i");
    let want = 1e-3 * (0.02f64.powi(2) + 0.01f64.powi(2)).sqrt();
    assert!(
        (std / want - 1.0).abs() < 0.25,
        "std {std} vs {want} (mean {mean})\n{text}"
    );
    // Mismatch at two temperatures: the same samples at each, a yield each.
    let out = call(
        &ws,
        "monte_carlo",
        json!({"circuit": "div.cir", "mismatch": "local", "runs": 30, "corners": {"temperatures": [27, 85]},
               "specs": "v = value_at(V(out), 0) in 0.495..0.505"}),
    )
    .await;
    let text = ok(&out);
    let corners = data(&out)["corners"].as_array().unwrap();
    assert_eq!(corners.len(), 2, "{text}");
    assert_eq!(corners[0]["corner"], "27C");
    assert_eq!(data(&out)["runs"], 60);
    // Without statistical sections there is nothing for mismatch to switch.
    let (_d2, plain) = project(&[(
        "rc.cir",
        "* rc\nV1 in 0 AC 1\nR1 in out 1k\nC1 out 0 1n\n.ac dec 10 1 1Meg\n.end\n",
    )]);
    let out = call(
        &plain,
        "monte_carlo",
        json!({"circuit": "rc.cir", "mismatch": "local", "specs": "bw = bandwidth_3db(V(out)) >= 1k"}),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.text_content().contains("needs `.lib <file>"),
        "{}",
        out.text_content()
    );
}

/// Corner libraries are read only where the policy lets a simulation read:
/// a path that climbs out, a link to a file outside the project and an
/// absolute path elsewhere are refused with the policy's reason before
/// anything runs, and nothing of those files appears in the answer.
#[tokio::test(flavor = "multi_thread")]
async fn corner_libraries_go_through_the_policy() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("proj");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        dir.path().join("secret.lib"),
        ".lib private_res_section\n.endl\n",
    )
    .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path().join("secret.lib"), root.join("link.lib")).unwrap();
    let rc = |lib: &str| {
        format!(
            "* rc\n.lib {lib} res_typ\nV1 in 0 AC 1\nR1 in out 1k\nC1 out 0 100n\n.ac dec 10 1 1Meg\n.end\n"
        )
    };
    let files = [
        ("up.cir", rc("../secret.lib")),
        ("link.cir", rc("link.lib")),
        ("abs.cir", rc("/etc/passwd")),
    ];
    for (name, text) in &files {
        std::fs::write(root.join(name), text).unwrap();
    }
    let project = Project::open(&root, ProjectOptions::default()).unwrap();
    let ws = Arc::new(Workspace::with_project(project));
    for (name, _) in &files {
        if *name == "link.cir" && cfg!(not(unix)) {
            continue;
        }
        let out = call(
            &ws,
            "check_specs",
            json!({"circuit": name, "save": false, "specs": "bw = bandwidth_3db(V(out)) >= 1k",
                   "corners": {"process": ["ss"]}}),
        )
        .await;
        let text = out.text_content();
        assert!(out.is_error, "{name}: {text}");
        assert!(
            text.contains("outside the project") || text.contains("climbs out"),
            "{name}: {text}"
        );
        assert!(!text.contains("private_res_section"), "{name}: {text}");
    }
}

/// Oversized corner requests fail at once with a clear error.
#[tokio::test(flavor = "multi_thread")]
async fn oversized_corner_requests_fail_fast() {
    let (_dir, ws) = project(&[(
        "rc.cir",
        "* rc\nV1 in 0 AC 1\nR1 in out 1k\nC1 out 0 100n\n.ac dec 10 1 1Meg\n.end\n",
    )]);
    let temps: Vec<i64> = (0..10_000).collect();
    let started = std::time::Instant::now();
    let out = call(
        &ws,
        "check_specs",
        json!({"circuit": "rc.cir", "save": false, "specs": "bw = bandwidth_3db(V(out)) >= 1k",
               "corners": {"temperatures": temps}}),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.text_content().contains("at most 64 values"),
        "{}",
        out.text_content()
    );
    let temps: Vec<i64> = (0..60).collect();
    let out = call(
        &ws,
        "monte_carlo",
        json!({"circuit": "rc.cir", "tolerances": [{"target": "R1", "tol_pct": 1}],
               "specs": "bw = bandwidth_3db(V(out)) >= 1k",
               "corners": {"temperatures": temps, "supplies": [{"target": "V1", "values": [0, 1, 2, 3]}]}}),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.text_content()
            .contains("240 corners is more than the limit of 200"),
        "{}",
        out.text_content()
    );
    // Runs or candidates times corners over the limit, asked for outright.
    let out = call(
        &ws,
        "monte_carlo",
        json!({"circuit": "rc.cir", "tolerances": [{"target": "R1", "tol_pct": 1}],
               "specs": "bw = bandwidth_3db(V(out)) >= 1k", "runs": 300,
               "corners": {"temperatures": [27, 85]}}),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.text_content().contains("at most 250 runs"),
        "{}",
        out.text_content()
    );
    let out = call(
        &ws,
        "optimize",
        json!({"circuit": "rc.cir", "save_specs": false,
               "params": [{"name": "C1", "min": "10n", "max": "1u"}],
               "specs": "bw = bandwidth_3db(V(out)) >= 1k", "max_evals": 300,
               "corners": {"temperatures": [-40, 0, 27, 85, 125, 150, 175, 200, 225, 250]}}),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.text_content().contains("at most 200 (max_evals)"),
        "{}",
        out.text_content()
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}
