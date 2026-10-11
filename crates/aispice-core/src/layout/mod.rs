//! Drawing a schematic from a SPICE netlist.
//!
//! Legible schematic generation is where netlist-to-drawing tools usually
//! fall down: parts piled on each other, wires through bodies, op-amps drawn
//! backwards. This module draws the way an engineer does. Each element is
//! mapped to the LTspice symbol for it (or kept as a SPICE line in a
//! directive when no symbol can represent it exactly). Parts are placed by
//! following the signal from the input source to the output, with supply
//! rails as labels at the top and bottom, ground symbols under grounded pins,
//! series parts along the path and shunt parts hanging from it, feedback
//! above the stage it wraps. A router that follows LTspice's connection rules
//! wires short connections and labels long ones. Several candidate layouts
//! are drawn and measured with [`quality`], and the best one is kept.
//!
//! The result is checked by construction and again at the end: the drawing
//! is netlisted the way LTspice does it and compared with the input. A
//! mismatch is an error, never a wrong schematic.

mod circuit;
mod geom;
pub(crate) mod maze;
mod place;
mod quality;
mod symbols;
mod text;
mod verify;
mod wire;

pub use quality::{Quality, quality};
pub use verify::compare_netlists;

pub(crate) use geom::{Dir, centre, core_body, pin_facing, place_rect, segment_hits};
pub(crate) use text::{flag_box, symbol_texts};

use crate::geometry::{GRID, Point, Rect};
use crate::netlist::{Line, Netlist};
use crate::schematic::{Item, Schematic, Sheet, Text};
use crate::symbol::SymbolLibrary;
use place::{Placement, Strategy};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How to lay a netlist out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct LayoutOptions {
    /// How many candidate layouts to try: 1 is quick, 3 searches widely.
    pub effort: u8,
    /// Write the netlist's title line on the sheet as a comment.
    pub title_comment: bool,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            effort: 2,
            title_comment: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LayoutResult {
    pub schematic: Schematic,
    /// Elements kept as SPICE lines, nets renamed, and similar notes.
    pub warnings: Vec<String>,
    pub quality: Quality,
    /// Automatically numbered nets the drawing labels under a new name:
    /// (name in the netlist, name on the sheet).
    pub renames: Vec<(String, String)>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum LayoutError {
    #[error("the netlist has no elements to draw")]
    Empty,
    #[error("the drawing would not netlist back to the input, so none is returned: {}", .0.join("; "))]
    Mismatch(Vec<String>),
}

/// A finished candidate layout: its rank, the drawing, its metrics and the
/// nets it had to rename.
type Candidate = (f64, Schematic, Quality, Vec<(usize, String)>);

/// Lay a netlist out as a schematic LTspice can open and a person can read.
pub fn from_netlist(
    netlist: &Netlist,
    lib: &SymbolLibrary,
    opts: &LayoutOptions,
) -> Result<LayoutResult, LayoutError> {
    let c = circuit::build(netlist, lib);
    if c.devices.is_empty() && c.fallbacks.is_empty() {
        return Err(LayoutError::Empty);
    }
    let title = opts
        .title_comment
        .then(|| netlist.title.trim())
        .filter(|t| !t.is_empty() && !t.starts_with('*') && !t.contains(":\\"))
        .map(str::to_string);

    let mut best: Option<Candidate> = None;
    let mut problems = Vec::new();
    for st in strategies(&c, opts.effort) {
        let (placed, conventions) = place::place(&c, &st);
        let wiring = wire::wire(&c, &placed);
        let sch = emit(&c, &placed, &wiring, title.as_deref());
        let q = quality(&sch, lib);
        let expected = renamed(netlist, &c, &wiring.renames);
        match verify::round_trip(&expected, &sch, lib) {
            Ok(()) => {
                // Drawing conventions the metrics cannot see (signal flowing
                // left to right, ground pins down) break ties between
                // otherwise clean candidates.
                let rank = rank(&q, &sch, &c) - conventions * 0.5;
                if best.as_ref().is_none_or(|(r, ..)| rank > *r) {
                    best = Some((rank, sch, q, wiring.renames.clone()));
                }
            }
            Err(p) => problems = p,
        }
    }
    let Some((_, schematic, quality, renames)) = best else {
        return Err(LayoutError::Mismatch(problems));
    };
    let mut warnings = c.warnings.clone();
    for (n, name) in &renames {
        warnings.push(format!(
            "net {} is joined by labels and is called `{name}` on the sheet",
            c.nets[*n].name
        ));
    }
    Ok(LayoutResult {
        schematic,
        warnings,
        quality,
        renames: renames
            .into_iter()
            .map(|(n, name)| (c.nets[n].name.clone(), name))
            .collect(),
    })
}

/// Higher is better: the score first, then fewer labels standing in for
/// wires, fewer corners, less wire and less area.
fn rank(q: &Quality, sch: &Schematic, c: &circuit::Circuit) -> f64 {
    let mut counts: std::collections::HashMap<String, usize> = Default::default();
    for f in sch.flags().filter(|f| !f.is_ground()) {
        *counts.entry(f.label.to_ascii_uppercase()).or_default() += 1;
    }
    // Rails are labelled at every pin by design; a signal net labelled twice
    // is a wire the router gave up on.
    let extra: usize = counts
        .iter()
        .filter(|(name, _)| !c.nets.iter().any(|n| c.is_rail_name(n, name)))
        .map(|(_, k)| k - 1)
        .sum();
    q.score
        - extra as f64 * 4.0
        - q.labels as f64 * 0.2
        - q.bends as f64 * 0.4
        - q.wire_length as f64 / 3000.0
        - q.area as f64 / 1.5e6
}

/// The input with automatically named nets renamed the way the drawing
/// labels them, which is what its netlist must match.
fn renamed(n: &Netlist, c: &circuit::Circuit, renames: &[(usize, String)]) -> Netlist {
    if renames.is_empty() {
        return n.clone();
    }
    let mut out = n.clone();
    for item in out.items.iter_mut() {
        if let Line::Element(e) = item {
            for node in e.nodes.iter_mut() {
                if let Some((_, name)) = renames
                    .iter()
                    .find(|(i, _)| c.nets[*i].name.eq_ignore_ascii_case(node))
                {
                    *node = name.clone();
                }
            }
        }
    }
    out
}

/// The candidate layouts to try: the default, the other order of placing
/// active and shunt parts, feedback below, and flips of the active parts.
fn strategies(c: &circuit::Circuit, effort: u8) -> Vec<Strategy> {
    use crate::geometry::Orient::*;
    let n = c.devices.len();
    let base = Strategy {
        orient: vec![None; n],
        ..Strategy::default()
    };
    let mut out = vec![base.clone()];
    if effort == 0 {
        return out;
    }
    out.push(Strategy {
        actives_first: true,
        ..base.clone()
    });
    if c.feedback.iter().any(|f| *f) {
        out.push(Strategy {
            feedback_below: true,
            ..base.clone()
        });
        out.push(Strategy {
            feedback_first: true,
            ..base.clone()
        });
    }
    // Start from the active part nearest the input, with and without
    // feedback placed first: the stage takes its natural shape and the
    // sources fit around it.
    let seed = (0..n)
        .filter(|&i| c.devices[i].kind.active())
        .min_by_key(|&i| {
            let depth = c.devices[i]
                .pins
                .iter()
                .filter(|p| p.role == circuit::PinRole::Input)
                .filter_map(|p| c.nets[p.net].depth)
                .min()
                .unwrap_or(u32::MAX);
            (depth, i)
        });
    if seed.is_some() {
        out.push(Strategy {
            seed,
            feedback_first: true,
            ..base.clone()
        });
        if effort >= 2 {
            out.push(Strategy {
                seed,
                ..base.clone()
            });
        }
    }
    // Transistor circuits: current paths stood up as columns between the
    // rails, the rest fitted around them.
    let transistors = c.devices.iter().filter(|d| d.kind.transistor()).count();
    if transistors >= 3 {
        out.push(Strategy {
            columns: true,
            ..base.clone()
        });
        out.push(Strategy {
            columns: true,
            spread: 1,
            ..base.clone()
        });
    }
    if effort >= 2 {
        // Flip active parts between their two natural orientations.
        let flippable: Vec<(usize, [crate::geometry::Orient; 2])> = c
            .devices
            .iter()
            .enumerate()
            .filter_map(|(i, d)| match d.kind {
                circuit::Kind::OpAmp | circuit::Kind::Controlled => Some((i, [R0, M180])),
                circuit::Kind::Bjt { p: false } | circuit::Kind::Fet { p: false } => {
                    Some((i, [R0, M0]))
                }
                circuit::Kind::Bjt { p: true } | circuit::Kind::Fet { p: true } => {
                    Some((i, [M180, R180]))
                }
                _ => None,
            })
            .collect();
        let k = flippable.len().min(if effort >= 3 { 5 } else { 3 });
        for mask in 1u32..(1 << k) {
            let mut st = base.clone();
            for (bit, (i, pair)) in flippable.iter().take(k).enumerate() {
                let o = pair[((mask >> bit) & 1) as usize];
                st.orient[*i] = Some(vec![o]);
            }
            out.push(st);
        }
        out.push(Strategy {
            spread: 1,
            ..base.clone()
        });
    }
    if effort >= 3 {
        let more: Vec<Strategy> = out
            .iter()
            .map(|s| Strategy {
                actives_first: !s.actives_first,
                ..s.clone()
            })
            .collect();
        out.extend(more);
    }
    out
}

/// The schematic for a placement and its wiring, with directives below.
fn emit(
    c: &circuit::Circuit,
    placed: &[Placement],
    wiring: &wire::Wiring,
    title: Option<&str>,
) -> Schematic {
    let mut sch = Schematic::new();
    let mut bounds: Option<Rect> = None;
    let mut add = |r: Rect| bounds = Some(bounds.map_or(r, |b| b.union(r)));
    let mut symbols = Vec::new();
    for (d, pl) in c.devices.iter().zip(placed) {
        let s = symbols::symbol(d, pl.at, pl.orient, pl.flip_text);
        if let Some(b) = d.def.placed_bounds(pl.at, pl.orient) {
            add(b);
        }
        for t in text::symbol_texts(&s, &d.def) {
            add(t);
        }
        symbols.push(s);
    }
    for w in &wiring.wires {
        add(Rect::from_points(w.a, w.b));
    }
    for f in &wiring.flags {
        add(Rect::from_points(
            f.at.offset(-16, -16),
            f.at.offset(16, 24),
        ));
    }
    let b = bounds.unwrap_or(Rect::from_points(Point::new(0, 0), Point::new(0, 0)));
    // Move the drawing so its top-left corner sits near the sheet origin.
    let shift = Point::new(
        64 - b.min.x.div_euclid(GRID) * GRID,
        64 - b.min.y.div_euclid(GRID) * GRID,
    );
    for w in &wiring.wires {
        sch.insert(Item::Wire(crate::schematic::Wire::new(
            w.a + shift,
            w.b + shift,
        )));
    }
    for f in &wiring.flags {
        sch.insert(Item::Flag(crate::schematic::Flag {
            at: f.at + shift,
            label: f.label.clone(),
        }));
    }
    for mut s in symbols {
        s.at = s.at + shift;
        sch.insert(Item::Symbol(s));
    }
    let left = b.min.x + shift.x;
    let mut y = b.max.y + shift.y + 3 * GRID;
    let mut texts: Vec<Text> = Vec::new();
    if let Some(t) = title {
        texts.push(Text::comment(Point::new(left, y), circuit::escape(t)));
        y += 2 * GRID;
    }
    for t in &c.texts {
        // Left-justified text is centred on its anchor, block and all.
        let lines = t.lines().count().max(1) as i32;
        let anchor = crate::geometry::snap(y + lines * 12);
        texts.push(Text::directive(
            Point::new(left, anchor),
            circuit::escape(t),
        ));
        y = anchor + lines * 12 + GRID;
    }
    for t in texts {
        sch.insert(Item::Text(t));
    }
    let right = b.max.x + shift.x;
    sch.sheet = Sheet {
        number: 1,
        width: (right + 128).max(880),
        height: (y + 64).max(680),
    };
    sch
}

#[cfg(test)]
mod tests;
