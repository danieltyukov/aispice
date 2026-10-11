//! Just enough GDSII to check a macro: cell names, top cells, bounding boxes
//! and labels.
//!
//! The reader walks the record stream once, keeps each cell's shapes and
//! references, and computes a cell's box through its whole hierarchy on
//! demand. Text does not count toward a box, as in KLayout. ChipLeap's
//! `chipleap.macro.gds` reads GDS the same way.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// (x0, y0, x1, y1).
pub type BBox = (f64, f64, f64, f64);
type Pt = (f64, f64);
/// x' = a x + b y + e, y' = c x + d y + f.
type Affine = (f64, f64, f64, f64, f64, f64);

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct GdsError(pub String);

fn err<T>(msg: impl Into<String>) -> Result<T, GdsError> {
    Err(GdsError(msg.into()))
}

#[derive(Debug, Clone)]
struct Ref {
    cell: String,
    x: f64,
    y: f64,
    mirror: bool,
    mag: f64,
    angle: f64,
    cols: i32,
    rows: i32,
    col_step: Pt,
    row_step: Pt,
}

#[derive(Debug, Clone, Default)]
struct Cell {
    /// Each shape's corner points: a polygon's vertices, a path's box corners.
    shapes: Vec<Vec<Pt>>,
    refs: Vec<Ref>,
    labels: Vec<String>,
}

/// A parsed GDS file. Coordinates are in micrometres.
#[derive(Debug, Clone)]
pub struct GdsLibrary {
    pub um_per_dbu: f64,
    order: Vec<String>,
    cells: BTreeMap<String, Cell>,
}

impl GdsLibrary {
    pub fn has_cell(&self, name: &str) -> bool {
        self.cells.contains_key(name)
    }

    /// Cells no other cell references, in file order.
    pub fn top_cells(&self) -> Vec<String> {
        let used: HashSet<&str> = self
            .cells
            .values()
            .flat_map(|c| c.refs.iter().map(|r| r.cell.as_str()))
            .collect();
        self.order
            .iter()
            .filter(|n| !used.contains(n.as_str()))
            .cloned()
            .collect()
    }

    /// Text strings placed directly in a cell.
    pub fn labels(&self, cell: &str) -> Vec<String> {
        self.cells
            .get(cell)
            .map(|c| c.labels.clone())
            .unwrap_or_default()
    }

    /// The cell's bounding box through its hierarchy, or None when it is empty.
    pub fn bbox(&self, cell: &str) -> Result<Option<BBox>, GdsError> {
        let k = self.um_per_dbu;
        Ok(self
            .bbox_dbu(cell, &mut Vec::new())?
            .map(|b| (b.0 * k, b.1 * k, b.2 * k, b.3 * k)))
    }

    fn cell<'a>(&'a self, name: &str, stack: &[String]) -> Result<&'a Cell, GdsError> {
        if stack.iter().any(|s| s == name) {
            return err(format!("cell {name} contains itself"));
        }
        match self.cells.get(name) {
            Some(c) => Ok(c),
            None => err(format!("cell {name} is referenced but not defined")),
        }
    }

    fn bbox_dbu(&self, name: &str, stack: &mut Vec<String>) -> Result<Option<BBox>, GdsError> {
        let cell = self.cell(name, stack)?;
        let mut bx: Option<BBox> = None;
        for pts in &cell.shapes {
            bx = union(bx, points_box(pts));
        }
        stack.push(name.to_string());
        for r in &cell.refs {
            for (dx, dy) in corners(r) {
                if r.angle % 90.0 == 0.0 {
                    // A quarter turn maps the child's box onto the placed box exactly.
                    if let Some(child) = self.bbox_dbu(&r.cell, stack)? {
                        bx = union(bx, transform(child, r, dx, dy));
                    }
                } else if let Some(placed) = self.exact(&r.cell, affine(r, dx, dy), stack)? {
                    bx = union(bx, placed);
                }
            }
        }
        stack.pop();
        Ok(bx)
    }

    /// The box of a cell under any transform, from its transformed corner points.
    fn exact(
        &self,
        name: &str,
        m: Affine,
        stack: &mut Vec<String>,
    ) -> Result<Option<BBox>, GdsError> {
        let cell = self.cell(name, stack)?;
        let mut bx: Option<BBox> = None;
        for pts in &cell.shapes {
            let moved: Vec<Pt> = pts.iter().map(|p| apply(m, *p)).collect();
            bx = union(bx, points_box(&moved));
        }
        stack.push(name.to_string());
        for r in &cell.refs {
            for (dx, dy) in corners(r) {
                if let Some(placed) = self.exact(&r.cell, compose(m, affine(r, dx, dy)), stack)? {
                    bx = union(bx, placed);
                }
            }
        }
        stack.pop();
        Ok(bx)
    }
}

fn union(a: Option<BBox>, b: BBox) -> Option<BBox> {
    Some(match a {
        None => b,
        Some(a) => (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)),
    })
}

fn points_box(pts: &[Pt]) -> BBox {
    let mut b = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for &(x, y) in pts {
        b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
    }
    b
}

/// Offsets of the corner instances of a reference; the others lie between them.
fn corners(r: &Ref) -> Vec<Pt> {
    let is: Vec<i32> = if r.cols > 1 {
        vec![0, r.cols - 1]
    } else {
        vec![0]
    };
    let js: Vec<i32> = if r.rows > 1 {
        vec![0, r.rows - 1]
    } else {
        vec![0]
    };
    let mut out = Vec::new();
    for &i in &is {
        for &j in &js {
            let (i, j) = (i as f64, j as f64);
            out.push((
                r.x + i * r.col_step.0 + j * r.row_step.0,
                r.y + i * r.col_step.1 + j * r.row_step.1,
            ));
        }
    }
    out
}

/// Exact sine and cosine for quarter turns, so rotated boxes do not pick up
/// rounding.
fn cos_sin(angle: f64) -> Pt {
    let a = angle.rem_euclid(360.0);
    if a == 0.0 {
        (1.0, 0.0)
    } else if a == 90.0 {
        (0.0, 1.0)
    } else if a == 180.0 {
        (-1.0, 0.0)
    } else if a == 270.0 {
        (0.0, -1.0)
    } else {
        let r = a.to_radians();
        (r.cos(), r.sin())
    }
}

fn affine(r: &Ref, dx: f64, dy: f64) -> Affine {
    let (c, s) = cos_sin(r.angle);
    let f = if r.mirror { -1.0 } else { 1.0 };
    (r.mag * c, -r.mag * s * f, r.mag * s, r.mag * c * f, dx, dy)
}

/// m after n.
fn compose(m: Affine, n: Affine) -> Affine {
    let (a, b, c, d, e, f) = m;
    let (p, q, r, s, t, u) = n;
    (
        a * p + b * r,
        a * q + b * s,
        c * p + d * r,
        c * q + d * s,
        a * t + b * u + e,
        c * t + d * u + f,
    )
}

fn apply(m: Affine, (x, y): Pt) -> Pt {
    (m.0 * x + m.1 * y + m.4, m.2 * x + m.3 * y + m.5)
}

fn transform(b: BBox, r: &Ref, dx: f64, dy: f64) -> BBox {
    let m = affine(r, dx, dy);
    let pts = [(b.0, b.1), (b.2, b.1), (b.2, b.3), (b.0, b.3)].map(|p| apply(m, p));
    points_box(&pts)
}

fn real8(b: &[u8]) -> f64 {
    let sign = if b[0] & 0x80 != 0 { -1.0 } else { 1.0 };
    let exp = (b[0] & 0x7f) as i32 - 64;
    let mut mant: u64 = 0;
    for &x in &b[1..8] {
        mant = (mant << 8) | x as u64;
    }
    sign * mant as f64 / (1u64 << 56) as f64 * 16f64.powi(exp)
}

/// The box of a path: half the width around every point, and along the path
/// at its two ends only as far as the path type extends it (flush ends do not).
fn path_box(xy: &[Pt], width: f64, pathtype: i16, ext: Pt) -> BBox {
    let w = width.abs() / 2.0;
    let (begin, end) = match pathtype {
        1 | 2 => (w, w),
        4 => ext,
        _ => (0.0, 0.0),
    };
    let n = xy.len();
    let mut pts = Vec::with_capacity(2 * n);
    for (k, &(x, y)) in xy.iter().enumerate() {
        let (mut ex, mut ey) = (w, w);
        if n > 1 && (k == 0 || k == n - 1) {
            let (ox, oy) = if k == 0 { xy[1] } else { xy[n - 2] };
            let along = if k == 0 { begin } else { end };
            if oy == y {
                ex = along;
            } else if ox == x {
                ey = along;
            }
        }
        pts.push((x - ex, y - ey));
        pts.push((x + ex, y + ey));
    }
    points_box(&pts)
}

fn need<'a>(body: &'a [u8], n: usize, what: &str) -> Result<&'a [u8], GdsError> {
    if body.len() < n {
        return err(format!("{what} record is too short"));
    }
    Ok(&body[..n])
}

fn i16_at(b: &[u8]) -> i16 {
    i16::from_be_bytes([b[0], b[1]])
}

fn i32_at(b: &[u8]) -> i32 {
    i32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn name_of(body: &[u8]) -> String {
    let end = body.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    // Latin-1: every byte is the code point of the same value.
    body[..end].iter().map(|&b| b as char).collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Element {
    Shape,
    Path,
    Sref,
    Aref,
    Text,
    Node,
}

struct Pending {
    element: Element,
    xy: Vec<Pt>,
    width: f64,
    pathtype: i16,
    ext: Pt,
    sname: String,
    strans: u16,
    mag: f64,
    angle: f64,
    colrow: (i32, i32),
    text: String,
}

impl Pending {
    fn new(element: Element) -> Self {
        Pending {
            element,
            xy: Vec::new(),
            width: 0.0,
            pathtype: 0,
            ext: (0.0, 0.0),
            sname: String::new(),
            strans: 0,
            mag: 1.0,
            angle: 0.0,
            colrow: (1, 1),
            text: String::new(),
        }
    }

    fn finish(self, name: &str, cell: &mut Cell) -> Result<(), GdsError> {
        match self.element {
            Element::Text => {
                cell.labels.push(self.text);
                return Ok(());
            }
            Element::Node => return Ok(()),
            _ => {}
        }
        if self.xy.is_empty() {
            return err(format!("element without coordinates in cell {name}"));
        }
        let mirror = self.strans & 0x8000 != 0;
        match self.element {
            Element::Shape => cell.shapes.push(self.xy),
            Element::Path => {
                let (x0, y0, x1, y1) = path_box(&self.xy, self.width, self.pathtype, self.ext);
                cell.shapes
                    .push(vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)]);
            }
            Element::Sref => cell.refs.push(Ref {
                cell: self.sname,
                x: self.xy[0].0,
                y: self.xy[0].1,
                mirror,
                mag: self.mag,
                angle: self.angle,
                cols: 1,
                rows: 1,
                col_step: (0.0, 0.0),
                row_step: (0.0, 0.0),
            }),
            Element::Aref => {
                if self.xy.len() < 3 {
                    return err(format!(
                        "array reference without three points in cell {name}"
                    ));
                }
                let (cols, rows) = self.colrow;
                if cols < 1 || rows < 1 {
                    return err(format!(
                        "array reference with {cols} x {rows} instances in cell {name}"
                    ));
                }
                let ((x0, y0), (x1, y1), (x2, y2)) = (self.xy[0], self.xy[1], self.xy[2]);
                let (c, r) = (cols as f64, rows as f64);
                cell.refs.push(Ref {
                    cell: self.sname,
                    x: x0,
                    y: y0,
                    mirror,
                    mag: self.mag,
                    angle: self.angle,
                    cols,
                    rows,
                    col_step: ((x1 - x0) / c, (y1 - y0) / c),
                    row_step: ((x2 - x0) / r, (y2 - y0) / r),
                });
            }
            Element::Text | Element::Node => {}
        }
        Ok(())
    }
}

/// Parse GDSII bytes.
pub fn parse_gds(data: &[u8]) -> Result<GdsLibrary, GdsError> {
    let mut cells: BTreeMap<String, Cell> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut um_per_dbu = 0.0;
    let mut cell: Option<String> = None;
    let mut pending: Option<Pending> = None;
    let mut pos = 0usize;
    let mut ended = false;
    while pos < data.len() {
        if pos + 4 > data.len() {
            return err(format!("truncated record header at byte {pos}"));
        }
        let length = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        let rtype = data[pos + 2];
        if length < 4 || pos + length > data.len() {
            return err(format!("bad record length {length} at byte {pos}"));
        }
        let body = &data[pos + 4..pos + length];
        let at = pos;
        pos += length;
        match rtype {
            0x03 => um_per_dbu = real8(&need(body, 16, "UNITS")?[8..16]) * 1e6,
            0x05 | 0x07 => cell = None,
            0x06 => {
                let name = name_of(body);
                if cells.contains_key(&name) {
                    return err(format!("cell {name} is defined twice"));
                }
                cells.insert(name.clone(), Cell::default());
                order.push(name.clone());
                cell = Some(name);
            }
            0x08 | 0x09 | 0x0A | 0x0B | 0x0C | 0x15 | 0x2D => {
                if cell.is_none() {
                    return err(format!("element outside a cell at byte {at}"));
                }
                pending = Some(Pending::new(match rtype {
                    0x09 => Element::Path,
                    0x0A => Element::Sref,
                    0x0B => Element::Aref,
                    0x0C => Element::Text,
                    0x15 => Element::Node,
                    _ => Element::Shape,
                }));
            }
            0x11 => {
                let (Some(name), Some(p)) = (cell.as_ref(), pending.take()) else {
                    return err(format!("ENDEL outside an element at byte {at}"));
                };
                let target = cells.get_mut(name).expect("the current cell exists");
                p.finish(name, target)?;
            }
            0x04 => {
                ended = true;
                break;
            }
            0x10 => {
                let (points, rest) = body.as_chunks::<8>();
                if !rest.is_empty() {
                    return err("XY record has an odd number of coordinates");
                }
                let xy = points
                    .iter()
                    .map(|c| (i32_at(&c[..4]) as f64, i32_at(&c[4..]) as f64))
                    .collect();
                if let Some(p) = pending.as_mut() {
                    p.xy = xy;
                }
            }
            // Element properties: checked wherever they appear, kept for the
            // element being read.
            0x0F => {
                let v = i32_at(need(body, 4, "WIDTH")?) as f64;
                if let Some(p) = pending.as_mut() {
                    p.width = v;
                }
            }
            0x21 => {
                let v = i16_at(need(body, 2, "PATHTYPE")?);
                if let Some(p) = pending.as_mut() {
                    p.pathtype = v;
                }
            }
            0x30 | 0x31 => {
                let what = if rtype == 0x30 { "BGNEXTN" } else { "ENDEXTN" };
                let v = i32_at(need(body, 4, what)?) as f64;
                if let Some(p) = pending.as_mut() {
                    if rtype == 0x30 {
                        p.ext.0 = v;
                    } else {
                        p.ext.1 = v;
                    }
                }
            }
            0x12 | 0x19 => {
                let v = name_of(body);
                if let Some(p) = pending.as_mut() {
                    if rtype == 0x12 {
                        p.sname = v;
                    } else {
                        p.text = v;
                    }
                }
            }
            0x1A => {
                let v = i16_at(need(body, 2, "STRANS")?) as u16;
                if let Some(p) = pending.as_mut() {
                    p.strans = v;
                }
            }
            0x1B | 0x1C => {
                let what = if rtype == 0x1B { "MAG" } else { "ANGLE" };
                let v = real8(need(body, 8, what)?);
                if let Some(p) = pending.as_mut() {
                    if rtype == 0x1B {
                        p.mag = v;
                    } else {
                        p.angle = v;
                    }
                }
            }
            0x13 => {
                let b = need(body, 4, "COLROW")?;
                let v = (i16_at(&b[..2]) as i32, i16_at(&b[2..]) as i32);
                if let Some(p) = pending.as_mut() {
                    p.colrow = v;
                }
            }
            _ => {}
        }
    }
    if !ended {
        return err("no ENDLIB record: the file is truncated");
    }
    if um_per_dbu <= 0.0 {
        return err("no UNITS record");
    }
    Ok(GdsLibrary {
        um_per_dbu,
        order,
        cells,
    })
}

pub fn read_gds(path: &Path) -> Result<GdsLibrary, GdsError> {
    let data = std::fs::read(path).map_err(|e| GdsError(e.to_string()))?;
    parse_gds(&data)
}
