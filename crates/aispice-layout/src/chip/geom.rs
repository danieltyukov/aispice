//! Geometry for chip assembly: placements as KLayout transformations, and
//! rectangles on IHP SG13G2 layers, in nanometres.

pub use crate::generator::plan::Rect;
use aispice_core::macropkg::lef::LefRect;
use serde::Serialize;

/// GDS layer numbers of IHP SG13G2's metals and vias by LEF name.
pub fn layer_by_name(name: &str) -> Option<i16> {
    Some(match name {
        "Metal1" => 8,
        "Via1" => 19,
        "Metal2" => 10,
        "Via2" => 29,
        "Metal3" => 30,
        "Via3" => 49,
        "Metal4" => 50,
        "Via4" => 66,
        "Metal5" => 67,
        "TopVia1" => 125,
        "TopMetal1" => 126,
        "TopVia2" => 133,
        "TopMetal2" => 134,
        _ => return None,
    })
}

pub const METAL1: i16 = 8;
pub const VIA1: i16 = 19;
pub const METAL2: i16 = 10;
pub const VIA2: i16 = 29;
pub const METAL3: i16 = 30;
pub const VIA3: i16 = 49;
pub const METAL4: i16 = 50;
/// Datatype of labels on metal layers.
pub const TEXT: i16 = 25;

/// A rectangle on a GDS layer (drawing datatype).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LayerRect {
    pub layer: i16,
    pub rect: Rect,
}

impl LayerRect {
    /// A LEF rectangle in nm, moved by `offset` (um), when its layer is a
    /// metal of the PDK.
    pub fn from_lef(r: &LefRect, offset: (f64, f64)) -> Option<LayerRect> {
        let layer = layer_by_name(&r.layer)?;
        let nm = |v: f64| (v * 1000.0).round() as i64;
        Some(LayerRect {
            layer,
            rect: Rect::new(
                nm(r.rect[0] + offset.0),
                nm(r.rect[1] + offset.1),
                nm(r.rect[2] + offset.0),
                nm(r.rect[3] + offset.1),
            ),
        })
    }
}

/// A placement: mirror about the x axis (first), then `rot` quarter turns
/// counter-clockwise, then move by (`dx`, `dy`); KLayout's `Trans`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Trans {
    pub rot: u8,
    pub mirror: bool,
    pub dx: i64,
    pub dy: i64,
}

impl Trans {
    pub fn new(rot: u8, dx: i64, dy: i64) -> Trans {
        Trans {
            rot: rot % 4,
            mirror: false,
            dx,
            dy,
        }
    }

    pub fn point(&self, x: i64, y: i64) -> (i64, i64) {
        let y = if self.mirror { -y } else { y };
        let (x, y) = match self.rot % 4 {
            0 => (x, y),
            1 => (-y, x),
            2 => (-x, -y),
            _ => (y, -x),
        };
        (x + self.dx, y + self.dy)
    }

    pub fn rect(&self, r: &Rect) -> Rect {
        let (x1, y1) = self.point(r.x1, r.y1);
        let (x2, y2) = self.point(r.x2, r.y2);
        Rect::new(x1, y1, x2, y2)
    }
}

/// Which way out of a placed object a pin leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Facing {
    North,
    East,
    South,
    West,
}

impl Facing {
    /// The facing of a cell's local north (`+y`) after a placement.
    pub fn of_local_north(t: &Trans) -> Facing {
        let (x0, y0) = t.point(0, 0);
        let (x1, y1) = t.point(0, 1);
        match (x1 - x0, y1 - y0) {
            (0, 1) => Facing::North,
            (0, _) => Facing::South,
            (1, 0) => Facing::East,
            _ => Facing::West,
        }
    }

    pub fn step(self) -> (i64, i64) {
        match self {
            Facing::North => (0, 1),
            Facing::East => (1, 0),
            Facing::South => (0, -1),
            Facing::West => (-1, 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transformations_follow_klayout() {
        let r = Rect::new(0, 0, 80, 180);
        assert_eq!(Trans::new(0, 10, 20).rect(&r), Rect::new(10, 20, 90, 200));
        assert_eq!(Trans::new(1, 0, 0).rect(&r), Rect::new(-180, 0, 0, 80));
        assert_eq!(Trans::new(2, 0, 0).rect(&r), Rect::new(-80, -180, 0, 0));
        assert_eq!(Trans::new(3, 0, 0).rect(&r), Rect::new(0, -80, 180, 0));
        let m = Trans {
            rot: 0,
            mirror: true,
            dx: 0,
            dy: 0,
        };
        assert_eq!(m.point(3, 4), (3, -4));
        assert_eq!(Facing::of_local_north(&Trans::new(0, 5, 5)), Facing::North);
        assert_eq!(Facing::of_local_north(&Trans::new(1, 5, 5)), Facing::West);
        assert_eq!(Facing::of_local_north(&Trans::new(2, 5, 5)), Facing::South);
        assert_eq!(Facing::of_local_north(&Trans::new(3, 5, 5)), Facing::East);
    }
}
