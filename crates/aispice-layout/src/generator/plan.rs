//! A drawing plan: what a generator decided, in a form the KLayout drawing
//! script reads as data. Coordinates are in database units (1 nm).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The IHP SG13G2 layers a plan draws on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Activ,
    GatPoly,
    Cont,
    PSd,
    NWell,
    Metal1,
    Via1,
    Metal2,
    Via2,
    Metal3,
    Via3,
    Metal4,
}

impl Layer {
    /// GDS layer number of the drawing purpose.
    pub fn gds(self) -> i16 {
        match self {
            Layer::Activ => 1,
            Layer::GatPoly => 5,
            Layer::Cont => 6,
            Layer::PSd => 14,
            Layer::NWell => 31,
            Layer::Metal1 => 8,
            Layer::Via1 => 19,
            Layer::Metal2 => 10,
            Layer::Via2 => 29,
            Layer::Metal3 => 30,
            Layer::Via3 => 49,
            Layer::Metal4 => 50,
        }
    }

    /// Minimum space between shapes of different nets, nm.
    pub fn space(self) -> i64 {
        match self {
            Layer::Activ => 210,
            Layer::GatPoly => 180,
            Layer::Cont => 180,
            Layer::Metal1 => 180,
            Layer::Via1 | Layer::Via2 | Layer::Via3 => 220,
            Layer::Metal2 | Layer::Metal3 | Layer::Metal4 => 210,
            Layer::PSd => 310,
            Layer::NWell => 620,
        }
    }
}

/// GDS datatypes for pins and labels on metal layers.
pub const PIN: i16 = 2;
pub const TEXT: i16 = 25;

/// A rectangle, left-bottom to right-top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect {
    pub x1: i64,
    pub y1: i64,
    pub x2: i64,
    pub y2: i64,
}

impl Rect {
    pub fn new(x1: i64, y1: i64, x2: i64, y2: i64) -> Self {
        Self {
            x1: x1.min(x2),
            y1: y1.min(y2),
            x2: x1.max(x2),
            y2: y1.max(y2),
        }
    }

    /// A square of side `size` centred on a point.
    pub fn centred(x: i64, y: i64, w: i64, h: i64) -> Self {
        Self::new(x - w / 2, y - h / 2, x - w / 2 + w, y - h / 2 + h)
    }

    pub fn width(&self) -> i64 {
        self.x2 - self.x1
    }

    pub fn height(&self) -> i64 {
        self.y2 - self.y1
    }

    pub fn translate(&self, dx: i64, dy: i64) -> Self {
        Self::new(self.x1 + dx, self.y1 + dy, self.x2 + dx, self.y2 + dy)
    }

    pub fn expand(&self, d: i64) -> Self {
        Self::new(self.x1 - d, self.y1 - d, self.x2 + d, self.y2 + d)
    }

    pub fn union(&self, o: &Rect) -> Self {
        Self::new(
            self.x1.min(o.x1),
            self.y1.min(o.y1),
            self.x2.max(o.x2),
            self.y2.max(o.y2),
        )
    }

    /// Whether the two share area or an edge.
    pub fn touches(&self, o: &Rect) -> bool {
        self.x1 <= o.x2 && o.x1 <= self.x2 && self.y1 <= o.y2 && o.y1 <= self.y2
    }

    /// Euclidean distance between the two, 0 when they touch.
    pub fn distance(&self, o: &Rect) -> f64 {
        let dx = (o.x1 - self.x2).max(self.x1 - o.x2).max(0) as f64;
        let dy = (o.y1 - self.y2).max(self.y1 - o.y2).max(0) as f64;
        (dx * dx + dy * dy).sqrt()
    }

    pub fn um(&self) -> [f64; 4] {
        [
            self.x1 as f64 / 1000.0,
            self.y1 as f64 / 1000.0,
            self.x2 as f64 / 1000.0,
            self.y2 as f64 / 1000.0,
        ]
    }
}

/// One rectangle on a layer, with the net it belongs to (`None` for shapes
/// that carry no signal, such as wells and implants).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shape {
    pub layer: Layer,
    pub rect: Rect,
    pub net: Option<String>,
}

/// One of IHP's MOS pcells, placed unrotated with its origin at (x, y).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PcellInst {
    /// `nmos` or `pmos`.
    pub pcell: String,
    /// Total width over all fingers, nm.
    pub w: i64,
    pub l: i64,
    pub ng: i64,
    pub x: i64,
    pub y: i64,
    /// The schematic device it is part of, or `dummy`.
    pub device: String,
}

/// A net name on a metal layer; `pin` makes it a port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    pub layer: Layer,
    pub text: String,
    pub x: i64,
    pub y: i64,
    pub pin: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub cell: String,
    pub instances: Vec<PcellInst>,
    pub shapes: Vec<Shape>,
    pub labels: Vec<Label>,
}

impl Plan {
    pub fn translate(&mut self, dx: i64, dy: i64) {
        for i in &mut self.instances {
            i.x += dx;
            i.y += dy;
        }
        for s in &mut self.shapes {
            s.rect = s.rect.translate(dx, dy);
        }
        for l in &mut self.labels {
            l.x += dx;
            l.y += dy;
        }
    }

    pub fn add(&mut self, layer: Layer, rect: Rect, net: Option<&str>) {
        self.shapes.push(Shape {
            layer,
            rect,
            net: net.map(str::to_string),
        });
    }

    pub fn extend(&mut self, other: Plan) {
        self.instances.extend(other.instances);
        self.shapes.extend(other.shapes);
        self.labels.extend(other.labels);
    }

    /// The bounding box of the shapes and pcells.
    pub fn bbox(&self) -> Option<Rect> {
        self.shapes
            .iter()
            .map(|s| s.rect)
            .chain(self.instances.iter().flat_map(|i| {
                pcell_shapes(i, &BTreeMap::new())
                    .into_iter()
                    .map(|s| s.rect)
            }))
            .reduce(|a, b| a.union(&b))
    }

    /// The plan as the drawing script reads it: pcells, rectangles as
    /// `[layer, datatype, x1, y1, x2, y2]` and texts as
    /// `[layer, datatype, text, x, y]`.
    pub fn to_draw_json(&self) -> serde_json::Value {
        let shapes: Vec<_> = self
            .shapes
            .iter()
            .map(|s| {
                serde_json::json!([s.layer.gds(), 0, s.rect.x1, s.rect.y1, s.rect.x2, s.rect.y2])
            })
            .collect();
        let mut texts = Vec::new();
        let mut pins = Vec::new();
        for l in &self.labels {
            texts.push(serde_json::json!([l.layer.gds(), TEXT, l.text, l.x, l.y]));
            if l.pin {
                // Magic reads text on the pin purpose as a port.
                texts.push(serde_json::json!([l.layer.gds(), PIN, l.text, l.x, l.y]));
                let r = Rect::centred(l.x, l.y, 200, 200);
                pins.push(serde_json::json!([
                    l.layer.gds(),
                    PIN,
                    r.x1,
                    r.y1,
                    r.x2,
                    r.y2
                ]));
            }
        }
        let shapes: Vec<_> = shapes.into_iter().chain(pins).collect();
        let instances: Vec<_> = self
            .instances
            .iter()
            .map(|i| {
                serde_json::json!({"pcell": i.pcell, "w": i.w, "l": i.l, "ng": i.ng, "x": i.x, "y": i.y})
            })
            .collect();
        serde_json::json!({
            "cell": self.cell,
            "instances": instances,
            "shapes": shapes,
            "texts": texts,
        })
    }

    /// Shapes of different nets on one layer closer than the layer's rule,
    /// counting the pcells' own Activ, poly, contacts and Metal1 (with the
    /// nets given for their columns and gates in `pcell_nets`). Empty for a
    /// plan that is clean by construction.
    pub fn spacing_errors(&self, pcell_nets: &BTreeMap<(i64, i64), String>) -> Vec<String> {
        let mut by_layer: BTreeMap<Layer, Vec<Shape>> = BTreeMap::new();
        for s in &self.shapes {
            by_layer.entry(s.layer).or_default().push(s.clone());
        }
        for i in &self.instances {
            for s in pcell_shapes(i, pcell_nets) {
                by_layer.entry(s.layer).or_default().push(s);
            }
        }
        let mut out = Vec::new();
        for (layer, shapes) in &by_layer {
            if matches!(layer, Layer::PSd | Layer::NWell) {
                continue;
            }
            let rule = layer.space() as f64;
            for (i, a) in shapes.iter().enumerate() {
                for b in &shapes[i + 1..] {
                    let (Some(na), Some(nb)) = (&a.net, &b.net) else {
                        continue;
                    };
                    if na == nb {
                        continue;
                    }
                    let d = a.rect.distance(&b.rect);
                    if d < rule {
                        out.push(format!(
                            "{layer:?}: {na} {:?} and {nb} {:?} are {d:.0} nm apart (rule {rule:.0})",
                            a.rect, b.rect
                        ));
                    }
                }
            }
        }
        out
    }
}

/// The Activ, gate poly, contacts and Metal1 stripes IHP's MOS pcell draws
/// for an instance, as far as the planner relies on them. Nets come from
/// `nets`, keyed by the shape's lower-left corner; shapes without an entry
/// have no net.
pub fn pcell_shapes(i: &PcellInst, nets: &BTreeMap<(i64, i64), String>) -> Vec<Shape> {
    use super::rules::*;
    let wf = i.w / i.ng.max(1);
    let p = i.l + PCELL_PITCH_EXTRA;
    let net = |x: i64, y: i64| nets.get(&(x, y)).cloned();
    let mut out = Vec::new();
    let act = Rect::new(i.x, i.y, i.x + PCELL_ACT_END + i.ng * p, i.y + wf);
    out.push(Shape {
        layer: Layer::Activ,
        rect: act,
        net: None,
    });
    for k in 0..=i.ng {
        let x = i.x + PCELL_SD_X + k * p;
        let r = Rect::new(x, i.y, x + PCELL_SD_W, i.y + wf);
        out.push(Shape {
            layer: Layer::Metal1,
            rect: r,
            net: net(r.x1, r.y1),
        });
    }
    for k in 0..i.ng {
        let x = i.x + PCELL_GATE_X + k * p;
        let r = Rect::new(x, i.y - PCELL_GATE_EXT, x + i.l, i.y + wf + PCELL_GATE_EXT);
        out.push(Shape {
            layer: Layer::GatPoly,
            rect: r,
            net: net(r.x1, r.y1),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangles_measure_and_move() {
        let a = Rect::new(10, 10, 0, 0);
        assert_eq!((a.x1, a.y1, a.x2, a.y2), (0, 0, 10, 10));
        let b = a.translate(13, 14);
        assert!((a.distance(&b) - 5.0).abs() < 1e-9);
        assert!(a.touches(&Rect::new(10, 0, 20, 5)));
        assert_eq!(a.distance(&Rect::new(10, 0, 20, 5)), 0.0);
        assert_eq!(Rect::centred(0, 0, 190, 190), Rect::new(-95, -95, 95, 95));
        assert_eq!(a.expand(5), Rect::new(-5, -5, 15, 15));
    }

    #[test]
    fn pcells_follow_ihp_geometry() {
        let i = PcellInst {
            pcell: "nmos".into(),
            w: 2000,
            l: 500,
            ng: 2,
            x: 0,
            y: 0,
            device: "M1".into(),
        };
        let s = pcell_shapes(&i, &BTreeMap::new());
        // Measured from the pcell: Activ (0,0;2060,1000), stripes at 70,
        // 950 and 1830, gates at 340 and 1220 from -180 to 1180.
        assert_eq!(s[0].rect, Rect::new(0, 0, 2060, 1000));
        let m1: Vec<i64> = s
            .iter()
            .filter(|s| s.layer == Layer::Metal1)
            .map(|s| s.rect.x1)
            .collect();
        assert_eq!(m1, [70, 950, 1830]);
        let poly: Vec<Rect> = s
            .iter()
            .filter(|s| s.layer == Layer::GatPoly)
            .map(|s| s.rect)
            .collect();
        assert_eq!(
            poly,
            [
                Rect::new(340, -180, 840, 1180),
                Rect::new(1220, -180, 1720, 1180)
            ]
        );
    }

    #[test]
    fn spacing_errors_name_both_nets() {
        let mut p = Plan::default();
        p.add(Layer::Metal2, Rect::new(0, 0, 200, 1000), Some("a"));
        p.add(Layer::Metal2, Rect::new(500, 0, 700, 1000), Some("b"));
        p.add(Layer::Metal2, Rect::new(200, 0, 300, 1000), Some("a"));
        let e = p.spacing_errors(&BTreeMap::new());
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(e[0].contains("a") && e[0].contains("b"), "{e:?}");
    }
}
