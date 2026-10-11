//! Just enough GDSII to check a macro: cell names, top cells, bounding boxes
//! and labels.
//!
//! The reader walks the record stream once and keeps each cell's shape
//! points, the box of its own shapes, its references and its labels. It then
//! walks the hierarchy once without recursion: a cell that contains itself,
//! directly or through other cells, is an error, and so are references nested
//! deeper than the limit, so everything after it can recurse safely. A cell's
//! box through its hierarchy is computed once per cell, and an array
//! reference's box comes from its four corner instances, never from every
//! instance. Text does not count toward a box, as in KLayout. ChipLeap's
//! `chipleap.macro.gds` reads GDS the same way.

use super::limits::{LimitError, Limits, ReadError, read_capped};
use super::text::quote;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;

/// (x0, y0, x1, y1).
pub type BBox = (f64, f64, f64, f64);
type Pt = (f64, f64);
/// x' = a x + b y + e, y' = c x + d y + f.
type Affine = (f64, f64, f64, f64, f64, f64);

#[derive(Debug, thiserror::Error)]
pub enum GdsError {
    /// A malformed stream, or a reference to a cell the file does not define.
    #[error("{0}")]
    Syntax(String),
    /// A cell contains itself, directly or through other cells.
    #[error("{0}")]
    Cycle(String),
    #[error(transparent)]
    Limit(#[from] LimitError),
}

fn err<T>(msg: impl Into<String>) -> Result<T, GdsError> {
    Err(GdsError::Syntax(msg.into()))
}

fn limit<T>(msg: impl Into<String>) -> Result<T, GdsError> {
    Err(GdsError::Limit(LimitError(msg.into())))
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
    /// Every shape's corner points: a polygon's vertices, a path's box corners.
    points: Vec<Pt>,
    /// The box of the cell's own shapes.
    own: Option<BBox>,
    refs: Vec<Ref>,
    labels: Vec<String>,
}

/// A parsed GDS file. Coordinates are in micrometres.
#[derive(Debug, Clone)]
pub struct GdsLibrary {
    pub um_per_dbu: f64,
    limits: Limits,
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

    /// The cell's bounding box through its hierarchy, or None when it is
    /// empty. Fails for a reference to a cell the file does not define, and
    /// when measuring it takes too much work or overflows.
    pub fn bbox(&self, cell: &str) -> Result<Option<BBox>, GdsError> {
        let k = self.um_per_dbu;
        let mut m = Measure {
            lib: self,
            boxes: HashMap::new(),
            hulls: HashMap::new(),
            points: 0,
        };
        Ok(m.bbox(cell)?.map(|b| (b.0 * k, b.1 * k, b.2 * k, b.3 * k)))
    }
}

/// One measurement: boxes and hulls computed once per cell, and the points
/// spent on references at other angles than quarter turns.
struct Measure<'a> {
    lib: &'a GdsLibrary,
    boxes: HashMap<&'a str, Option<BBox>>,
    hulls: HashMap<&'a str, Rc<Vec<Pt>>>,
    points: usize,
}

impl<'a> Measure<'a> {
    fn cell(&self, name: &str) -> Result<&'a Cell, GdsError> {
        match self.lib.cells.get(name) {
            Some(c) => Ok(c),
            None => err(format!("cell {name} is referenced but not defined")),
        }
    }

    fn spend(&mut self, points: usize) -> Result<(), GdsError> {
        self.points = self.points.saturating_add(points);
        let most = self.lib.limits.gds_points;
        if self.points > most {
            return limit(format!(
                "references at angles other than quarter turns need more than {most} points to measure"
            ));
        }
        Ok(())
    }

    fn bbox(&mut self, name: &str) -> Result<Option<BBox>, GdsError> {
        if let Some(b) = self.boxes.get(name) {
            return Ok(*b);
        }
        let cell = self.cell(name)?;
        let mut bx = cell.own;
        for r in &cell.refs {
            let corners = corners(r);
            if r.angle % 90.0 == 0.0 {
                // A quarter turn maps the child's box onto the placed box exactly.
                if let Some(child) = self.bbox(&r.cell)? {
                    for &(dx, dy) in &corners {
                        bx = union(bx, transform(child, r, dx, dy)?);
                    }
                }
            } else {
                let hull = self.hull(&r.cell)?;
                if !hull.is_empty() {
                    self.spend(hull.len().saturating_mul(corners.len()))?;
                    for &(dx, dy) in &corners {
                        bx = union(bx, points_box(&place(affine(r, dx, dy), &hull)?));
                    }
                }
            }
        }
        let key = self.key(name);
        self.boxes.insert(key, bx);
        Ok(bx)
    }

    /// The convex hull of a cell through its hierarchy. Any transform maps
    /// it onto the hull of the placed cell, so its placed points give the
    /// placed box exactly.
    fn hull(&mut self, name: &str) -> Result<Rc<Vec<Pt>>, GdsError> {
        if let Some(h) = self.hulls.get(name) {
            return Ok(Rc::clone(h));
        }
        let cell = self.cell(name)?;
        let mut children = Vec::with_capacity(cell.refs.len());
        for r in &cell.refs {
            children.push((r, corners(r), self.hull(&r.cell)?));
        }
        let spent = children.iter().fold(cell.points.len(), |n, (_, c, h)| {
            n.saturating_add(c.len().saturating_mul(h.len()))
        });
        self.spend(spent)?;
        let mut pts = cell.points.clone();
        for (r, corners, h) in &children {
            for &(dx, dy) in corners {
                pts.extend(place(affine(r, dx, dy), h)?);
            }
        }
        let hull = Rc::new(convex_hull(pts));
        let key = self.key(name);
        self.hulls.insert(key, Rc::clone(&hull));
        Ok(hull)
    }

    /// The library's own copy of a cell name, which lives as long as the
    /// memo tables.
    fn key(&self, name: &str) -> &'a str {
        self.lib
            .cells
            .get_key_value(name)
            .map_or("", |(k, _)| k.as_str())
    }
}

const OVERFLOW: &str = "a reference's magnification takes coordinates out of range";
/// The largest coordinate a measurement keeps, in database units: far past any
/// layout, and small enough that the hull's cross products of differences
/// cannot overflow.
const FAR: f64 = 1e150;

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
            let (i, j) = (f64::from(i), f64::from(j));
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

fn apply(m: Affine, (x, y): Pt) -> Pt {
    (m.0 * x + m.1 * y + m.4, m.2 * x + m.3 * y + m.5)
}

/// Points under a transform. A huge magnification, compounded through the
/// hierarchy, can take them out of range, and the box would then be
/// meaningless.
fn place(m: Affine, pts: &[Pt]) -> Result<Vec<Pt>, GdsError> {
    let out: Vec<Pt> = pts.iter().map(|&p| apply(m, p)).collect();
    if out.iter().all(|(x, y)| x.abs() <= FAR && y.abs() <= FAR) {
        Ok(out)
    } else {
        limit(OVERFLOW)
    }
}

fn transform(b: BBox, r: &Ref, dx: f64, dy: f64) -> Result<BBox, GdsError> {
    let m = affine(r, dx, dy);
    let pts = place(m, &[(b.0, b.1), (b.2, b.1), (b.2, b.3), (b.0, b.3)])?;
    Ok(points_box(&pts))
}

fn cross(o: Pt, a: Pt, b: Pt) -> f64 {
    (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
}

/// Andrew's monotone chain. ChipLeap sorts and compares the same way, so both
/// tools keep the same points. Every coordinate is within [`FAR`], so the
/// comparisons are total and the cross products finite; equal coordinates (0
/// and -0 among them) keep their order, as in Python's stable sort.
fn convex_hull(mut pts: Vec<Pt>) -> Vec<Pt> {
    use std::cmp::Ordering::Equal;
    pts.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(Equal)
            .then(a.1.partial_cmp(&b.1).unwrap_or(Equal))
    });
    if pts.len() <= 2 {
        return pts;
    }
    let mut lower: Vec<Pt> = Vec::new();
    for &p in &pts {
        while let [.., a, b] = lower[..]
            && cross(a, b, p) <= 0.0
        {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Vec<Pt> = Vec::new();
    for &p in pts.iter().rev() {
        while let [.., a, b] = upper[..]
            && cross(a, b, p) <= 0.0
        {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

fn real8(b: &[u8; 8]) -> f64 {
    let sign = if b[0] & 0x80 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from(b[0] & 0x7f) - 64;
    let mut mant: u64 = 0;
    for &x in &b[1..] {
        mant = (mant << 8) | u64::from(x);
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
        let other = match k {
            0 => xy.get(1),
            _ if k + 1 == n => xy.get(n.wrapping_sub(2)),
            _ => None,
        };
        if let Some(&(ox, oy)) = other {
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

/// The first `N` bytes of a record body, or an error naming the record.
fn need<const N: usize>(body: &[u8], what: &str) -> Result<[u8; N], GdsError> {
    match body.get(..N).and_then(|b| <[u8; N]>::try_from(b).ok()) {
        Some(b) => Ok(b),
        None => err(format!("{what} record is too short")),
    }
}

fn name_of(body: &[u8]) -> String {
    let end = body.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    // Latin-1: every byte is the code point of the same value.
    body[..end].iter().map(|&b| char::from(b)).collect()
}

/// Depth-first over every cell in file order and every reference in element
/// order, with an explicit stack: the first cycle found, or the first cell
/// with references nested deeper than the limit below it, is an error.
fn check_hierarchy(
    order: &[String],
    cells: &BTreeMap<String, Cell>,
    limits: &Limits,
) -> Result<(), GdsError> {
    let mut height: HashMap<&str, usize> = HashMap::new();
    for root in order {
        if height.contains_key(root.as_str()) {
            continue;
        }
        let mut path: Vec<&str> = vec![root.as_str()];
        let mut on_path: HashSet<&str> = HashSet::from([root.as_str()]);
        let mut stack: Vec<(&str, &[Ref], usize)> = Vec::new();
        if let Some(c) = cells.get(root) {
            stack.push((root.as_str(), &c.refs, 0));
        }
        while let Some(top) = stack.last_mut() {
            let (name, refs, k) = *top;
            if let Some(r) = refs.get(k) {
                top.2 += 1;
                let child = r.cell.as_str();
                if on_path.contains(child) {
                    let from = path.iter().position(|c| *c == child).unwrap_or(0);
                    let mut cycle: Vec<String> = path[from..].iter().map(|c| quote(c)).collect();
                    cycle.push(quote(child));
                    return Err(GdsError::Cycle(format!(
                        "cell {} contains itself: {}",
                        quote(child),
                        cycle.join(" -> ")
                    )));
                }
                if height.contains_key(child) {
                    continue;
                }
                let Some((key, c)) = cells.get_key_value(child) else {
                    continue;
                };
                path.push(key);
                on_path.insert(key);
                stack.push((key, &c.refs, 0));
                continue;
            }
            let h = refs
                .iter()
                .filter_map(|r| height.get(r.cell.as_str()).map(|h| h + 1))
                .max()
                .unwrap_or(0);
            if h > limits.gds_depth {
                return limit(format!(
                    "references nest more than {} deep below cell {}",
                    limits.gds_depth,
                    quote(name)
                ));
            }
            height.insert(name, h);
            stack.pop();
            path.pop();
            on_path.remove(name);
        }
    }
    Ok(())
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
        let Some(&(x0, y0)) = self.xy.first() else {
            return err(format!("element without coordinates in cell {name}"));
        };
        let mirror = self.strans & 0x8000 != 0;
        match self.element {
            Element::Shape => add_shape(cell, &self.xy),
            Element::Path => {
                let (x0, y0, x1, y1) = path_box(&self.xy, self.width, self.pathtype, self.ext);
                add_shape(cell, &[(x0, y0), (x1, y0), (x1, y1), (x0, y1)]);
            }
            Element::Sref => cell.refs.push(Ref {
                cell: self.sname,
                x: x0,
                y: y0,
                mirror,
                mag: self.mag,
                angle: self.angle,
                cols: 1,
                rows: 1,
                col_step: (0.0, 0.0),
                row_step: (0.0, 0.0),
            }),
            Element::Aref => {
                let [_, (x1, y1), (x2, y2), ..] = self.xy[..] else {
                    return err(format!(
                        "array reference without three points in cell {name}"
                    ));
                };
                let (cols, rows) = self.colrow;
                if cols < 1 || rows < 1 {
                    return err(format!(
                        "array reference with {cols} x {rows} instances in cell {name}"
                    ));
                }
                let (c, r) = (f64::from(cols), f64::from(rows));
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

fn add_shape(cell: &mut Cell, pts: &[Pt]) {
    cell.points.extend_from_slice(pts);
    cell.own = union(cell.own, points_box(pts));
}

/// Parse GDSII bytes with the default limits.
pub fn parse_gds(data: &[u8]) -> Result<GdsLibrary, GdsError> {
    parse_gds_with(data, &Limits::DEFAULT)
}

/// Parse GDSII bytes. Fails on a malformed stream, when a cell contains
/// itself, and when the file has too many records, elements or cells or nests
/// references too deep.
pub fn parse_gds_with(data: &[u8], limits: &Limits) -> Result<GdsLibrary, GdsError> {
    let mut cells: BTreeMap<String, Cell> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut um_per_dbu = 0.0;
    let mut cell: Option<String> = None;
    let mut pending: Option<Pending> = None;
    let mut pos = 0usize;
    let mut records = 0usize;
    let mut elements = 0usize;
    let mut ended = false;
    while pos < data.len() {
        let Some(&[l0, l1, rtype, _]) = data.get(pos..pos + 4) else {
            return err(format!("truncated record header at byte {pos}"));
        };
        let length = usize::from(u16::from_be_bytes([l0, l1]));
        let Some(body) = data.get(pos + 4..pos + length).filter(|_| length >= 4) else {
            return err(format!("bad record length {length} at byte {pos}"));
        };
        let at = pos;
        pos += length;
        records += 1;
        if records > limits.gds_records {
            return limit(format!(
                "the GDS has more than {} records",
                limits.gds_records
            ));
        }
        match rtype {
            0x03 => {
                let b: [u8; 16] = need(body, "UNITS")?;
                let mut meters = [0u8; 8];
                meters.copy_from_slice(&b[8..]);
                um_per_dbu = real8(&meters) * 1e6;
            }
            0x05 | 0x07 => cell = None,
            0x06 => {
                let name = name_of(body);
                if cells.contains_key(&name) {
                    return err(format!("cell {name} is defined twice"));
                }
                if cells.len() >= limits.gds_cells {
                    return limit(format!("the GDS has more than {} cells", limits.gds_cells));
                }
                cells.insert(name.clone(), Cell::default());
                order.push(name.clone());
                cell = Some(name);
            }
            0x08 | 0x09 | 0x0A | 0x0B | 0x0C | 0x15 | 0x2D => {
                if cell.is_none() {
                    return err(format!("element outside a cell at byte {at}"));
                }
                elements += 1;
                if elements > limits.gds_elements {
                    return limit(format!(
                        "the GDS has more than {} elements",
                        limits.gds_elements
                    ));
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
                let target = cell.as_ref().and_then(|n| cells.get_mut(n).map(|c| (n, c)));
                let (Some((name, target)), Some(p)) = (target, pending.take()) else {
                    return err(format!("ENDEL outside an element at byte {at}"));
                };
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
                    .map(|&[a, b, c, d, e, f, g, h]| {
                        (
                            f64::from(i32::from_be_bytes([a, b, c, d])),
                            f64::from(i32::from_be_bytes([e, f, g, h])),
                        )
                    })
                    .collect();
                if let Some(p) = pending.as_mut() {
                    p.xy = xy;
                }
            }
            // Element properties: checked wherever they appear, kept for the
            // element being read.
            0x0F => {
                let v = f64::from(i32::from_be_bytes(need(body, "WIDTH")?));
                if let Some(p) = pending.as_mut() {
                    p.width = v;
                }
            }
            0x21 => {
                let v = i16::from_be_bytes(need(body, "PATHTYPE")?);
                if let Some(p) = pending.as_mut() {
                    p.pathtype = v;
                }
            }
            0x30 | 0x31 => {
                let what = if rtype == 0x30 { "BGNEXTN" } else { "ENDEXTN" };
                let v = f64::from(i32::from_be_bytes(need(body, what)?));
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
                let v = u16::from_be_bytes(need(body, "STRANS")?);
                if let Some(p) = pending.as_mut() {
                    p.strans = v;
                }
            }
            0x1B | 0x1C => {
                let what = if rtype == 0x1B { "MAG" } else { "ANGLE" };
                let v = real8(&need(body, what)?);
                if let Some(p) = pending.as_mut() {
                    if rtype == 0x1B {
                        p.mag = v;
                    } else {
                        p.angle = v;
                    }
                }
            }
            0x13 => {
                let [a, b, c, d] = need(body, "COLROW")?;
                let v = (
                    i32::from(i16::from_be_bytes([a, b])),
                    i32::from(i16::from_be_bytes([c, d])),
                );
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
    check_hierarchy(&order, &cells, limits)?;
    Ok(GdsLibrary {
        um_per_dbu,
        limits: *limits,
        order,
        cells,
    })
}

/// Read and parse a GDS file; a file larger than the limit is an error.
pub fn read_gds(path: &Path) -> Result<GdsLibrary, GdsError> {
    let limits = Limits::DEFAULT;
    let data = read_capped(path, limits.gds_bytes).map_err(|e| match e {
        ReadError::TooLarge(m) => GdsError::Limit(LimitError(m)),
        ReadError::Io(e) => GdsError::Syntax(e.to_string()),
    })?;
    parse_gds_with(&data, &limits)
}
