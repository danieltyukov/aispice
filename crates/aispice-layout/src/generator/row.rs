//! One row of transistors of one type, in local coordinates with the first
//! island's Activ at the origin.
//!
//! Every device is split into units of two fingers that share the drain in
//! the middle (`S g D g S`), so neighbouring units abut on a source column.
//! Devices that share a source net form an island: their units are
//! interdigitated or placed about a common centroid, with dummy fingers at
//! both ends (gate to the off rail, both sides to the source). Each run of
//! fingers of one device is one IHP pcell; the planner adds what the pcell
//! leaves out: a contacted poly pad above every gate, a Via1 on every
//! source and drain stripe, and the guard ring.
//!
//! Gate terminals leave the row upward on Metal2 and source, drain and ring
//! terminals downward; the router joins them to tracks.

use super::plan::{Layer, PcellInst, Plan, Rect};
use super::rules::*;
use super::{ExtraDevice, Params, Pattern};
use serde::Serialize;
use std::collections::BTreeMap;

/// Widest finger the planner chooses by itself.
const MAX_FINGER_W: i64 = 2000;
/// Activ gap between two islands of a row.
const ISLAND_GAP: i64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Nmos,
    Pmos,
}

impl Kind {
    pub fn pcell(self) -> &'static str {
        match self {
            Kind::Nmos => "nmos",
            Kind::Pmos => "pmos",
        }
    }
}

/// A schematic transistor placed in a row.
#[derive(Debug, Clone, PartialEq)]
pub struct RowDev {
    pub name: String,
    pub model: &'static str,
    /// Total width (w times m), nm.
    pub w: i64,
    pub l: i64,
    pub d: String,
    pub g: String,
    pub s: String,
    pub b: String,
}

impl RowDev {
    /// The terminal that is not `source`.
    fn drain_for(&self, source: &str) -> &str {
        if self.s == source { &self.d } else { &self.s }
    }
}

/// Devices that share a source net and so one strip of Activ.
#[derive(Debug, Clone)]
pub struct Island {
    pub devices: Vec<RowDev>,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct RowSpec {
    /// What the row is, for reports: `pair`, `load`, ...
    pub name: String,
    pub kind: Kind,
    pub islands: Vec<Island>,
    /// The bulk net: the guard ring's net.
    pub bulk: String,
    /// The net dummy gates go to (the bulk rail turns them off).
    pub off: String,
}

/// Where a terminal leaves the row: the Via1 it ends in and its net.
#[derive(Debug, Clone, PartialEq)]
pub struct Terminal {
    pub x: i64,
    pub via: Rect,
    pub net: String,
}

/// How a row was built, for the layout report.
#[derive(Debug, Clone, Serialize)]
pub struct RowSummary {
    pub name: String,
    pub kind: Kind,
    pub finger_width_um: f64,
    pub length_um: f64,
    /// Fingers per schematic device.
    pub fingers: BTreeMap<String, i64>,
    /// Unit order of each island, dummies as `d`.
    pub order: Vec<String>,
}

pub struct RowLayout {
    pub plan: Plan,
    /// Source, drain and ring terminals (leave downward).
    pub down: Vec<Terminal>,
    /// Gate terminals (leave upward).
    pub up: Vec<Terminal>,
    /// Outer box of the guard ring or tap bar.
    pub ring: Rect,
    pub bulk: String,
    pub dummies: Vec<ExtraDevice>,
    pub summary: RowSummary,
    /// Nets of the pcells' stripes and gates, keyed by lower-left corner,
    /// for geometry checks.
    pub pcell_nets: BTreeMap<(i64, i64), String>,
}

/// The finger width for a row: the smallest device gets `fingers` fingers
/// (or the fewest even count that keeps fingers at most 2 um wide), and
/// every device must then have an even number of fingers of that width.
fn finger_width(devs: &[&RowDev], fingers: Option<u32>) -> Result<i64, String> {
    let wmin = devs.iter().map(|d| d.w).min().unwrap_or(0);
    let counts: Vec<i64> = match fingers {
        Some(f) if f == 0 || f % 2 == 1 => {
            return Err(format!(
                "fingers must be a positive even number (got {f}): devices are built from two-finger units"
            ));
        }
        Some(f) => vec![i64::from(f)],
        None => (1..=64).map(|k| 2 * k).collect(),
    };
    for f in counts {
        if wmin % f != 0 || (wmin / f) % GRID != 0 {
            continue;
        }
        let wf = wmin / f;
        if wf < MIN_FINGER_W {
            break;
        }
        if fingers.is_none() && wf > MAX_FINGER_W {
            continue;
        }
        if devs.iter().all(|d| d.w % wf == 0 && (d.w / wf) % 2 == 0) {
            return Ok(wf);
        }
    }
    let list = devs
        .iter()
        .map(|d| format!("{} w={} um", d.name, d.w as f64 / 1000.0))
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "no common finger width fits {list}: every device needs an even number of fingers of one width, at least {} um and on the {} nm grid{}",
        MIN_FINGER_W as f64 / 1000.0,
        GRID,
        fingers
            .map(|f| format!(", with {f} fingers on the smallest"))
            .unwrap_or_default()
    ))
}

/// Unit order for one island: `counts[i]` units of device `i`.
pub fn unit_order(counts: &[usize], pattern: Pattern) -> Vec<usize> {
    // Spread each device's units evenly: unit k of device i sits at
    // (k + 0.5) / n_i, ties to the lower index.
    fn spread(counts: &[usize]) -> Vec<usize> {
        let mut slots: Vec<(f64, usize)> = Vec::new();
        for (i, &n) in counts.iter().enumerate() {
            for k in 0..n {
                slots.push(((k as f64 + 0.5) / n as f64, i));
            }
        }
        slots.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        slots.into_iter().map(|s| s.1).collect()
    }
    match pattern {
        Pattern::Interdigitated => spread(counts),
        Pattern::CommonCentroid => {
            let halves: Vec<usize> = counts.iter().map(|n| n / 2).collect();
            let half = spread(&halves);
            let middle: Vec<usize> = counts
                .iter()
                .enumerate()
                .filter(|(_, n)| *n % 2 == 1)
                .map(|(i, _)| i)
                .collect();
            let mut out = half.clone();
            out.extend(middle);
            out.extend(half.iter().rev());
            out
        }
    }
}

/// Lay out one row.
pub fn build(spec: &RowSpec, params: &Params) -> Result<RowLayout, String> {
    let devs: Vec<&RowDev> = spec.islands.iter().flat_map(|i| i.devices.iter()).collect();
    let Some(first) = devs.first() else {
        return Err(format!("row {} has no devices", spec.name));
    };
    let l = first.l;
    if let Some(d) = devs.iter().find(|d| d.l != l) {
        return Err(format!(
            "{} and {} in row {} have different lengths; a row shares one gate pitch",
            first.name, d.name, spec.name
        ));
    }
    let wf = finger_width(&devs, params.fingers)?;
    let p = l + PCELL_PITCH_EXTRA;
    let dummies = i64::from(params.dummies);
    let model = first.model;

    let mut plan = Plan::default();
    let mut down = Vec::new();
    let mut up = Vec::new();
    let mut extra = Vec::new();
    let mut pcell_nets = BTreeMap::new();
    let mut fingers = BTreeMap::new();
    let mut order_text = Vec::new();
    let patch_y1 = 0.min(wf - SD_PATCH_H);

    let mut x0 = 0i64;
    for (ii, island) in spec.islands.iter().enumerate() {
        if ii > 0 {
            x0 += ISLAND_GAP;
        }
        // Gates in order (None for a dummy) and the net of every column.
        let counts: Vec<usize> = island
            .devices
            .iter()
            .map(|d| (d.w / wf / 2) as usize)
            .collect();
        for (d, n) in island.devices.iter().zip(&counts) {
            fingers.insert(d.name.clone(), 2 * *n as i64);
        }
        let order = unit_order(&counts, params.pattern);
        let mut gates: Vec<Option<usize>> = Vec::new();
        let mut columns: Vec<String> = vec![island.source.clone()];
        for _ in 0..dummies {
            gates.push(None);
            columns.push(island.source.clone());
        }
        for &u in &order {
            let d = &island.devices[u];
            gates.extend([Some(u), Some(u)]);
            columns.push(d.drain_for(&island.source).to_string());
            columns.push(island.source.clone());
        }
        for _ in 0..dummies {
            gates.push(None);
            columns.push(island.source.clone());
        }
        let mut text = Vec::new();
        text.extend(std::iter::repeat_n("d".to_string(), dummies as usize));
        text.extend(order.iter().map(|&u| island.devices[u].name.clone()));
        text.extend(std::iter::repeat_n("d".to_string(), dummies as usize));
        order_text.push(text.join(" "));

        // One pcell per run of fingers of one device.
        let mut k = 0;
        while k < gates.len() {
            let mut n = 1;
            while k + n < gates.len() && gates[k + n] == gates[k] {
                n += 1;
            }
            let device = match gates[k] {
                Some(u) => island.devices[u].name.clone(),
                None => "dummy".into(),
            };
            plan.instances.push(PcellInst {
                pcell: spec.kind.pcell().into(),
                w: wf * n as i64,
                l,
                ng: n as i64,
                x: x0 + k as i64 * p,
                y: 0,
                device,
            });
            if gates[k].is_none() {
                extra.push(ExtraDevice {
                    name: String::new(),
                    model: model.to_string(),
                    w: wf * n as i64,
                    l,
                    ng: n as i64,
                    d: island.source.clone(),
                    g: spec.off.clone(),
                    s: island.source.clone(),
                    b: spec.bulk.clone(),
                });
            }
            k += n;
        }

        // Source and drain columns: a widened Metal1 patch and a Via1.
        for (k, net) in columns.iter().enumerate() {
            let k = k as i64;
            pcell_nets.insert((x0 + PCELL_SD_X + k * p, 0), net.clone());
            let cx = x0 + PCELL_SD_X + PCELL_SD_W / 2 + k * p;
            plan.add(
                Layer::Metal1,
                Rect::new(cx - SD_PATCH_W / 2, patch_y1, cx + SD_PATCH_W / 2, wf),
                Some(net),
            );
            let via = Rect::new(
                cx - VIA / 2,
                patch_y1 + VIA_ENDCAP,
                cx + VIA / 2,
                patch_y1 + VIA_ENDCAP + VIA,
            );
            plan.add(Layer::Via1, via, Some(net));
            down.push(Terminal {
                x: cx,
                via,
                net: net.clone(),
            });
        }

        // Gates: poly up to a contacted pad, Metal1 pad, Via1.
        for (k, g) in gates.iter().enumerate() {
            let k = k as i64;
            let net = match g {
                Some(u) => island.devices[*u].g.clone(),
                None => spec.off.clone(),
            };
            let gx = x0 + PCELL_GATE_X + k * p;
            pcell_nets.insert((gx, -PCELL_GATE_EXT), net.clone());
            let pc = gx + snap_down(l / 2);
            let top = wf + PAD_GAP;
            plan.add(Layer::GatPoly, Rect::new(gx, wf, gx + l, top), Some(&net));
            let pad = if l >= POLY_PAD {
                Rect::new(gx, top, gx + l, top + POLY_PAD)
            } else {
                Rect::new(pc - POLY_PAD / 2, top, pc + POLY_PAD / 2, top + POLY_PAD)
            };
            plan.add(Layer::GatPoly, pad, Some(&net));
            let cy = top + POLY_PAD / 2;
            plan.add(Layer::Cont, Rect::centred(pc, cy, CNT, CNT), Some(&net));
            plan.add(
                Layer::Metal1,
                Rect::centred(pc, cy, M1_PAD_W, M1_PAD_H),
                Some(&net),
            );
            let via = Rect::centred(pc, cy, VIA, VIA);
            plan.add(Layer::Via1, via, Some(&net));
            up.push(Terminal { x: pc, via, net });
        }
        x0 += PCELL_ACT_END + gates.len() as i64 * p;
    }

    // The devices' extent, then the ring around it.
    let dev_box = Rect::new(
        0,
        patch_y1.min(-PCELL_GATE_EXT),
        x0,
        wf + PAD_GAP + POLY_PAD / 2 + M1_PAD_H / 2,
    );
    let inner = dev_box.expand(RING_GAP);
    let outer = inner.expand(RING_W);
    let sides: Vec<Rect> = if params.guard_ring {
        vec![
            Rect::new(outer.x1, outer.y1, outer.x2, inner.y1),
            Rect::new(outer.x1, inner.y2, outer.x2, outer.y2),
            Rect::new(outer.x1, inner.y1, inner.x1, inner.y2),
            Rect::new(inner.x2, inner.y1, outer.x2, inner.y2),
        ]
    } else {
        vec![Rect::new(outer.x1, outer.y1, outer.x2, inner.y1)]
    };
    let bulk = spec.bulk.as_str();
    for s in &sides {
        plan.add(Layer::Activ, *s, Some(bulk));
        plan.add(Layer::Metal1, *s, Some(bulk));
        if spec.kind == Kind::Nmos {
            plan.add(Layer::PSd, s.expand(PSD_ENC), None);
        }
    }
    // Contacts along each side; the vertical sides stop short of the
    // horizontal sides' contacts.
    for (i, s) in sides.iter().enumerate() {
        let horizontal = i < 2;
        let (from, to) = if horizontal {
            (s.x1 + CNT_ACT_ENC, s.x2 - CNT_ACT_ENC)
        } else {
            (
                s.y1 + CNT_SPACE - CNT_ACT_ENC,
                s.y2 - CNT_SPACE + CNT_ACT_ENC,
            )
        };
        for c in spaced(from, to) {
            let r = if horizontal {
                Rect::new(c, s.y1 + CNT_ACT_ENC, c + CNT, s.y1 + CNT_ACT_ENC + CNT)
            } else {
                Rect::new(s.x1 + CNT_ACT_ENC, c, s.x1 + CNT_ACT_ENC + CNT, c + CNT)
            };
            plan.add(Layer::Cont, r, Some(bulk));
        }
    }
    if spec.kind == Kind::Pmos {
        plan.add(Layer::NWell, outer.expand(NWELL_ENC), None);
    }
    for x in [outer.x1 + RING_W / 2, outer.x2 - RING_W / 2] {
        let via = Rect::centred(x, outer.y1 + RING_W / 2, VIA, VIA);
        plan.add(Layer::Via1, via, Some(bulk));
        down.push(Terminal {
            x,
            via,
            net: spec.bulk.clone(),
        });
    }

    for (i, e) in extra.iter_mut().enumerate() {
        e.name = format!("{}_dummy{}", spec.name, i + 1);
    }
    Ok(RowLayout {
        plan,
        down,
        up,
        ring: outer,
        bulk: spec.bulk.clone(),
        dummies: extra,
        summary: RowSummary {
            name: spec.name.clone(),
            kind: spec.kind,
            finger_width_um: wf as f64 / 1000.0,
            length_um: l as f64 / 1000.0,
            fingers,
            order: order_text,
        },
        pcell_nets,
    })
}

/// Lower-left positions of as many contacts as fit between `from` and `to`
/// at the minimum pitch, centred.
fn spaced(from: i64, to: i64) -> Vec<i64> {
    let len = to - from;
    if len < CNT {
        return Vec::new();
    }
    let n = (len - CNT) / (CNT + CNT_SPACE) + 1;
    let used = n * CNT + (n - 1) * CNT_SPACE;
    let start = from + snap_down((len - used) / 2);
    (0..n).map(|i| start + i * (CNT + CNT_SPACE)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, w: i64, l: i64, d: &str, g: &str, s: &str) -> RowDev {
        RowDev {
            name: name.into(),
            model: "sg13_lv_nmos",
            w,
            l,
            d: d.into(),
            g: g.into(),
            s: s.into(),
            b: "0".into(),
        }
    }

    #[test]
    fn unit_orders() {
        use Pattern::*;
        assert_eq!(unit_order(&[2, 2], Interdigitated), [0, 1, 0, 1]);
        assert_eq!(unit_order(&[1, 2], Interdigitated), [1, 0, 1]);
        assert_eq!(unit_order(&[2, 2], CommonCentroid), [0, 1, 1, 0]);
        assert_eq!(
            unit_order(&[4, 4], CommonCentroid),
            [0, 1, 0, 1, 1, 0, 1, 0]
        );
        assert_eq!(unit_order(&[1, 2], CommonCentroid), [1, 0, 1]);
        assert_eq!(unit_order(&[1, 1], CommonCentroid), [0, 1]);
    }

    #[test]
    fn finger_widths_are_common_and_even() {
        let a = dev("M1", 2000, 500, "x", "g", "0");
        let b = dev("M2", 4000, 500, "y", "g", "0");
        assert_eq!(finger_width(&[&a, &b], None), Ok(1000));
        assert_eq!(finger_width(&[&a, &b], Some(4)), Ok(500));
        assert!(
            finger_width(&[&a, &b], Some(3))
                .unwrap_err()
                .contains("even")
        );
        let c = dev("M3", 3000, 500, "y", "g", "0");
        // 2u and 3u: 0.5 um fingers give 4 and 6.
        assert_eq!(finger_width(&[&a, &c], None), Ok(500));
        let tiny = dev("M4", 400, 500, "y", "g", "0");
        assert!(finger_width(&[&tiny], None).unwrap_err().contains("0.3 um"));
    }

    #[test]
    fn contacts_fill_a_side() {
        assert_eq!(spaced(0, 160), [0]);
        assert_eq!(spaced(0, 500), [0, 340]);
        assert_eq!(spaced(0, 520), [10, 350]);
        assert!(spaced(0, 100).is_empty());
    }

    #[test]
    fn a_mirror_row_is_clean_by_construction() {
        let spec = RowSpec {
            name: "mirror".into(),
            kind: Kind::Nmos,
            islands: vec![Island {
                devices: vec![
                    dev("M1", 2000, 500, "ref", "ref", "0"),
                    dev("M2", 2000, 500, "out", "ref", "0"),
                ],
                source: "0".into(),
            }],
            bulk: "0".into(),
            off: "0".into(),
        };
        let row = build(&spec, &Params::default()).unwrap();
        assert_eq!(row.summary.order, ["d M1 M2 d"]);
        assert_eq!(row.summary.fingers["M1"], 2);
        // One pcell per run of fingers: d, M1, M2, d.
        assert_eq!(row.plan.instances.len(), 4);
        assert_eq!(row.dummies.len(), 2);
        assert_eq!(row.up.len(), 6);
        assert_eq!(row.down.len(), 7 + 2);
        let errors = row.plan.spacing_errors(&row.pcell_nets);
        assert!(errors.is_empty(), "{errors:#?}");
    }
}
