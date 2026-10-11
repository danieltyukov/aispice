//! The template library through the `templates` tool, and the proof behind
//! "verified": every template, copied into a project the way an agent would,
//! netlists without warnings and meets every spec in its own table on
//! ngspice, with room to spare. The simulation test is skipped when ngspice
//! is missing. IC templates need their PDK and are simulated in
//! `templates_ihp.rs`.

use aispice_agent::{Tool, ToolContext, ToolOutput};
use aispice_sim::spec::{evaluate, parse_specs};
use aispice_tools::tools::Templates;
use aispice_tools::{Project, ProjectOptions, RunMods, Workspace, templates};
use serde_json::{Value, json};
use std::sync::Arc;

fn workspace() -> (tempfile::TempDir, Arc<Workspace>) {
    let dir = tempfile::tempdir().unwrap();
    let project = Project::open(dir.path(), ProjectOptions::default()).unwrap();
    (dir, Arc::new(Workspace::with_project(project)))
}

async fn call(ws: &Arc<Workspace>, input: Value) -> ToolOutput {
    Templates { ws: ws.clone() }
        .call(&ToolContext::default(), input)
        .await
}

fn have_ngspice() -> bool {
    std::process::Command::new("ngspice")
        .arg("--version")
        .output()
        .is_ok()
}

#[tokio::test]
async fn list_filters_and_returns_items() {
    let (_dir, ws) = workspace();
    let all = call(&ws, json!({"action": "list"})).await;
    assert!(!all.is_error, "{}", all.text_content());
    let data = all.data.unwrap();
    assert_eq!(data["kind"], "templates");
    let items = data["items"].as_array().unwrap();
    assert_eq!(items.len(), templates::all().len());
    for key in ["id", "title", "category", "description"] {
        assert!(items[0][key].is_string(), "{key}");
    }

    let amps = call(&ws, json!({"action": "list", "category": "amplifiers"})).await;
    let items = amps.data.unwrap()["items"].as_array().unwrap().clone();
    assert!(items.len() >= 4);
    assert!(items.iter().all(|i| i["category"] == "amplifiers"));

    let found = call(&ws, json!({"action": "list", "query": "photodiode"})).await;
    assert!(
        found.text_content().contains("transimpedance_amplifier"),
        "{}",
        found.text_content()
    );

    let bad = call(&ws, json!({"action": "list", "category": "plumbing"})).await;
    assert!(bad.is_error && bad.text_content().contains("filters"));
}

#[tokio::test]
async fn show_gives_equations_specs_and_the_schematic() {
    let (_dir, ws) = workspace();
    let out = call(&ws, json!({"action": "show", "id": "sallen_key_lowpass"})).await;
    let text = out.text_content();
    assert!(!out.is_error, "{text}");
    assert!(text.contains("f0 = 1 / (2 pi sqrt(R1 R2 C1 C2))"), "{text}");
    assert!(
        text.contains("f3db = bandwidth_3db(V(out)/V(in))"),
        "{text}"
    );
    assert!(text.contains("U1"), "{text}");
    let data = out.data.unwrap();
    assert_eq!(data["kind"], "template");
    assert_eq!(data["action"], "show");
    assert_eq!(data["template"]["id"], "sallen_key_lowpass");
    assert_eq!(data["template"]["analysis"], ".ac dec 50 10 100k");
    assert!(data["template"]["parameters"].as_array().unwrap().len() >= 4);
    assert_eq!(data["summary"]["components"].as_array().unwrap().len(), 6);

    let missing = call(&ws, json!({"action": "show", "id": "flux_capacitor"})).await;
    assert!(missing.is_error && missing.text_content().contains("rc_lowpass"));
    let no_id = call(&ws, json!({"action": "show"})).await;
    assert!(no_id.is_error);
}

#[tokio::test]
async fn use_creates_the_circuit_and_specs_and_never_overwrites() {
    let (dir, ws) = workspace();
    let out = call(
        &ws,
        json!({"action": "use", "id": "rc_lowpass", "circuit": "filter.asc"}),
    )
    .await;
    assert!(!out.is_error, "{}", out.text_content());
    let data = out.data.unwrap();
    assert_eq!(data["kind"], "template");
    assert_eq!(data["action"], "use");
    assert_eq!(data["circuit"], "filter.asc");
    assert_eq!(data["specs_file"], "filter.specs");

    // The schematic is the template, byte for byte, and its creation is in
    // the history.
    let t = templates::find("rc_lowpass").unwrap();
    assert_eq!(std::fs::read(dir.path().join("filter.asc")).unwrap(), t.asc);
    let specs = std::fs::read_to_string(dir.path().join("filter.specs")).unwrap();
    assert_eq!(parse_specs(&specs).unwrap(), parse_specs(&t.specs).unwrap());
    let p = ws.project().unwrap();
    let (snaps, _) = p.history("filter.asc").unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].summary, "Created from template rc_lowpass");

    // A second use to the same name changes nothing.
    let before = std::fs::read(dir.path().join("filter.asc")).unwrap();
    let again = call(
        &ws,
        json!({"action": "use", "id": "rc_highpass", "circuit": "filter.asc"}),
    )
    .await;
    assert!(again.is_error && again.text_content().contains("already exists"));
    assert_eq!(
        std::fs::read(dir.path().join("filter.asc")).unwrap(),
        before
    );

    // An existing specs file is not overwritten either, and blocks the
    // schematic too.
    std::fs::write(dir.path().join("mine.specs"), "gain = max(V(out)) >= 1\n").unwrap();
    let clash = call(
        &ws,
        json!({"action": "use", "id": "rc_highpass", "circuit": "mine.asc"}),
    )
    .await;
    assert!(clash.is_error, "{}", clash.text_content());
    assert!(!dir.path().join("mine.asc").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("mine.specs")).unwrap(),
        "gain = max(V(out)) >= 1\n"
    );

    for bad in [
        json!({"action": "use", "id": "rc_lowpass", "circuit": "../escape.asc"}),
        json!({"action": "use", "id": "rc_lowpass", "circuit": "/tmp/escape.asc"}),
        json!({"action": "use", "id": "rc_lowpass", "circuit": "notes.txt"}),
        json!({"action": "use", "id": "rc_lowpass"}),
        json!({"action": "use", "circuit": "x.asc"}),
    ] {
        let out = call(&ws, bad.clone()).await;
        assert!(out.is_error, "{bad}: {}", out.text_content());
    }
}

/// A project beside a fake IHP PDK: registered or not, and used by the
/// project or not.
fn pdk_workspace(register: bool, use_it: bool) -> (tempfile::TempDir, Arc<Workspace>) {
    use aispice_tools::pdk::{PROJECT_FILE, Registry};
    let dir = tempfile::tempdir().unwrap();
    aispice_sim::pdk::fake_ihp(dir.path());
    let mut pdks = Registry::default();
    if register {
        pdks.add("sg13", &dir.path().join("IHP-Open-PDK")).unwrap();
    }
    let root = dir.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    if use_it {
        std::fs::write(root.join(PROJECT_FILE), "pdk = \"sg13\"\n").unwrap();
    }
    let ws = Workspace::new();
    ws.runner.set_config(aispice_tools::RunnerConfig {
        pdks,
        ..Default::default()
    });
    ws.set_project(Project::open(&root, ProjectOptions::default()).unwrap());
    (dir, Arc::new(ws))
}

/// An IC template is drawn in its PDK's devices: `use` refuses it, writing
/// nothing, until the project uses a registered PDK of that kind, and says
/// what to do. Then it is copied with its corners and layout generator.
#[tokio::test]
async fn an_ic_template_needs_a_project_on_its_pdk() {
    let input = json!({"action": "use", "id": "ota5t", "circuit": "ota.asc"});
    for (register, hint) in [
        (false, "no ihp-sg13g2 PDK is registered"),
        (
            true,
            "make it use the registered sg13 first (pdk tool, action use)",
        ),
    ] {
        let (dir, ws) = pdk_workspace(register, false);
        let out = call(&ws, input.clone()).await;
        let text = out.text_content();
        assert!(out.is_error, "{text}");
        assert!(text.contains("this project uses no PDK"), "{text}");
        assert!(text.contains(hint), "{text}");
        assert!(text.contains("nothing was written"), "{text}");
        let root = dir.path().join("project");
        assert!(!root.join("ota.asc").exists() && !root.join("ota.specs").exists());
    }

    let (dir, ws) = pdk_workspace(true, true);
    let out = call(&ws, input).await;
    let text = out.text_content();
    assert!(!out.is_error, "{text}");
    assert!(text.contains("PDK: ihp-sg13g2."), "{text}");
    assert!(
        text.contains(
            r#"as corners: {"process":["tt","ss","ff","sf","fs"],"temperatures":[-40.0,27.0,125.0]"#
        ),
        "{text}"
    );
    assert!(
        text.contains(r#"Layout: the ota5t generator draws this block, with the roles {"bias":"M6","load":["M3","M4"],"pair":["M1","M2"],"tail":"M5"}."#),
        "{text}"
    );
    let t = templates::find("ota5t").unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("project/ota.asc")).unwrap(),
        t.asc
    );
    // A non-IC template needs no PDK, whatever the project uses.
    let plain = call(
        &ws,
        json!({"action": "use", "id": "rc_lowpass", "circuit": "rc.asc"}),
    )
    .await;
    assert!(!plain.is_error, "{}", plain.text_content());
}

#[tokio::test]
async fn ic_templates_list_and_show_their_pdk() {
    let (_dir, ws) = workspace();
    let out = call(&ws, json!({"action": "list", "category": "ic"})).await;
    let text = out.text_content();
    assert!(text.starts_with("6 template(s):"), "{text}");
    assert!(
        text.contains("  ota5t [ic, ihp-sg13g2] Five-transistor OTA"),
        "{text}"
    );
    let items = out.data.unwrap()["items"].as_array().unwrap().clone();
    assert!(items.iter().all(|i| i["pdk"] == "ihp-sg13g2"));
    let found = call(&ws, json!({"action": "list", "query": "sg13g2 comparator"})).await;
    assert!(
        found.text_content().contains("comparator [ic"),
        "{}",
        found.text_content()
    );

    let shown = call(&ws, json!({"action": "show", "id": "bandgap"})).await;
    let text = shown.text_content();
    assert!(!shown.is_error, "{text}");
    assert!(text.contains("on ngspice;"), "{text}");
    assert!(!text.contains("Layout:"), "{text}");
    let data = shown.data.unwrap();
    assert_eq!(data["template"]["pdk"], "ihp-sg13g2");
    assert_eq!(data["template"]["simulators"], json!(["ngspice"]));
    assert_eq!(data["template"]["corners"]["supplies"][0]["target"], "VDD");
    let shown = call(&ws, json!({"action": "show", "id": "two_stage"})).await;
    let layout = &shown.data.unwrap()["template"]["layout"];
    assert_eq!(layout["generator"], "two_stage");
    assert_eq!(layout["roles"]["cc"], "C1");
}

struct DenyAll;

#[async_trait::async_trait]
impl aispice_tools::Approver for DenyAll {
    async fn approve(&self, _circuit: &str, _summary: &str, _diff: &str) -> bool {
        false
    }
}

#[tokio::test]
async fn declined_approval_writes_nothing() {
    let (dir, ws) = workspace();
    ws.set_hooks(aispice_tools::Hooks {
        approver: Some(Arc::new(DenyAll)),
        after_save: None,
    });
    let out = call(
        &ws,
        json!({"action": "use", "id": "ce_amplifier", "circuit": "amp.asc"}),
    )
    .await;
    assert!(out.text_content().contains("declined"));
    assert!(!dir.path().join("amp.asc").exists());
    assert!(!dir.path().join("amp.specs").exists());
}

/// How close a spec value may sit to its limits: a two-sided spec must be
/// inside the middle 70% of its window, a one-sided one must pass by at least
/// 5% of its limit.
fn comfortable(value: f64, min: Option<f64>, max: Option<f64>) -> bool {
    match (min, max) {
        (Some(lo), Some(hi)) => {
            let edge = 0.15 * (hi - lo);
            value >= lo + edge && value <= hi - edge
        }
        _ => aispice_sim::spec::margin(value, min, max).is_some_and(|m| m >= 0.05),
    }
}

/// With `AISPICE_LTSPICE_TESTS=1` and LTspice installed, every template is
/// netlisted by LTspice itself and must match aispice's netlist, apart from
/// the default model cards LTspice adds for every semiconductor.
#[tokio::test]
async fn template_netlists_match_live_ltspice() {
    use aispice_core::netlist::{Line, Netlist, compare::compare, parse};
    use aispice_sim::backend::{Ltspice, Simulator};
    if std::env::var("AISPICE_LTSPICE_TESTS").ok().as_deref() != Some("1") {
        eprintln!("skipped: set AISPICE_LTSPICE_TESTS=1 to netlist with LTspice");
        return;
    }
    let lt = Ltspice {
        headless: Some(true),
        ..Default::default()
    };
    if !lt.detect().await.found {
        eprintln!("skipped: LTspice not found");
        return;
    }
    fn without_default_cards(mut n: Netlist) -> Netlist {
        n.items.retain(|item| match item {
            Line::Directive { text } => {
                let t = text.trim().to_ascii_lowercase();
                let words: Vec<&str> = t.split_whitespace().collect();
                !(words.len() == 3 && words[0] == ".model" && words[1] == words[2])
            }
            _ => true,
        });
        n
    }
    let dir = tempfile::tempdir().unwrap();
    let lib = aispice_core::symbol::SymbolLibrary::builtin_only();
    let mut failures = Vec::new();
    // IC templates use the PDK's device symbols, which LTspice does not have.
    for t in templates::all().iter().filter(|t| t.pdk.is_none()) {
        let asc = dir.path().join(format!("{}.asc", t.id));
        std::fs::write(&asc, t.asc).unwrap();
        let live = lt
            .netlist_asc(&asc)
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", t.id));
        let (ours, _) = aispice_core::netlist::build(&t.schematic(), &lib, &t.id);
        let theirs = without_default_cards(parse(&live));
        if let Err(p) = compare(&without_default_cards(ours.netlist), &theirs) {
            failures.push(format!("{}: {p:?}\n{live}", t.id));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[tokio::test(flavor = "multi_thread")]
async fn every_template_meets_its_specs_on_ngspice() {
    if !have_ngspice() {
        eprintln!("skipped: ngspice not installed");
        return;
    }
    let (dir, ws) = workspace();
    let p = ws.project().unwrap();
    let mut failures = Vec::new();
    // IC templates need their PDK: templates_ihp.rs checks them at their
    // corners.
    for t in templates::all().iter().filter(|t| t.pdk.is_none()) {
        let circuit = format!("{}.asc", t.id);
        let out = call(
            &ws,
            json!({"action": "use", "id": t.id, "circuit": circuit}),
        )
        .await;
        assert!(!out.is_error, "{}: {}", t.id, out.text_content());

        let (_, _, warnings) = ws.runner.netlist_for(&p, &circuit).unwrap();
        assert!(
            warnings.is_empty(),
            "{}: netlist warnings {warnings:?}",
            t.id
        );
        let run = ws
            .runner
            .run_circuit(
                &p,
                &circuit,
                Some("ngspice"),
                &RunMods::default(),
                &Default::default(),
            )
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", t.id));
        assert!(
            run.output.errors.is_empty(),
            "{}: {:?}",
            t.id,
            run.output.errors
        );

        let specs_text =
            std::fs::read_to_string(dir.path().join(format!("{}.specs", t.id))).unwrap();
        let specs = parse_specs(&specs_text).unwrap();
        let report = evaluate(&specs, &run.output.datasets);
        println!("{}:", t.id);
        for row in &report.rows {
            println!("  {}", row.display);
            let ok = row.pass && row.value.is_some_and(|v| comfortable(v, row.min, row.max));
            if !ok {
                failures.push(format!("{}: {}", t.id, row.display));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "specs failing or too close to a limit:\n{}",
        failures.join("\n")
    );
}
