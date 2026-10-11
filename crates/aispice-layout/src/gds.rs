//! A small GDSII reader: enough to know what a layout file holds before it
//! is handed to KLayout or Magic (its cells, which of them are top cells,
//! the layers in use, the text labels) and to refuse files that are not
//! GDS, too large, or nested without end.
//!
//! A layout file is untrusted input. The reader never allocates more than
//! the file it was given, caps names and nesting, and stops at the first
//! malformed record.

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Files larger than this are refused.
pub const MAX_BYTES: u64 = 256 << 20;
/// Cell references nest at most this deep.
pub const MAX_DEPTH: usize = 64;
/// Longest name or label kept.
const MAX_NAME: usize = 1024;
/// At most this many labels are kept per cell.
const MAX_TEXTS: usize = 10_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GdsError {
    #[error("could not read {0}")]
    Io(String),
    #[error("the file is larger than {} MiB", MAX_BYTES >> 20)]
    TooLarge,
    #[error("not a GDSII file: {0}")]
    NotGds(String),
    #[error("cell references nest deeper than {MAX_DEPTH} levels, or loop (at `{0}`)")]
    TooDeep(String),
}

/// A text label: what LVS reads as a net name.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Label {
    pub layer: i16,
    pub datatype: i16,
    pub text: String,
    /// Position in database units.
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Cell {
    pub name: String,
    /// Names of the cells this one places.
    pub children: BTreeSet<String>,
    pub layers: BTreeSet<(i16, i16)>,
    pub labels: Vec<Label>,
    /// Shapes drawn in the cell itself (boundaries, paths, boxes).
    pub shapes: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Library {
    pub name: String,
    /// The size of a database unit in metres (1e-9 for a 1 nm grid).
    pub dbu_m: f64,
    pub cells: BTreeMap<String, Cell>,
}

impl Library {
    /// Cells no other cell places, in name order.
    pub fn top_cells(&self) -> Vec<&str> {
        let placed: BTreeSet<&str> = self
            .cells
            .values()
            .flat_map(|c| c.children.iter().map(String::as_str))
            .collect();
        self.cells
            .keys()
            .map(String::as_str)
            .filter(|n| !placed.contains(n))
            .collect()
    }

    /// The one top cell, or why there is not exactly one.
    pub fn single_top(&self) -> Result<&str, String> {
        match self.top_cells().as_slice() {
            [one] => Ok(one),
            [] => Err("the layout has no top cell".into()),
            many => Err(format!(
                "the layout has {} top cells ({}); name the cell to check",
                many.len(),
                many.join(", ")
            )),
        }
    }

    /// Every label in a cell and the cells under it, with positions in
    /// database units relative to that cell's origin left out: LVS reads the
    /// names, not where they sit.
    pub fn labels_under(&self, cell: &str) -> Vec<&Label> {
        let mut out = Vec::new();
        let mut stack = vec![cell];
        let mut seen = BTreeSet::new();
        while let Some(c) = stack.pop() {
            if !seen.insert(c) {
                continue;
            }
            if let Some(cell) = self.cells.get(c) {
                out.extend(cell.labels.iter());
                stack.extend(cell.children.iter().map(String::as_str));
            }
        }
        out
    }
}

/// Read a GDSII file.
pub fn read(path: &Path) -> Result<Library, GdsError> {
    let meta = std::fs::metadata(path).map_err(|e| GdsError::Io(e.to_string()))?;
    if meta.len() > MAX_BYTES {
        return Err(GdsError::TooLarge);
    }
    let bytes = std::fs::read(path).map_err(|e| GdsError::Io(e.to_string()))?;
    parse(&bytes)
}

const HEADER: u8 = 0x00;
const BGNLIB: u8 = 0x01;
const LIBNAME: u8 = 0x02;
const UNITS: u8 = 0x03;
const ENDLIB: u8 = 0x04;
const BGNSTR: u8 = 0x05;
const STRNAME: u8 = 0x06;
const ENDSTR: u8 = 0x07;
const BOUNDARY: u8 = 0x08;
const PATH: u8 = 0x09;
const SREF: u8 = 0x0A;
const AREF: u8 = 0x0B;
const TEXT: u8 = 0x0C;
const LAYER: u8 = 0x0D;
const DATATYPE: u8 = 0x0E;
const XY: u8 = 0x10;
const ENDEL: u8 = 0x11;
const SNAME: u8 = 0x12;
const TEXTTYPE: u8 = 0x16;
const STRING: u8 = 0x19;
const BOX: u8 = 0x2D;

/// Parse GDSII bytes.
pub fn parse(bytes: &[u8]) -> Result<Library, GdsError> {
    let bad = |m: &str| GdsError::NotGds(m.to_string());
    let mut lib = Library::default();
    let mut pos = 0usize;
    let mut first = true;
    let mut cell: Option<Cell> = None;
    // The element being read: its kind and what it has said so far.
    let mut element: Option<u8> = None;
    let (mut layer, mut datatype, mut xy) = (0i16, 0i16, (0i32, 0i32));
    let mut text = String::new();
    let mut ended = false;
    while pos + 4 <= bytes.len() {
        let len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        let kind = bytes[pos + 2];
        if len < 4 || pos + len > bytes.len() {
            // Some writers pad the file with zeros after ENDLIB.
            if ended && bytes[pos..].iter().all(|b| *b == 0) {
                break;
            }
            return Err(bad("a record runs past the end of the file"));
        }
        let data = &bytes[pos + 4..pos + len];
        pos += len;
        if first {
            if kind != HEADER {
                return Err(bad("it does not start with a HEADER record"));
            }
            first = false;
            continue;
        }
        match kind {
            BGNLIB | HEADER => {}
            LIBNAME => lib.name = string(data),
            UNITS => {
                if data.len() < 16 {
                    return Err(bad("short UNITS record"));
                }
                lib.dbu_m = real8(&data[8..16]);
            }
            ENDLIB => {
                ended = true;
                break;
            }
            BGNSTR => cell = Some(Cell::default()),
            STRNAME => {
                if let Some(c) = cell.as_mut() {
                    c.name = string(data);
                }
            }
            ENDSTR => {
                let c = cell.take().ok_or_else(|| bad("ENDSTR outside a cell"))?;
                if c.name.is_empty() {
                    return Err(bad("a cell has no name"));
                }
                lib.cells.insert(c.name.clone(), c);
            }
            BOUNDARY | PATH | BOX | SREF | AREF | TEXT => {
                element = Some(kind);
                text.clear();
            }
            LAYER => layer = int16(data),
            DATATYPE | TEXTTYPE => datatype = int16(data),
            XY => {
                if data.len() >= 8 {
                    xy = (
                        i32::from_be_bytes([data[0], data[1], data[2], data[3]]),
                        i32::from_be_bytes([data[4], data[5], data[6], data[7]]),
                    );
                }
            }
            SNAME | STRING => text = string(data),
            ENDEL => {
                let c = cell
                    .as_mut()
                    .ok_or_else(|| bad("an element outside a cell"))?;
                match element.take() {
                    Some(SREF | AREF) => {
                        c.children.insert(std::mem::take(&mut text));
                    }
                    Some(TEXT) => {
                        if c.labels.len() < MAX_TEXTS {
                            c.labels.push(Label {
                                layer,
                                datatype,
                                text: std::mem::take(&mut text),
                                x: xy.0,
                                y: xy.1,
                            });
                        }
                        c.layers.insert((layer, datatype));
                    }
                    Some(_) => {
                        c.shapes += 1;
                        c.layers.insert((layer, datatype));
                    }
                    None => return Err(bad("ENDEL without an element")),
                }
            }
            _ => {}
        }
    }
    if first {
        return Err(bad("the file is empty"));
    }
    if !ended {
        return Err(bad("there is no ENDLIB record"));
    }
    check_depth(&lib)?;
    Ok(lib)
}

/// Refuse reference chains deeper than [`MAX_DEPTH`], which also catches
/// a cell that places itself.
fn check_depth(lib: &Library) -> Result<(), GdsError> {
    fn depth<'a>(
        lib: &'a Library,
        name: &'a str,
        on_path: &mut Vec<&'a str>,
        done: &mut BTreeMap<&'a str, usize>,
    ) -> Result<usize, GdsError> {
        if let Some(d) = done.get(name) {
            return Ok(*d);
        }
        if on_path.len() >= MAX_DEPTH || on_path.contains(&name) {
            return Err(GdsError::TooDeep(name.to_string()));
        }
        on_path.push(name);
        let mut d = 0;
        if let Some(c) = lib.cells.get(name) {
            for child in &c.children {
                d = d.max(1 + depth(lib, child, on_path, done)?);
            }
        }
        on_path.pop();
        done.insert(name, d);
        Ok(d)
    }
    let mut done = BTreeMap::new();
    for name in lib.cells.keys() {
        depth(lib, name, &mut Vec::new(), &mut done)?;
    }
    Ok(())
}

fn int16(data: &[u8]) -> i16 {
    if data.len() >= 2 {
        i16::from_be_bytes([data[0], data[1]])
    } else {
        0
    }
}

fn string(data: &[u8]) -> String {
    let end = data.iter().position(|b| *b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end.min(MAX_NAME)]).into_owned()
}

/// A GDSII eight-byte real: sign, excess-64 base-16 exponent, 56-bit
/// mantissa.
fn real8(b: &[u8]) -> f64 {
    let sign = if b[0] & 0x80 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from(b[0] & 0x7f) - 64;
    let mut mantissa = 0u64;
    for byte in &b[1..8] {
        mantissa = (mantissa << 8) | u64::from(*byte);
    }
    sign * (mantissa as f64 / (1u64 << 56) as f64) * 16f64.powi(exp)
}

/// Writes small GDSII files for tests: rectangles, references and labels.
#[doc(hidden)]
pub mod testwrite {
    /// A cell to write.
    #[derive(Debug, Clone, Default)]
    pub struct TestCell {
        pub name: String,
        /// (layer, datatype, x1, y1, x2, y2) in database units.
        pub rects: Vec<(i16, i16, i32, i32, i32, i32)>,
        /// (cell name, x, y).
        pub refs: Vec<(String, i32, i32)>,
        /// (layer, texttype, text, x, y).
        pub labels: Vec<(i16, i16, String, i32, i32)>,
    }

    fn record(out: &mut Vec<u8>, kind: u8, dt: u8, data: &[u8]) {
        let len = (data.len() + 4) as u16;
        out.extend_from_slice(&len.to_be_bytes());
        out.push(kind);
        out.push(dt);
        out.extend_from_slice(data);
    }

    fn name(s: &str) -> Vec<u8> {
        let mut b = s.as_bytes().to_vec();
        if b.len() % 2 == 1 {
            b.push(0);
        }
        b
    }

    /// Encode a GDSII eight-byte real.
    pub fn real8(v: f64) -> [u8; 8] {
        if v == 0.0 {
            return [0; 8];
        }
        let (sign, mut m) = if v < 0.0 { (0x80u8, -v) } else { (0, v) };
        let mut exp = 64i32;
        while m >= 1.0 {
            m /= 16.0;
            exp += 1;
        }
        while m < 1.0 / 16.0 {
            m *= 16.0;
            exp -= 1;
        }
        let mantissa = (m * (1u64 << 56) as f64).round() as u64;
        let mut out = [0u8; 8];
        out[0] = sign | (exp as u8 & 0x7f);
        out[1..].copy_from_slice(&mantissa.to_be_bytes()[1..]);
        out
    }

    /// GDSII bytes for a library of cells with a 1 nm database unit.
    pub fn library(cells: &[TestCell]) -> Vec<u8> {
        let mut o = Vec::new();
        record(&mut o, 0x00, 0x02, &600i16.to_be_bytes());
        record(&mut o, 0x01, 0x02, &[0; 24]);
        record(&mut o, 0x02, 0x06, &name("LIB"));
        let mut units = Vec::new();
        units.extend_from_slice(&real8(1e-3));
        units.extend_from_slice(&real8(1e-9));
        record(&mut o, 0x03, 0x05, &units);
        for c in cells {
            record(&mut o, 0x05, 0x02, &[0; 24]);
            record(&mut o, 0x06, 0x06, &name(&c.name));
            for &(layer, dt, x1, y1, x2, y2) in &c.rects {
                record(&mut o, 0x08, 0x00, &[]);
                record(&mut o, 0x0D, 0x02, &layer.to_be_bytes());
                record(&mut o, 0x0E, 0x02, &dt.to_be_bytes());
                let mut xy = Vec::new();
                for (x, y) in [(x1, y1), (x2, y1), (x2, y2), (x1, y2), (x1, y1)] {
                    xy.extend_from_slice(&x.to_be_bytes());
                    xy.extend_from_slice(&y.to_be_bytes());
                }
                record(&mut o, 0x10, 0x03, &xy);
                record(&mut o, 0x11, 0x00, &[]);
            }
            for (child, x, y) in &c.refs {
                record(&mut o, 0x0A, 0x00, &[]);
                record(&mut o, 0x12, 0x06, &name(child));
                let mut xy = Vec::new();
                xy.extend_from_slice(&x.to_be_bytes());
                xy.extend_from_slice(&y.to_be_bytes());
                record(&mut o, 0x10, 0x03, &xy);
                record(&mut o, 0x11, 0x00, &[]);
            }
            for (layer, tt, text, x, y) in &c.labels {
                record(&mut o, 0x0C, 0x00, &[]);
                record(&mut o, 0x0D, 0x02, &layer.to_be_bytes());
                record(&mut o, 0x16, 0x02, &tt.to_be_bytes());
                let mut xy = Vec::new();
                xy.extend_from_slice(&x.to_be_bytes());
                xy.extend_from_slice(&y.to_be_bytes());
                record(&mut o, 0x10, 0x03, &xy);
                record(&mut o, 0x19, 0x06, &name(text));
                record(&mut o, 0x11, 0x00, &[]);
            }
            record(&mut o, 0x07, 0x00, &[]);
        }
        record(&mut o, 0x04, 0x00, &[]);
        o
    }
}

#[cfg(test)]
mod tests {
    use super::testwrite::{TestCell, library};
    use super::*;

    fn cell(name: &str) -> TestCell {
        TestCell {
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn reads_cells_tops_layers_and_labels() {
        let mut leaf = cell("leaf");
        leaf.rects.push((8, 0, 0, 0, 160, 1000));
        leaf.labels.push((8, 25, "vout".into(), 80, 500));
        let mut top = cell("top");
        top.refs.push(("leaf".into(), 0, 0));
        top.refs.push(("leaf".into(), 500, 0));
        top.rects.push((10, 0, 0, 0, 200, 200));
        let lib = parse(&library(&[leaf, top, cell("stray")])).unwrap();
        assert!((lib.dbu_m - 1e-9).abs() < 1e-18, "{}", lib.dbu_m);
        assert_eq!(lib.top_cells(), ["stray", "top"]);
        assert!(lib.single_top().unwrap_err().contains("2 top cells"));
        assert_eq!(
            lib.cells["leaf"].layers.iter().collect::<Vec<_>>(),
            [&(8, 0), &(8, 25)]
        );
        assert_eq!(lib.cells["top"].shapes, 1);
        let labels = lib.labels_under("top");
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].text, "vout");
    }

    #[test]
    fn refuses_what_is_not_gds() {
        assert!(matches!(parse(b""), Err(GdsError::NotGds(_))));
        assert!(matches!(
            parse(b"%PDF-1.7 not a layout"),
            Err(GdsError::NotGds(_))
        ));
        let mut truncated = library(&[cell("a")]);
        truncated.truncate(truncated.len() - 2);
        assert!(parse(&truncated).is_err());
    }

    #[test]
    fn refuses_cycles_and_deep_nesting() {
        let mut a = cell("a");
        a.refs.push(("b".into(), 0, 0));
        let mut b = cell("b");
        b.refs.push(("a".into(), 0, 0));
        assert!(matches!(
            parse(&library(&[a, b])),
            Err(GdsError::TooDeep(_))
        ));
        let chain: Vec<TestCell> = (0..=MAX_DEPTH + 1)
            .map(|i| {
                let mut c = cell(&format!("c{i}"));
                if i <= MAX_DEPTH {
                    c.refs.push((format!("c{}", i + 1), 0, 0));
                }
                c
            })
            .collect();
        assert!(matches!(parse(&library(&chain)), Err(GdsError::TooDeep(_))));
    }

    #[test]
    fn eight_byte_reals_round_trip() {
        for v in [1e-3, 1e-9, 0.5, 1.0, 2.5e-7, -3.75] {
            let back = real8(&super::testwrite::real8(v));
            assert!((back - v).abs() <= 1e-12 * v.abs(), "{v} -> {back}");
        }
    }
}
