//! A MIM capacitor as a row of its own: IHP's `cmim` pcell with both plates
//! brought down to Metal2, where the router joins them to tracks like any
//! other terminal.
//!
//! The bottom plate (Metal5) gets a tab below the capacitor with a via
//! stack down to Metal2; the top plate (TopMetal1) gets a tab that reaches
//! a TopVia1 on a Metal5 island beside it, with the same stack below.
//! Measured with IHP's decks: TopMetal1 1.64 um wide (`TM1.a`), TopVia1
//! 0.42 um enclosed 0.42 um by TopMetal1 and 0.1 um by Metal5, and every
//! metal pad at least 0.144 um2.

use super::plan::{Layer, PcellInst, Plan, Rect};
use super::row::{Kind, RowLayout, RowSummary, Terminal};
use super::rules::VIA;
use std::collections::BTreeMap;

/// Smallest side the generator draws; the top plate's tab is wider than
/// IHP's 1.14 um minimum.
pub const MIN_SIDE: i64 = 2000;
/// Largest MIM area per device (`MIM.g`, 5625 um2), in nm2.
pub const MAX_AREA: i64 = 5_625_000_000;

/// A capacitor to place.
#[derive(Debug, Clone, PartialEq)]
pub struct CapSpec {
    pub name: String,
    pub w: i64,
    pub l: i64,
    /// The net on the top plate (the device's first pin, PLUS).
    pub top: String,
    /// The net on the bottom plate (MINUS).
    pub bottom: String,
}

/// A stack of Via4, Via3 and Via2 at a point, with Metal4, Metal3 and
/// Metal2 pads; returns the Via2, which the router's stub covers.
fn stack(plan: &mut Plan, x: i64, y: i64, net: &str) -> Rect {
    for v in [Layer::Via4, Layer::Via3, Layer::Via2] {
        plan.add(v, Rect::centred(x, y, VIA, VIA), Some(net));
    }
    for m in [Layer::Metal4, Layer::Metal3, Layer::Metal2] {
        plan.add(m, Rect::centred(x, y, 300, 500), Some(net));
    }
    Rect::centred(x, y, VIA, VIA)
}

pub fn build(spec: &CapSpec) -> Result<RowLayout, String> {
    if spec.w < MIN_SIDE || spec.l < MIN_SIDE {
        return Err(format!(
            "{} is {} x {} um; the generator draws MIM capacitors of at least {} um a side",
            spec.name,
            spec.w as f64 / 1000.0,
            spec.l as f64 / 1000.0,
            MIN_SIDE as f64 / 1000.0
        ));
    }
    if spec.w * spec.l > MAX_AREA {
        return Err(format!(
            "{} is larger than IHP's 5625 um2 per MIM device; split it",
            spec.name
        ));
    }
    let (w, l) = (spec.w, spec.l);
    let mut plan = Plan::default();
    plan.instances.push(PcellInst {
        pcell: "cmim".into(),
        w,
        l,
        ng: 1,
        x: 0,
        y: 0,
        device: spec.name.clone(),
    });
    let mut pcell_nets = BTreeMap::new();
    pcell_nets.insert((-600, -600), spec.bottom.clone());
    pcell_nets.insert((-40, -40), spec.top.clone());

    // Bottom plate: a Metal5 tab under the left edge.
    let bx = -300;
    plan.add(
        Layer::Metal5,
        Rect::new(-600, -1800, 0, -600),
        Some(&spec.bottom),
    );
    let bottom_via = stack(&mut plan, bx, -1400, &spec.bottom);

    // Top plate: a TopMetal1 tab under the right edge down to a TopVia1 on
    // a Metal5 island.
    let tx = w + 40 - 820;
    plan.add(
        Layer::TopMetal1,
        Rect::new(tx - 820, -3240, tx + 820, -40),
        Some(&spec.top),
    );
    plan.add(
        Layer::TopVia1,
        Rect::centred(tx, -2600, 420, 420),
        Some(&spec.top),
    );
    plan.add(
        Layer::Metal5,
        Rect::centred(tx, -2600, 620, 620),
        Some(&spec.top),
    );
    let top_via = stack(&mut plan, tx, -2600, &spec.top);

    let bbox = Rect::new(-600, -3240, (w + 600).max(tx + 820), l + 600);
    Ok(RowLayout {
        plan,
        down: vec![
            Terminal {
                x: bx,
                via: bottom_via,
                net: spec.bottom.clone(),
            },
            Terminal {
                x: tx,
                via: top_via,
                net: spec.top.clone(),
            },
        ],
        up: Vec::new(),
        ring: bbox,
        bulk: String::new(),
        dummies: Vec::new(),
        summary: RowSummary {
            name: spec.name.clone(),
            kind: Kind::Mim,
            finger_width_um: w as f64 / 1000.0,
            length_um: l as f64 / 1000.0,
            fingers: BTreeMap::new(),
            order: Vec::new(),
        },
        pcell_nets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capacitor_row_is_clean_by_construction() {
        let spec = CapSpec {
            name: "C1".into(),
            w: 10_000,
            l: 10_000,
            top: "out1".into(),
            bottom: "out".into(),
        };
        let row = build(&spec).unwrap();
        assert_eq!(row.down.len(), 2);
        assert_eq!(row.down[0].net, "out");
        let errors = row.plan.spacing_errors(&row.pcell_nets);
        assert!(errors.is_empty(), "{errors:#?}");
        let small = CapSpec {
            w: 1500,
            ..spec.clone()
        };
        assert!(build(&small).err().unwrap().contains("at least 2 um"));
        let huge = CapSpec {
            w: 80_000,
            l: 80_000,
            ..spec
        };
        assert!(build(&huge).err().unwrap().contains("5625"));
    }
}
