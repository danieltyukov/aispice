//! Corners and rejection on the real simulators: a temperature corner must
//! reach every simulator (Xyce ignores `.temp`, so the dialect rewrites it),
//! a process corner must select the right section of a corner library, which
//! ngspice and LTspice can only do through aispice's inlining and Spectre,
//! on another machine, only with the library inlined in the deck, and the
//! derived AC run of a PSRR measurement must give the known answer. Skips
//! per simulator when it is missing; LTspice needs `AISPICE_LTSPICE_TESTS=1`
//! and Spectre the variables described in `common`.

mod common;

use aispice_core::netlist::{Policy, canonical_includes, parse};
use aispice_sim::backend::{SimId, Simulator};
use aispice_sim::corner::{self, CornerSet};
use aispice_sim::dialect::translate;
use aispice_sim::library::{LibraryFiles, expand_lib_sections, inline_includes};
use aispice_sim::measure::{Measure, measure};
use aispice_sim::stimulus;
use common::*;
use std::f64::consts::PI;

/// An RC lowpass whose resistor warms by 0.2% per degree from 27 C.
const WARM_RC: &str =
    "* warm rc\nV1 in 0 AC 1\nR1 in out 1k tc1=2m\nC1 out 0 100n\n.ac dec 50 10 1Meg\n.end\n";

const LIBRARY: &str = "* resistor corners\n.lib res_typ\n.param rsh=1k\n.endl res_typ\n.lib res_wcs\n.param rsh=1.2k\n.endl res_wcs\n.lib res_bcs\n.param rsh=800\n.endl res_bcs\n";

async fn local_simulators() -> Vec<(SimId, Box<dyn Simulator>)> {
    let mut sims: Vec<(SimId, Box<dyn Simulator>)> = Vec::new();
    if let Some(ng) = ngspice().await {
        sims.push((SimId::Ngspice, Box::new(ng)));
    }
    if let Some(x) = xyce().await {
        sims.push((SimId::Xyce, Box::new(x)));
    }
    if let Some(lt) = ltspice().await {
        sims.push((SimId::Ltspice, Box::new(lt)));
    }
    sims
}

async fn bandwidth(id: SimId, sim: &dyn Simulator, deck: &str) -> f64 {
    let tr = translate(&parse(deck), id.target());
    assert!(tr.unsupported.is_empty(), "{id}: {:?}", tr.unsupported);
    let out = run(sim, &tr.text, "corner")
        .await
        .unwrap_or_else(|e| panic!("{id}: {e}\n{}", tr.text));
    let r = measure(
        "bw",
        &Measure::parse("bandwidth_3db(V(out))").unwrap(),
        &out.datasets,
    );
    r.value
        .unwrap_or_else(|| panic!("{id}: {:?}\n{}", r.note, tr.text))
}

#[tokio::test]
async fn temperature_corners_reach_every_simulator() {
    let mut sims = local_simulators().await;
    if let Some(sp) = spectre().await {
        sims.push((SimId::Spectre, Box::new(sp)));
    }
    let set = CornerSet {
        temperatures: vec![-40.0, 125.0],
        ..Default::default()
    };
    let variants = corner::variants(&parse(WARM_RC), &set, &corner::no_lookup).unwrap();
    for (id, sim) in &sims {
        for ((name, n), t) in variants.iter().zip([-40.0, 125.0]) {
            let deck = aispice_core::netlist::write(n);
            let got = bandwidth(*id, sim.as_ref(), &deck).await;
            let r = 1e3 * (1.0 + 2e-3 * (t - 27.0));
            let want = 1.0 / (2.0 * PI * r * 100e-9);
            eprintln!("{id} {name}: {got:.1} Hz (want {want:.1})");
            assert!(rel(got, want) < 0.005, "{id} {name}: {got} vs {want}");
        }
    }
}

#[tokio::test]
async fn process_corners_select_library_sections() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("corner.lib"), LIBRARY).unwrap();
    let deck = "* corner rc\n.lib corner.lib res_typ\nV1 in 0 AC 1\nR1 in out {rsh}\nC1 out 0 100n\n.ac dec 50 10 1Meg\n.end\n";
    let set = CornerSet {
        process: vec!["tt".into(), "ss".into(), "ff".into()],
        ..Default::default()
    };
    let policy = Policy {
        base_dir: dir.path().to_path_buf(),
        allowed_dirs: vec![dir.path().to_path_buf()],
        allow_control_blocks: false,
        trusted_dirs: Vec::new(),
    };
    let files = LibraryFiles::new(policy.clone(), dir.path());
    let lookup = |file: &str| files.sections(file);
    let variants = corner::variants(&parse(deck), &set, &lookup).unwrap();
    let mut sims = local_simulators().await;
    if let Some(sp) = spectre().await {
        sims.push((SimId::Spectre, Box::new(sp)));
    }
    for (id, sim) in sims {
        for ((name, n), r) in variants.iter().zip([1e3, 1.2e3, 800.0]) {
            let mut text = aispice_core::netlist::write(n);
            // As the runner does: ngspice in its LTspice mode and LTspice
            // cannot select a section themselves, and Spectre's server has
            // none of the files.
            let files = LibraryFiles::new(policy.clone(), dir.path());
            if matches!(id, SimId::Ngspice | SimId::Ltspice) {
                text = expand_lib_sections(&text, &files).unwrap();
            } else if id == SimId::Spectre {
                text = inline_includes(&text, &files).unwrap().text;
            }
            // And the simulator runs elsewhere, so the library is named by
            // its full path.
            let text = canonical_includes(&text, &policy);
            let got = bandwidth(id, sim.as_ref(), &text).await;
            let want = 1.0 / (2.0 * PI * r * 100e-9);
            eprintln!("{id} {name}: {got:.1} Hz (want {want:.1})");
            assert!(rel(got, want) < 0.005, "{id} {name}: {got} vs {want}");
        }
    }
}

/// `out = 100 V(inp) + 0.01 V(vdd)`: 80 dB of supply rejection.
#[tokio::test]
async fn psrr_from_a_derived_run_on_every_simulator() {
    let deck = "* psrr\nVDD vdd 0 DC 1.8\nVin inp 0 DC 0 AC 1\nE1 out x inp 0 100\nE2 x 0 vdd 0 0.01\nRl out 0 10k\n.ac dec 10 1 1Meg\n.end\n";
    let mut sims = local_simulators().await;
    if let Some(sp) = spectre().await {
        sims.push((SimId::Spectre, Box::new(sp)));
    }
    let main = parse(deck);
    let derived = stimulus::ac_on(&main, "VDD").unwrap();
    for (id, sim) in sims {
        let mut datasets = Vec::new();
        for (n, mark) in [(&main, false), (&derived, true)] {
            let tr = translate(n, id.target());
            let out = run(sim.as_ref(), &tr.text, "psrr")
                .await
                .unwrap_or_else(|e| panic!("{id}: {e}\n{}", tr.text));
            for mut d in out.datasets {
                if mark {
                    stimulus::mark(&mut d, "VDD");
                }
                datasets.push(d);
            }
        }
        let r = measure(
            "psrr",
            &Measure::parse("psrr_db_at(V(out), VDD, 1k)").unwrap(),
            &datasets,
        );
        let v = r.value.unwrap_or_else(|| panic!("{id}: {:?}", r.note));
        eprintln!("{id}: psrr {v} dB");
        assert!((v - 80.0).abs() < 1e-3, "{id}: {v}");
    }
}
