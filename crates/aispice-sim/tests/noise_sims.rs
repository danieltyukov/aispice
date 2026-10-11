//! `.noise` on the real simulators, checked against physics: a resistor's
//! thermal noise is sqrt(4kTR), and a common-source stage's input-referred
//! noise follows from its transconductance. Skips per simulator when it is
//! missing; LTspice needs `AISPICE_LTSPICE_TESTS=1` and Spectre the
//! variables described in `common`.

mod common;

use aispice_core::netlist::parse;
use aispice_sim::backend::{SimId, SimOutput, Simulator};
use aispice_sim::dataset::Quantity;
use aispice_sim::dialect::translate;
use aispice_sim::measure::{Measure, measure};
use common::*;

const K: f64 = 1.380_649e-23;
/// 27 C, every simulator's default temperature.
const T: f64 = 300.15;

const DIVIDER: &str = "* noise of a 1k/1k divider\nV1 in 0 DC 0 AC 1\nR1 in out 1k\nR2 out 0 1k\n.noise V(out) V1 dec 10 1 1Meg\n.end\n";

/// A level 1 NMOS without flicker noise and with no output conductance, so
/// the hand calculation is exact: gm = KP W/L (VGS - VTO) = 0.4 mS.
const COMMON_SOURCE: &str = "* common-source noise\nVDD vdd 0 1.8\nVG g 0 DC 0.7 AC 1\nRD vdd d 10k\nM1 d g 0 0 nmos1 W=10u L=1u\n.model nmos1 NMOS(LEVEL=1 VTO=0.5 KP=200u LAMBDA=0 KF=0)\n.noise V(d) VG dec 10 10 100k\n.end\n";

async fn simulators() -> Vec<(SimId, Box<dyn Simulator>)> {
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
    if let Some(sp) = spectre().await {
        sims.push((SimId::Spectre, Box::new(sp)));
    }
    sims
}

async fn run_on(id: SimId, sim: &dyn Simulator, deck: &str, name: &str) -> SimOutput {
    let tr = translate(&parse(deck), id.target());
    assert!(tr.unsupported.is_empty(), "{id}: {:?}", tr.unsupported);
    let out = run(sim, &tr.text, name)
        .await
        .unwrap_or_else(|e| panic!("{id}: {e}\n{}", tr.text));
    assert!(out.errors.is_empty(), "{id}: {:?}", out.errors);
    out
}

fn value(id: SimId, out: &SimOutput, text: &str) -> f64 {
    let r = measure("m", &Measure::parse(text).unwrap(), &out.datasets);
    r.value.unwrap_or_else(|| {
        panic!(
            "{id}: {text}: {:?}; datasets: {:?}",
            r.note,
            out.datasets
                .iter()
                .map(|d| (d.plotname.clone(), d.names()))
                .collect::<Vec<_>>()
        )
    })
}

#[tokio::test]
async fn resistor_noise_is_4ktr() {
    let want = (4.0 * K * T * 500.0).sqrt();
    for (id, sim) in simulators().await {
        let out = run_on(id, sim.as_ref(), DIVIDER, "divider").await;
        let on = value(id, &out, "output_noise_at(1k)");
        let inn = value(id, &out, "input_noise_at(1k)");
        let rms = value(id, &out, "output_noise_rms(1, 1Meg)");
        eprintln!("{id}: onoise {on:e}, inoise {inn:e}, rms {rms:e}");
        assert!(rel(on, want) < 0.02, "{id}: {on} vs {want}");
        // The divider halves the input, so the input-referred noise is
        // twice the output noise.
        assert!(rel(inn, 2.0 * want) < 0.02, "{id}: {inn}");
        assert!(rel(rms, want * (1e6f64 - 1.0).sqrt()) < 0.02, "{id}: {rms}");
        let ds = out
            .datasets
            .iter()
            .find(|d| d.vector("V(inoise)").is_some())
            .unwrap();
        assert_eq!(
            ds.vector("V(inoise)").unwrap().quantity,
            Quantity::Voltage,
            "{id}"
        );
    }
}

#[tokio::test]
async fn current_source_input_is_a_current_density() {
    let deck = "* transimpedance noise\nI1 0 out DC 0 AC 1\nR1 out 0 1k\n.noise V(out) I1 dec 10 1 1k\n.end\n";
    for (id, sim) in simulators().await {
        let out = run_on(id, sim.as_ref(), deck, "tia").await;
        let r = measure(
            "in",
            &Measure::parse("input_noise_at(100)").unwrap(),
            &out.datasets,
        );
        // sqrt(4kT/R): the resistor's noise current.
        let want = (4.0 * K * T / 1e3).sqrt();
        let v = r.value.unwrap_or_else(|| panic!("{id}: {:?}", r.note));
        assert!(rel(v, want) < 0.02, "{id}: {v} vs {want}");
        assert_eq!(r.unit, "A/√Hz", "{id}");
    }
}

#[tokio::test]
async fn common_source_noise_matches_the_hand_calculation() {
    let gm = 200e-6 * 10.0 * 0.2;
    let rd = 10e3;
    let input = (8.0 * K * T / (3.0 * gm) + 4.0 * K * T / (gm * gm * rd)).sqrt();
    let output = input * gm * rd;
    for (id, sim) in simulators().await {
        let out = run_on(id, sim.as_ref(), COMMON_SOURCE, "cs").await;
        let inn = value(id, &out, "input_noise_at(1k)");
        let on = value(id, &out, "output_noise_at(1k)");
        eprintln!("{id}: input {inn:e} (want {input:e}), output {on:e} (want {output:e})");
        assert!(rel(inn, input) < 0.10, "{id}: {inn} vs {input}");
        assert!(rel(on, output) < 0.10, "{id}: {on} vs {output}");
    }
}
