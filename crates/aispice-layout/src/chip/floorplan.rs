//! Where everything of a chip goes: die, seal ring, bond pads, the IO ring
//! (pads, corners, fillers), the supply straps into the ring, and the
//! blocks in the core.
//!
//! From the die edge inward: the seal ring (IHP's pcell, its 25 um edge box
//! on the die edge), clearance, a 70 um band for the bond pads, which abut
//! the IO cells' bond side as Croc places them, the 180 um IO ring, and the
//! core. All coordinates are nanometres with the die's lower left at 0.

use super::geom::{Facing, LayerRect, METAL3, METAL4, Rect, Trans};
use super::io::{CORNER, CORNER_CELL, FILLERS, IO_HEIGHT, IoLibrary};
use super::spec::{ChipSpec, PadKind, Side};
use serde::Serialize;
use std::collections::BTreeMap;

/// From the die edge to the IO cells' outer edge: seal ring, clearance and
/// the bond pad band.
pub const IO_OFFSET: i64 = 130_000;
/// Bond pads: 70 um squares centred this far outside a pad cell's middle.
pub const BOND_SIZE: i64 = 70_000;
const BOND_CENTRE: (i64, i64) = (40_000, -35_000);
/// The seal ring pcell's edge box, which sits on the die edge.
pub const SEAL_EDGE: i64 = 25_000;
/// The chip router's grid, nm. Every grid point is a multiple of it.
pub const GRID: i64 = 600;
/// Space between blocks placed in a row, and around them in the core.
pub const MACRO_GAP: i64 = 40_000;
/// Room above and below the block row for routes.
const ROUTE_ROOM: i64 = 120_000;
/// Supply straps into the IO ring rails, in a supply pad's frame: Metal3
/// onto the `vdd` rail (160 to 178 um), Metal4 onto the `vss` rail (162.2 to
/// 178 um); neither reaches the other rail of its layer.
pub const VDD_STRAP: Rect = Rect {
    x1: 36_000,
    y1: 162_000,
    x2: 44_000,
    y2: 180_000,
};
pub const VSS_STRAP: Rect = Rect {
    x1: 36_000,
    y1: 164_000,
    x2: 44_000,
    y2: 180_000,
};

/// A block as the floorplan sees it: its abstract, in its own (GDS) frame.
#[derive(Debug, Clone, PartialEq)]
pub struct MacroGeom {
    pub instance: String,
    /// The block's cell: the macro name.
    pub cell: String,
    /// LEF `SIZE`, nm.
    pub size: (i64, i64),
    /// LEF `ORIGIN`: where the cell's origin sits from its lower left, nm.
    pub origin: (i64, i64),
    /// Pin shapes in the cell's frame.
    pub pins: BTreeMap<String, Vec<LayerRect>>,
    /// The lower left asked for in the core, nm.
    pub at: Option<(i64, i64)>,
}

/// An IO cell placed in the ring.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IoPlaced {
    pub cell: String,
    pub trans: Trans,
}

/// A pad of the ring.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PadPlaced {
    pub net: String,
    pub kind: PadKind,
    pub cell: String,
    pub trans: Trans,
    /// The cell's outline on the die.
    pub bbox: Rect,
    /// Centre of its bond pad.
    pub bond: (i64, i64),
    /// Where the core connects: a signal pin or a supply strap, on the die.
    pub core: Option<LayerRect>,
    /// Which way the core is from the pad.
    pub inward: Facing,
}

/// A block placed in the core.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MacroPlaced {
    pub instance: String,
    pub cell: String,
    pub trans: Trans,
    /// Its outline on the die.
    pub bbox: Rect,
    /// Its pin shapes on the die.
    pub pins: BTreeMap<String, Vec<LayerRect>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Floorplan {
    pub die: Rect,
    /// Inside the IO ring.
    pub core: Rect,
    pub ring: Vec<IoPlaced>,
    pub pads: Vec<PadPlaced>,
    pub macros: Vec<MacroPlaced>,
}

fn snap_up(v: i64, step: i64) -> i64 {
    (v + step - 1).div_euclid(step) * step
}

fn snap_down(v: i64, step: i64) -> i64 {
    v.div_euclid(step) * step
}

/// Fillers that make up a gap exactly, widest first.
pub fn fillers(gap: i64) -> Result<Vec<(i64, &'static str)>, String> {
    let mut left = gap;
    let mut out = Vec::new();
    for (w, name) in FILLERS {
        while left >= w {
            out.push((w, name));
            left -= w;
        }
    }
    if left != 0 {
        return Err(format!(
            "a gap of {} um in the pad ring is not a whole number of um",
            gap as f64 / 1000.0
        ));
    }
    Ok(out)
}

/// How a cell that spans `s .. s + w` along a side, counted from the
/// side's start (left for south and north, bottom for west and east), is
/// placed so its outer edge is on the ring's outside and its local north
/// points into the core.
fn side_trans(side: Side, die: (i64, i64), s: i64, w: i64) -> Trans {
    let (dw, dh) = die;
    let start = IO_OFFSET + CORNER;
    match side {
        Side::South => Trans::new(0, start + s, IO_OFFSET),
        Side::North => Trans::new(2, start + s + w, dh - IO_OFFSET),
        Side::West => Trans::new(3, IO_OFFSET, start + s + w),
        Side::East => Trans::new(1, dw - IO_OFFSET, start + s),
    }
}

/// Lay out a chip: pads, ring, blocks.
pub fn plan(spec: &ChipSpec, io: &IoLibrary, macros: &[MacroGeom]) -> Result<Floorplan, String> {
    let pitch = spec.pad_pitch_nm();
    let count = |side: Side| spec.pads.iter().filter(|p| p.side == side).count() as i64;
    let pad_w = io.cell(PadKind::Analog.cell())?.width;
    for p in &spec.pads {
        if io.cell(p.kind.cell())?.width != pad_w {
            return Err(format!("{} is not {} um wide", p.kind.cell(), pad_w / 1000));
        }
    }
    let row_len = |n: i64| if n == 0 { 0 } else { (n - 1) * pitch + pad_w };

    // The core must hold the blocks with room to route around them.
    let auto: Vec<&MacroGeom> = macros.iter().filter(|m| m.at.is_none()).collect();
    let row_w: i64 =
        auto.iter().map(|m| m.size.0).sum::<i64>() + MACRO_GAP * (auto.len() as i64 + 1);
    let row_h = auto.iter().map(|m| m.size.1).max().unwrap_or(0) + 2 * ROUTE_ROOM;
    let mut need_w = row_w.max(200_000);
    let mut need_h = row_h.max(200_000);
    for m in macros.iter() {
        if let Some((x, y)) = m.at {
            need_w = need_w.max(x + m.size.0 + MACRO_GAP);
            need_h = need_h.max(y + m.size.1 + MACRO_GAP);
        }
    }
    let side_x = snap_up(
        row_len(count(Side::South).max(count(Side::North))).max(need_w) + 2 * 20_000,
        10_000,
    );
    let side_y = snap_up(
        row_len(count(Side::West).max(count(Side::East))).max(need_h) + 2 * 20_000,
        10_000,
    );
    let w = 2 * (IO_OFFSET + CORNER) + side_x;
    let h = 2 * (IO_OFFSET + CORNER) + side_y;
    let die = Rect::new(0, 0, w, h);
    let core = Rect::new(
        IO_OFFSET + IO_HEIGHT,
        IO_OFFSET + IO_HEIGHT,
        w - IO_OFFSET - IO_HEIGHT,
        h - IO_OFFSET - IO_HEIGHT,
    );

    let mut ring = vec![
        IoPlaced {
            cell: CORNER_CELL.into(),
            trans: Trans::new(0, IO_OFFSET, IO_OFFSET),
        },
        IoPlaced {
            cell: CORNER_CELL.into(),
            trans: Trans::new(1, w - IO_OFFSET, IO_OFFSET),
        },
        IoPlaced {
            cell: CORNER_CELL.into(),
            trans: Trans::new(2, w - IO_OFFSET, h - IO_OFFSET),
        },
        IoPlaced {
            cell: CORNER_CELL.into(),
            trans: Trans::new(3, IO_OFFSET, h - IO_OFFSET),
        },
    ];
    let mut pads = Vec::new();
    for side in Side::ALL {
        let len = if matches!(side, Side::South | Side::North) {
            side_x
        } else {
            side_y
        };
        let mine: Vec<_> = spec.pads.iter().filter(|p| p.side == side).collect();
        let n = mine.len() as i64;
        let first = snap_down((len - row_len(n)) / 2, 1000);
        let mut at = 0i64;
        let fill = |from: i64, to: i64, ring: &mut Vec<IoPlaced>| -> Result<(), String> {
            let mut s = from;
            for (fw, name) in fillers(to - from)? {
                ring.push(IoPlaced {
                    cell: name.into(),
                    trans: side_trans(side, (w, h), s, fw),
                });
                s += fw;
            }
            Ok(())
        };
        for (i, p) in mine.iter().enumerate() {
            let s = first + i as i64 * pitch;
            fill(at, s, &mut ring)?;
            let cell = io.cell(p.kind.cell())?;
            let trans = side_trans(side, (w, h), s, cell.width);
            ring.push(IoPlaced {
                cell: cell.name.clone(),
                trans,
            });
            let local_core = match p.kind {
                PadKind::Analog => {
                    let pin = if p.resistor { "padres" } else { "pad" };
                    Some(cell.core_pin(pin, "Metal3").ok_or_else(|| {
                        format!("{} has no {pin} pin on Metal3 at its core edge", cell.name)
                    })?)
                }
                PadKind::Input => cell.core_pin("p2c", "Metal3"),
                PadKind::Output => cell.core_pin("c2p", "Metal3"),
                PadKind::Vdd => Some(LayerRect {
                    layer: METAL3,
                    rect: VDD_STRAP,
                }),
                PadKind::Vss => Some(LayerRect {
                    layer: METAL4,
                    rect: VSS_STRAP,
                }),
                PadKind::Iovdd | PadKind::Iovss => None,
            };
            if p.kind.is_signal() && local_core.is_none() {
                return Err(format!("{} has no core pin on Metal3", cell.name));
            }
            pads.push(PadPlaced {
                net: p.net.clone(),
                kind: p.kind,
                cell: cell.name.clone(),
                trans,
                bbox: trans.rect(&Rect::new(0, 0, cell.width, cell.height)),
                bond: trans.point(BOND_CENTRE.0, BOND_CENTRE.1),
                core: local_core.map(|r| LayerRect {
                    layer: r.layer,
                    rect: trans.rect(&r.rect),
                }),
                inward: Facing::of_local_north(&trans),
            });
            at = s + cell.width;
        }
        fill(at, len, &mut ring)?;
    }

    // Blocks: those placed by hand, then the rest in a row in the middle.
    let mut placed: Vec<MacroPlaced> = Vec::new();
    let row_x = core.x1 + (core.width() - row_w) / 2 + MACRO_GAP;
    let row_y = core.y1 + (core.height() - (row_h - 2 * ROUTE_ROOM)) / 2;
    let mut x = row_x;
    for m in macros {
        let (llx, lly) = match m.at {
            Some((ax, ay)) => (core.x1 + ax, core.y1 + ay),
            None => {
                let at = (x, row_y);
                x += m.size.0 + MACRO_GAP;
                at
            }
        };
        // The cell's origin lands on the router's grid, so pins drawn on a
        // grid fixed to it (aispice's edge pins) do too.
        let dx = snap_up(llx + m.origin.0, GRID);
        let dy = snap_up(lly + m.origin.1, GRID);
        let trans = Trans::new(0, dx, dy);
        let bbox = Rect::new(
            dx - m.origin.0,
            dy - m.origin.1,
            dx - m.origin.0 + m.size.0,
            dy - m.origin.1 + m.size.1,
        );
        let inner = Rect::new(
            core.x1 + MACRO_GAP / 2,
            core.y1 + MACRO_GAP / 2,
            core.x2 - MACRO_GAP / 2,
            core.y2 - MACRO_GAP / 2,
        );
        if bbox.x1 < inner.x1 || bbox.y1 < inner.y1 || bbox.x2 > inner.x2 || bbox.y2 > inner.y2 {
            return Err(format!(
                "macro {} at ({:.1}, {:.1}) um does not fit in the core with {} um around it",
                m.instance,
                (bbox.x1 - core.x1) as f64 / 1000.0,
                (bbox.y1 - core.y1) as f64 / 1000.0,
                MACRO_GAP / 2000
            ));
        }
        if let Some(o) = placed.iter().find(|o| {
            o.bbox
                .expand(MACRO_GAP / 2)
                .touches(&bbox.expand(MACRO_GAP / 2))
        }) {
            return Err(format!(
                "macros {} and {} are closer than {} um",
                o.instance,
                m.instance,
                MACRO_GAP / 1000
            ));
        }
        let pins = m
            .pins
            .iter()
            .map(|(name, rects)| {
                (
                    name.clone(),
                    rects
                        .iter()
                        .map(|r| LayerRect {
                            layer: r.layer,
                            rect: trans.rect(&r.rect),
                        })
                        .collect(),
                )
            })
            .collect();
        placed.push(MacroPlaced {
            instance: m.instance.clone(),
            cell: m.cell.clone(),
            trans,
            bbox,
            pins,
        });
    }
    Ok(Floorplan {
        die,
        core,
        ring,
        pads,
        macros: placed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chip::io::test_lef;

    fn spec() -> ChipSpec {
        let mut text = String::from("name = \"t\"\n");
        for (net, side, kind) in [
            ("a", "west", "analog"),
            ("b", "west", "input"),
            ("c", "east", "output"),
            ("vdd", "north", "vdd"),
            ("vss", "south", "vss"),
            ("iovdd", "north", "iovdd"),
            ("iovss", "south", "iovss"),
        ] {
            text.push_str(&format!(
                "[[pads]]\nnet = \"{net}\"\nside = \"{side}\"\nkind = \"{kind}\"\n"
            ));
        }
        ChipSpec::parse(&text).unwrap()
    }

    fn block(name: &str) -> MacroGeom {
        let mut pins = BTreeMap::new();
        pins.insert(
            "inp".to_string(),
            vec![LayerRect {
                layer: METAL4,
                rect: Rect::new(-2_550, -600, -2_250, 15_830),
            }],
        );
        MacroGeom {
            instance: name.into(),
            cell: format!("{name}_cell"),
            size: (24_390, 25_620),
            origin: (7_350, 600),
            pins,
            at: None,
        }
    }

    #[test]
    fn the_ring_is_closed_and_every_side_is_filled_exactly() {
        let io = IoLibrary::from_lef(&test_lef()).unwrap();
        let fp = plan(&spec(), &io, &[block("ota"), block("cmp")]).unwrap();
        let widths: BTreeMap<String, i64> =
            io.cells.iter().map(|(n, c)| (n.clone(), c.width)).collect();
        // Every cell of a side, end to end, covers the side between corners.
        let side_len = fp.die.width() - 2 * (IO_OFFSET + CORNER);
        let mut covered = 0;
        let mut boxes: Vec<Rect> = Vec::new();
        for c in &fp.ring {
            let w = if c.cell == CORNER_CELL {
                CORNER
            } else {
                widths[&c.cell]
            };
            let h = if c.cell == CORNER_CELL {
                CORNER
            } else {
                IO_HEIGHT
            };
            let b = c.trans.rect(&Rect::new(0, 0, w, h));
            assert!(b.x1 >= IO_OFFSET && b.y1 >= IO_OFFSET, "{c:?}");
            assert!(
                b.x2 <= fp.die.x2 - IO_OFFSET && b.y2 <= fp.die.y2 - IO_OFFSET,
                "{c:?}"
            );
            if c.trans.rot == 0 && c.cell != CORNER_CELL {
                covered += w;
            }
            boxes.push(b);
        }
        assert_eq!(covered, side_len, "the south side is filled exactly");
        // Nothing overlaps.
        for (i, a) in boxes.iter().enumerate() {
            for b in &boxes[i + 1..] {
                let overlap = a.x1 < b.x2 && b.x1 < a.x2 && a.y1 < b.y2 && b.y1 < a.y2;
                assert!(!overlap, "{a:?} {b:?}");
            }
        }
        // The core is inside the ring, and the pads face it.
        assert_eq!(fp.core.x1, IO_OFFSET + IO_HEIGHT);
        for p in &fp.pads {
            if let Some(c) = &p.core {
                let inside = match p.inward {
                    Facing::North => c.rect.y2 == fp.core.y1,
                    Facing::South => c.rect.y1 == fp.core.y2,
                    Facing::East => c.rect.x2 == fp.core.x1,
                    Facing::West => c.rect.x1 == fp.core.x2,
                };
                assert!(inside, "{} core pin {:?} on the core edge", p.net, c.rect);
            }
            assert!(fp.die.x1 < p.bond.0 - BOND_SIZE / 2 && p.bond.0 + BOND_SIZE / 2 < fp.die.x2);
        }
        let west: Vec<&PadPlaced> = fp
            .pads
            .iter()
            .filter(|p| p.inward == Facing::East)
            .collect();
        assert_eq!(west.len(), 2);
        // Blocks sit on the grid, in the core, apart.
        for m in &fp.macros {
            assert_eq!(m.trans.dx % GRID, 0);
            assert_eq!(m.trans.dy % GRID, 0);
            assert!(m.bbox.x1 > fp.core.x1 && m.bbox.x2 < fp.core.x2);
            let pin = &m.pins["inp"][0].rect;
            assert_eq!(pin.y1, m.bbox.y1, "the edge pin is on the block's edge");
        }
        assert!(fp.macros[0].bbox.x2 + MACRO_GAP <= fp.macros[1].bbox.x1 + GRID);
    }

    #[test]
    fn corners_meet_the_pads_beside_them() {
        let io = IoLibrary::from_lef(&test_lef()).unwrap();
        let fp = plan(&spec(), &io, &[]).unwrap();
        let (w, h) = (fp.die.width(), fp.die.height());
        let corners: Vec<Rect> = fp
            .ring
            .iter()
            .filter(|c| c.cell == CORNER_CELL)
            .map(|c| c.trans.rect(&Rect::new(0, 0, CORNER, CORNER)))
            .collect();
        assert_eq!(
            corners[0],
            Rect::new(IO_OFFSET, IO_OFFSET, IO_OFFSET + CORNER, IO_OFFSET + CORNER)
        );
        assert_eq!(corners[1].x2, w - IO_OFFSET);
        assert_eq!(
            corners[2],
            Rect::new(
                w - IO_OFFSET - CORNER,
                h - IO_OFFSET - CORNER,
                w - IO_OFFSET,
                h - IO_OFFSET
            )
        );
        assert_eq!(corners[3].x1, IO_OFFSET);
        // A corner's local edge x = 180 um meets the next side's first cell.
        let se = fp.ring[1].trans;
        assert_eq!(se.point(CORNER, 0), (w - IO_OFFSET, IO_OFFSET + CORNER));
    }

    #[test]
    fn gaps_are_made_of_fillers() {
        let f = fillers(78_000).unwrap();
        let widths: Vec<i64> = f.iter().map(|(w, _)| *w).collect();
        assert_eq!(widths, [50_000, 20_000, 5_000, 2_000, 1_000]);
        assert!(fillers(500).is_err());
        assert!(fillers(0).unwrap().is_empty());
    }

    #[test]
    fn blocks_that_do_not_fit_are_named() {
        let io = IoLibrary::from_lef(&test_lef()).unwrap();
        let mut a = block("ota");
        a.at = Some((40_000, 40_000));
        let mut b = block("cmp");
        b.at = Some((50_000, 50_000));
        let e = plan(&spec(), &io, &[a.clone(), b]).unwrap_err();
        assert!(e.contains("closer than"), "{e}");
        a.at = Some((0, 0));
        let e = plan(&spec(), &io, &[a]).unwrap_err();
        assert!(e.contains("does not fit"), "{e}");
    }
}
