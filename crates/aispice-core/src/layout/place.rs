//! Placing parts so the drawing reads like a textbook schematic.
//!
//! Parts are placed one at a time, growing out from the input source the way
//! the signal travels. For each part every reasonable orientation, text side
//! and nearby grid position is scored, and the cheapest spot that keeps clear
//! of what is already placed wins. The score is the drawing conventions
//! written down: short, straight connections to what is already on the
//! part's nets; ground pins facing down and positive rails facing up; signal
//! running left to right; series parts lying along the path and shunt parts
//! standing between a node and ground; feedback parts above the stage they
//! wrap; no predicted crossings.
//!
//! Each connection the placer expects to draw is remembered as a sketch of
//! straight segments, so a later part can plan to tee into that wire rather
//! than only aiming at pins, which is how a feedback resistor finds the wire
//! into an op-amp input.

use super::circuit::{Circuit, Kind, NetKind, PinRole};
use super::geom::{Dir, core_body, orient_dir, overlaps, place_rect, segment_hits};
use super::symbols::{orientations, symbol, text_can_flip};
use super::text::symbol_texts;
use crate::geometry::{GRID, Orient, Point, Rect, snap};
use std::collections::HashSet;

/// Space kept between the zones (body, text, flags) of neighbouring parts.
const GAP: i32 = 24;

/// Convention weights, in the same unit as wire estimates (grid steps). They
/// are set so a convention outweighs a few steps of wire: a person accepts a
/// slightly longer wire to keep ground at the bottom and the signal flowing
/// right.
const W_SIDEWAYS: f64 = 10.0;
const W_BACKWARD: f64 = 10.0;
const W_UPRIGHT_SERIES: f64 = 10.0;
const W_FEEDBACK_SIDE: f64 = 8.0;
const W_CROSSING: f64 = 6.0;
const W_THROUGH_BODY: f64 = 12.0;
const W_THROUGH_TEXT: f64 = 4.0;

/// Choices a candidate layout makes differently from another.
#[derive(Debug, Clone, Default)]
pub(crate) struct Strategy {
    /// Allowed orientations per device, overriding the defaults.
    pub orient: Vec<Option<Vec<Orient>>>,
    /// Put feedback parts below the stage instead of above.
    pub feedback_below: bool,
    /// Place active parts before the shunt parts on the same node.
    pub actives_first: bool,
    /// Extra spacing between parts, in grid steps.
    pub spread: i32,
    /// Place a feedback part as soon as both its ends are on the sheet, so
    /// the parts hanging off its node are placed around it.
    pub feedback_first: bool,
    /// Start from this device instead of the input source: the core of the
    /// circuit first, its sources fitted around it afterwards.
    pub seed: Option<usize>,
    /// Stand the rail-to-rail current paths up as columns first.
    pub columns: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Placement {
    pub at: Point,
    pub orient: Orient,
    /// Name and value on the other side of the body.
    pub flip_text: bool,
}

/// A pin already on the sheet.
#[derive(Debug, Clone, Copy)]
struct PinAt {
    p: Point,
    f: Dir,
}

/// Geometry of one device in one orientation, relative to its origin.
#[derive(Debug, Clone)]
struct Shape {
    orient: Orient,
    flip_text: bool,
    pref: f64,
    pins: Vec<(Point, Dir)>,
    zone: Rect,
    body: Rect,
    /// Name and value texts and the flags at its pins: wires should not run
    /// through them.
    texts: Vec<Rect>,
}

struct Placer<'a> {
    c: &'a Circuit,
    st: &'a Strategy,
    shapes: Vec<Vec<Shape>>,
    pos: Vec<Option<Placement>>,
    /// Shape index chosen for each placed device.
    chosen: Vec<usize>,
    /// Devices in the order they were placed.
    order: Vec<usize>,
    zones: Vec<Option<Rect>>,
    bodies: Vec<Option<Rect>>,
    texts: Vec<Vec<Rect>>,
    net_pins: Vec<Vec<PinAt>>,
    /// Planned wires per net, as straight segments.
    net_segs: Vec<Vec<(Point, Point)>>,
    /// Devices placed by the column plan, which refinement leaves alone.
    fixed: Vec<bool>,
}

/// Room a flag needs beside a pin: a ground symbol below it, or a rail label
/// in the direction the pin faces.
fn flag_zone(p: Point, f: Dir, kind: NetKind, name: &str) -> Option<Rect> {
    match kind {
        NetKind::Ground => Some(match f {
            Dir::Down => Rect::from_points(p.offset(-16, 0), p.offset(16, 24)),
            Dir::Left | Dir::Right => {
                let s = f.step(p, 32);
                Rect::from_points(
                    Point::new(p.x.min(s.x) - 16, p.y),
                    Point::new(p.x.max(s.x) + 16, p.y + 40),
                )
            }
            Dir::Up => Rect::from_points(p.offset(-48, -40), p.offset(48, 8)),
        }),
        NetKind::Rail { .. } => {
            let len = (name.chars().count() as i32 * 12).max(24) + 8;
            Some(match f {
                Dir::Up => Rect::from_points(p.offset(-10, -len), p.offset(10, 0)),
                Dir::Down => Rect::from_points(p.offset(-10, 0), p.offset(10, len)),
                Dir::Left => Rect::from_points(p.offset(-len - 16, -10), p.offset(0, 10)),
                Dir::Right => Rect::from_points(p.offset(0, -10), p.offset(len + 16, 10)),
            })
        }
        NetKind::Signal => None,
    }
}

fn translate(r: Rect, by: Point) -> Rect {
    Rect {
        min: r.min + by,
        max: r.max + by,
    }
}

/// Fewest corners on a path that leaves heading `a` and arrives moving `b`,
/// with `d` the offset from start to end in grid steps, plus any detour it
/// forces.
fn bends(a: Dir, b: Dir, d: (i32, i32)) -> (u32, i32) {
    let along = |dir: Dir| {
        let (x, y) = dir.vec();
        x * d.0 + y * d.1
    };
    let perp_zero = |dir: Dir| {
        if dir.horizontal() { d.1 == 0 } else { d.0 == 0 }
    };
    if a == b {
        if along(a) > 0 && perp_zero(a) {
            (0, 0)
        } else if along(a) > 0 {
            (2, 0)
        } else {
            (4, 4)
        }
    } else if a == b.opposite() {
        if perp_zero(a) { (4, 4) } else { (2, 2) }
    } else if along(a) > 0 && along(b) > 0 {
        (1, 0)
    } else {
        (3, 2)
    }
}

/// The path a wire would most likely take from `p` (leaving along `a`) to
/// `q` (arriving moving along `b`), as corner points, when it has at most two
/// corners.
fn sketch_path(p: Point, a: Dir, q: Point, b: Dir, n: u32) -> Vec<Point> {
    match n {
        0 => vec![p, q],
        1 => {
            let corner = if a.horizontal() {
                Point::new(q.x, p.y)
            } else {
                Point::new(p.x, q.y)
            };
            vec![p, corner, q]
        }
        2 if a == b => {
            if a.horizontal() {
                let mx = snap((p.x + q.x) / 2);
                vec![p, Point::new(mx, p.y), Point::new(mx, q.y), q]
            } else {
                let my = snap((p.y + q.y) / 2);
                vec![p, Point::new(p.x, my), Point::new(q.x, my), q]
            }
        }
        2 => {
            if a.horizontal() {
                let x = if a == Dir::Right {
                    p.x.max(q.x) + 2 * GRID
                } else {
                    p.x.min(q.x) - 2 * GRID
                };
                vec![p, Point::new(x, p.y), Point::new(x, q.y), q]
            } else {
                let y = if a == Dir::Down {
                    p.y.max(q.y) + 2 * GRID
                } else {
                    p.y.min(q.y) - 2 * GRID
                };
                vec![p, Point::new(p.x, y), Point::new(q.x, y), q]
            }
        }
        _ => Vec::new(),
    }
}

/// Estimated cost, in grid steps, of a wire from pin `p` facing `fp` to `q`,
/// arriving in one of `arrive` directions: its length, two and a half steps
/// per corner, and any detour. Returns the sketch of the cheapest shape.
fn estimate(p: Point, fp: Dir, q: Point, arrive: &[Dir]) -> (f64, Vec<Point>) {
    let d = ((q.x - p.x) / GRID, (q.y - p.y) / GRID);
    if d == (0, 0) {
        return (4.0, vec![p]);
    }
    let len = d.0.abs() + d.1.abs();
    // Pins touching or nearly touching read as a crowded drawing.
    let short = if len < 2 { 3.0 } else { 0.0 };
    let mut best = (f64::INFINITY, Vec::new());
    for &b in arrive {
        let (n, detour) = bends(fp, b, d);
        // A Z with a tiny offset reads as a crooked wire, not a deliberate
        // route: pins that nearly line up should line up.
        let jog = if n == 2 && fp == b {
            let off = if fp.horizontal() {
                d.1.abs()
            } else {
                d.0.abs()
            };
            if off <= 2 { 4.0 } else { 0.0 }
        } else {
            0.0
        };
        let cost = len as f64 + detour as f64 + 2.5 * n as f64 + short + jog;
        if cost < best.0 {
            best = (cost, sketch_path(p, fp, q, b, n));
        }
    }
    best
}

/// The point of segment `s` nearest to `p`.
fn project(p: Point, s: (Point, Point)) -> Point {
    let (a, b) = s;
    Point::new(
        p.x.clamp(a.x.min(b.x), a.x.max(b.x)),
        p.y.clamp(a.y.min(b.y), a.y.max(b.y)),
    )
}

/// Whether two axis-aligned segments cross at a point inside both.
fn crosses(a: (Point, Point), b: (Point, Point)) -> bool {
    let (h, v) = if a.0.y == a.1.y && b.0.x == b.1.x {
        (a, b)
    } else if b.0.y == b.1.y && a.0.x == a.1.x {
        (b, a)
    } else {
        return false;
    };
    let (x0, x1) = (h.0.x.min(h.1.x), h.0.x.max(h.1.x));
    let (y0, y1) = (v.0.y.min(v.1.y), v.0.y.max(v.1.y));
    v.0.x > x0 && v.0.x < x1 && h.0.y > y0 && h.0.y < y1
}

impl<'a> Placer<'a> {
    fn new(c: &'a Circuit, st: &'a Strategy) -> Self {
        let mut shapes = Vec::new();
        for (i, d) in c.devices.iter().enumerate() {
            let allowed = st.orient.get(i).cloned().flatten();
            let mut list = Vec::new();
            for (o, pref) in orientations(d) {
                if allowed.as_ref().is_some_and(|a| !a.contains(&o)) {
                    continue;
                }
                let flips: &[bool] = if text_can_flip(d, o) {
                    &[false, true]
                } else {
                    &[false]
                };
                for &flip in flips {
                    // The input source sits at the left edge: its text reads
                    // best on the outside, leaving room for the first stage.
                    let side = match (Some(i) == c.input, flip) {
                        (true, true) | (false, false) => 0.0,
                        (true, false) => 0.5,
                        (false, true) => {
                            if d.kind.transistor() {
                                2.0
                            } else {
                                1.0
                            }
                        }
                    };
                    list.push(Self::shape(c, i, o, flip, pref + side));
                }
            }
            shapes.push(list);
        }
        Placer {
            c,
            st,
            shapes,
            pos: vec![None; c.devices.len()],
            chosen: vec![0; c.devices.len()],
            order: Vec::new(),
            zones: vec![None; c.devices.len()],
            bodies: vec![None; c.devices.len()],
            texts: vec![Vec::new(); c.devices.len()],
            net_pins: vec![Vec::new(); c.nets.len()],
            net_segs: vec![Vec::new(); c.nets.len()],
            fixed: vec![false; c.devices.len()],
        }
    }

    fn shape(c: &Circuit, i: usize, o: Orient, flip: bool, pref: f64) -> Shape {
        let d = &c.devices[i];
        let origin = Point::new(0, 0);
        let pins: Vec<(Point, Dir)> = d
            .pins
            .iter()
            .map(|p| (o.apply(p.at), orient_dir(p.facing, o)))
            .collect();
        let sym = symbol(d, origin, o, flip);
        let body = core_body(&d.def)
            .map(|b| place_rect(b, origin, o))
            .unwrap_or(Rect::from_points(origin, origin));
        let mut zone = body;
        let mut texts = symbol_texts(&sym, &d.def);
        for (k, &(p, f)) in pins.iter().enumerate() {
            zone.include(p);
            let net = &c.nets[d.pins[k].net];
            if let Some(z) = flag_zone(p, f, net.kind, &net.name) {
                // The flag's own pin sits on the zone's edge; only what lies
                // beyond it should keep wires away.
                texts.push(z);
            }
        }
        for t in &texts {
            zone = zone.union(*t);
        }
        Shape {
            orient: o,
            flip_text: flip,
            pref,
            pins,
            zone,
            body,
            texts,
        }
    }

    fn gap(&self) -> i32 {
        GAP + self.st.spread * GRID
    }

    fn fits(&self, zone: Rect) -> bool {
        let z = zone.inflate(self.gap() / 2);
        !self
            .zones
            .iter()
            .flatten()
            .any(|o| overlaps(&z, &o.inflate(self.gap() / 2)))
    }

    /// The cheapest planned connection from pin `p` to net `net` as placed so
    /// far: to one of its pins, or teeing into one of its planned wires.
    fn best_link(&self, net: usize, p: Point, f: Dir) -> Option<(f64, Vec<Point>)> {
        let mut best: Option<(f64, Vec<Point>)> = None;
        let mut take = |cand: (f64, Vec<Point>)| {
            if best.as_ref().is_none_or(|b| cand.0 < b.0) {
                best = Some(cand);
            }
        };
        for q in &self.net_pins[net] {
            take(estimate(p, f, q.p, &[q.f.opposite()]));
        }
        for &s in &self.net_segs[net] {
            let horizontal = s.0.y == s.1.y;
            let step = f.step(p, 2 * GRID);
            for t in [project(p, s), project(step, s), s.0, s.1] {
                // A planned wire ending on a pin is entered along the pin's
                // lead, which the pin itself already stands for.
                if self.net_pins[net].iter().any(|q| q.p == t) {
                    continue;
                }
                let interior = t != s.0 && t != s.1;
                // Into the middle of a wire only at a right angle.
                let arrive: Vec<Dir> = Dir::ALL
                    .into_iter()
                    .filter(|d| !interior || d.horizontal() != horizontal)
                    .collect();
                let (cost, path) = estimate(p, f, t, &arrive);
                // A tee is a junction, slightly worse than meeting a pin.
                take((cost + 0.5, path));
            }
        }
        best
    }

    /// Crossings with other nets' planned wires and passes through placed
    /// bodies, for a sketched path.
    fn sketch_penalty(
        &self,
        net: usize,
        path: &[Point],
        own_body: Option<Rect>,
        own_texts: &[Rect],
    ) -> f64 {
        let mut pen = 0.0;
        for w in path.windows(2) {
            let seg = (w[0], w[1]);
            for (n, segs) in self.net_segs.iter().enumerate() {
                if n == net {
                    continue;
                }
                for &s in segs {
                    if crosses(seg, s) {
                        pen += W_CROSSING;
                    }
                }
            }
            for b in self.bodies.iter().flatten().chain(own_body.iter()) {
                if segment_hits(w[0], w[1], &b.inflate(4)) {
                    pen += W_THROUGH_BODY;
                }
            }
            for t in self.texts.iter().flatten().chain(own_texts.iter()) {
                if segment_hits(w[0], w[1], &t.inflate(-2)) {
                    pen += W_THROUGH_TEXT;
                }
            }
        }
        pen
    }

    /// Planned wires that would run through a candidate's own texts.
    fn text_penalty(&self, texts: &[Rect], at: Point) -> f64 {
        let mut pen = 0.0;
        for t in texts {
            let t = translate(*t, at).inflate(-2);
            for segs in &self.net_segs {
                for &(a, b) in segs {
                    if segment_hits(a, b, &t) {
                        pen += W_THROUGH_TEXT;
                    }
                }
            }
        }
        pen
    }

    fn commit(&mut self, d: usize, shape: usize, at: Point) {
        let s = &self.shapes[d][shape];
        self.zones[d] = Some(translate(s.zone, at));
        self.bodies[d] = Some(translate(s.body, at));
        self.texts[d] = s.texts.iter().map(|t| translate(*t, at)).collect();
        self.pos[d] = Some(Placement {
            at,
            orient: s.orient,
            flip_text: s.flip_text,
        });
        self.chosen[d] = shape;
        if !self.order.contains(&d) {
            self.order.push(d);
        }
        self.add_pins(d);
    }

    /// Sketch a placed part's connections, then add its pins. Sketching
    /// first makes them aim at what was there already.
    fn add_pins(&mut self, d: usize) {
        let Some(pl) = self.pos[d] else { return };
        let s = self.shapes[d][self.chosen[d]].clone();
        for (k, &(rel, f)) in s.pins.iter().enumerate() {
            let net = self.c.devices[d].pins[k].net;
            if !self.c.is_signal(net) {
                continue;
            }
            if let Some((_, path)) = self.best_link(net, rel + pl.at, f) {
                for w in path.windows(2) {
                    if w[0] != w[1] {
                        self.net_segs[net].push((w[0], w[1]));
                    }
                }
            }
        }
        for (k, &(p, f)) in s.pins.iter().enumerate() {
            let net = self.c.devices[d].pins[k].net;
            self.net_pins[net].push(PinAt { p: p + pl.at, f });
        }
    }

    /// Take a part off the sheet and redo every sketch without it.
    fn remove(&mut self, d: usize) {
        self.zones[d] = None;
        self.bodies[d] = None;
        self.texts[d].clear();
        self.pos[d] = None;
        self.resketch();
    }

    /// Rebuild pins and planned wires by replaying the placed parts in the
    /// order they went down.
    fn resketch(&mut self) {
        for v in self.net_pins.iter_mut() {
            v.clear();
        }
        for v in self.net_segs.iter_mut() {
            v.clear();
        }
        for d in self.order.clone() {
            if self.pos[d].is_some() {
                self.add_pins(d);
            }
        }
    }

    /// Move each part, one at a time, to its best spot with every other part
    /// fixed. Undoes some of the short-sightedness of placing in order: a
    /// mirror transistor placed before its partner can turn to face it.
    fn refine(&mut self, passes: usize) {
        for _ in 0..passes {
            let mut moved = false;
            for d in self.order.clone() {
                let has_signal = self.c.devices[d].nets().any(|n| self.c.is_signal(n));
                let Some(old) = self.pos[d] else { continue };
                if !has_signal || self.fixed[d] {
                    continue;
                }
                let old_shape = self.chosen[d];
                self.remove(d);
                let old_cost = self.cost(d, old_shape, old.at, true);
                match (self.best_spot(d), old_cost) {
                    (Some((c, shape, at)), Some(oc)) if c < oc - 0.5 => {
                        self.commit(d, shape, at);
                        moved = true;
                    }
                    _ => self.commit(d, old_shape, old.at),
                }
                self.resketch();
            }
            if !moved {
                break;
            }
        }
    }

    /// Score a candidate; `None` if it collides. `full` adds the checks for
    /// predicted crossings and wires through bodies.
    fn cost(&self, d: usize, shape: usize, at: Point, full: bool) -> Option<f64> {
        let s = &self.shapes[d][shape];
        if !self.fits(translate(s.zone, at)) {
            return None;
        }
        let dev = &self.c.devices[d];
        let own_body = translate(s.body, at);
        let own_texts: Vec<Rect> = if full {
            s.texts.iter().map(|t| translate(*t, at)).collect()
        } else {
            Vec::new()
        };
        let mut total = self.conventions(d, shape, at);
        for (k, &(rel, f)) in s.pins.iter().enumerate() {
            let p = rel + at;
            let net = dev.pins[k].net;
            if !self.c.is_signal(net) {
                continue;
            }
            if let Some((cost, path)) = self.best_link(net, p, f) {
                total += cost;
                if full {
                    total += self.sketch_penalty(net, &path, Some(own_body), &own_texts);
                }
            }
        }
        if full {
            total += self.text_penalty(&s.texts, at);
        }
        Some(total + self.symmetry_bonus(d, shape, at))
    }

    /// A cheap lower-quality score for ranking candidates: conventions plus
    /// the straight-line distance from each signal pin to its net.
    fn quick_cost(&self, d: usize, shape: usize, at: Point) -> Option<f64> {
        let s = &self.shapes[d][shape];
        if !self.fits(translate(s.zone, at)) {
            return None;
        }
        let dev = &self.c.devices[d];
        let mut total = self.conventions(d, shape, at);
        for (k, &(rel, _)) in s.pins.iter().enumerate() {
            let net = dev.pins[k].net;
            if !self.c.is_signal(net) {
                continue;
            }
            let p = rel + at;
            let near = self.net_pins[net]
                .iter()
                .map(|q| q.p)
                .chain(self.net_segs[net].iter().map(|&s| project(p, s)))
                .map(|q| ((q.x - p.x).abs() + (q.y - p.y).abs()) / GRID)
                .min();
            if let Some(dist) = near {
                total += dist as f64;
            }
        }
        Some(total)
    }

    /// The drawing-convention part of a candidate's score: orientation
    /// preference, which way ground and rail pins face, signal direction,
    /// and how two-terminal parts lie.
    fn conventions(&self, d: usize, shape: usize, at: Point) -> f64 {
        let s = &self.shapes[d][shape];
        let dev = &self.c.devices[d];
        let mut total = s.pref;
        let mut signal_pins = Vec::new();
        let supply_source = dev.kind == Kind::Source && dev.nets().all(|n| !self.c.is_signal(n));
        for (k, &(rel, f)) in s.pins.iter().enumerate() {
            let p = rel + at;
            let pin = &dev.pins[k];
            let net = &self.c.nets[pin.net];
            match net.kind {
                NetKind::Ground => {
                    total += match f {
                        Dir::Down => 0.0,
                        Dir::Left | Dir::Right => W_SIDEWAYS,
                        Dir::Up => 2.0 * W_SIDEWAYS,
                    }
                }
                // A supply source drawn on its own only needs its ground
                // pin at the bottom; its rail label goes wherever the other
                // pin points.
                NetKind::Rail { .. } if supply_source => {}
                NetKind::Rail { positive } => {
                    let toward = if positive { Dir::Up } else { Dir::Down };
                    total += if f == toward {
                        0.0
                    } else if f == toward.opposite() {
                        1.6 * W_SIDEWAYS
                    } else {
                        0.8 * W_SIDEWAYS
                    };
                }
                NetKind::Signal => signal_pins.push((p, f, pin.net, pin.role)),
            }
        }
        // Signal flows left to right: through a part, the deeper net's pin
        // should not be left of the shallower one's.
        for &(p1, _, n1, r1) in &signal_pins {
            for &(p2, f2, n2, r2) in &signal_pins {
                let (Some(d1), Some(d2)) = (self.c.nets[n1].depth, self.c.nets[n2].depth) else {
                    continue;
                };
                if d1 >= d2 || n1 == n2 {
                    continue;
                }
                let active_pair =
                    dev.kind.active() && r1 == PinRole::Input && matches!(r2, PinRole::Output);
                if dev.kind.two_terminal() || active_pair {
                    if p2.x < p1.x {
                        total += W_BACKWARD;
                    }
                    if Some(n2) == self.c.output && f2 == Dir::Left {
                        total += 3.0;
                    }
                }
            }
        }
        // Two-terminal parts: along the path between two signal nets, upright
        // between a node and ground or a rail.
        if dev.kind.two_terminal() && s.pins.len() == 2 {
            let horizontal = s.pins[0].1.horizontal();
            let kinds = (
                self.c.nets[dev.pins[0].net].kind,
                self.c.nets[dev.pins[1].net].kind,
            );
            let both_signal = kinds.0 == NetKind::Signal && kinds.1 == NetKind::Signal;
            if both_signal && !horizontal && !self.c.feedback[d] && dev.kind != Kind::Source {
                // Diodes stand upright in bridges and clamps, current flowing
                // up toward the higher node; a light preference only. A
                // diode fed straight from the input source is in the signal
                // path, and lies along it like any series part.
                let from_input = self.c.input.is_some_and(|i| {
                    self.c.devices[i]
                        .nets()
                        .any(|n| self.c.is_signal(n) && dev.nets().any(|m| m == n))
                });
                total += if dev.kind == Kind::Diode && !from_input {
                    W_UPRIGHT_SERIES * 0.3
                } else {
                    W_UPRIGHT_SERIES
                };
            }
            if self.c.feedback[d] {
                // Above (or below) the parts it wraps.
                let ys: Vec<i32> = dev
                    .pins
                    .iter()
                    .flat_map(|p| self.net_pins[p.net].iter().map(|q| q.p.y))
                    .collect();
                if let (Some(&top), Some(&bottom)) = (ys.iter().min(), ys.iter().max()) {
                    let y = s.pins[0].0.y + at.y;
                    if !self.st.feedback_below && y > top {
                        total += W_FEEDBACK_SIDE;
                    }
                    if self.st.feedback_below && y < bottom {
                        total += W_FEEDBACK_SIDE;
                    }
                }
                if !horizontal {
                    total += W_UPRIGHT_SERIES * 0.6;
                }
            }
        }
        total
    }

    /// Candidate origins for `d` in `shape`: positions that put one of its
    /// pins near a placed pin of the same net, or level with one of the
    /// net's planned wires.
    fn candidates(&self, d: usize, shape: usize, reach: i32) -> Vec<Point> {
        let s = &self.shapes[d][shape];
        let dev = &self.c.devices[d];
        let mut out = HashSet::new();
        for (k, &(rel, _)) in s.pins.iter().enumerate() {
            let net = dev.pins[k].net;
            if !self.c.is_signal(net) {
                continue;
            }
            let anchors: Vec<Point> = self.net_pins[net]
                .iter()
                .rev()
                .take(3)
                .map(|q| q.p)
                .chain(self.net_segs[net].iter().rev().take(4).map(|s| s.1))
                .collect();
            for q in anchors {
                // Close by, every grid point.
                for dx in -reach..=reach {
                    for dy in -reach..=reach {
                        out.insert(q.offset(dx * GRID, dy * GRID) - rel);
                    }
                }
                // Farther out, the positions that line the pin up with the
                // anchor (straight wires), and a coarse grid for corners.
                let far = reach * 3;
                for k in -far..=far {
                    out.insert(q.offset(k * GRID, 0) - rel);
                    out.insert(q.offset(0, k * GRID) - rel);
                }
                for dx in (-far..=far).step_by(3) {
                    for dy in (-far..=far).step_by(3) {
                        out.insert(q.offset(dx * GRID, dy * GRID) - rel);
                    }
                }
            }
        }
        // Mirror images of a matched transistor already placed: differential
        // pairs and current mirrors read best drawn symmetrically.
        if dev.kind.transistor() {
            for (i, other) in self.c.devices.iter().enumerate() {
                let Some(pl) = self.pos[i] else { continue };
                if i == d || other.kind != dev.kind || !matched(dev, other) {
                    continue;
                }
                if s.orient != mirror_of(pl.orient) {
                    continue;
                }
                for k in 3..=28 {
                    for sign in [-1, 1] {
                        out.insert(Point::new(pl.at.x + sign * k * GRID, pl.at.y));
                    }
                }
            }
        }
        out.into_iter().collect()
    }

    /// Place `d` at its best spot near what is already placed.
    fn place_near(&mut self, d: usize) -> bool {
        match self.best_spot(d) {
            Some((_, shape, at)) => {
                self.commit(d, shape, at);
                true
            }
            None => false,
        }
    }

    /// The best spot for `d` near what is already placed.
    fn best_spot(&self, d: usize) -> Option<(f64, usize, Point)> {
        let order = |a: &(f64, usize, Point), b: &(f64, usize, Point)| {
            a.0.total_cmp(&b.0)
                .then((a.1, a.2.x, a.2.y).cmp(&(b.1, b.2.x, b.2.y)))
        };
        for reach in [6, 12, 20] {
            // A quick pass ranks every candidate that fits by conventions
            // and straight-line distance; the estimates of wire shapes, and
            // then of crossings, only run on the best of them.
            let mut quick: Vec<(f64, usize, Point)> = Vec::new();
            for shape in 0..self.shapes[d].len() {
                for at in self.candidates(d, shape, reach) {
                    if let Some(cost) = self.quick_cost(d, shape, at) {
                        quick.push((cost, shape, at));
                    }
                }
            }
            if quick.is_empty() {
                continue;
            }
            quick.sort_by(order);
            quick.truncate(160);
            let mut scored: Vec<(f64, usize, Point)> = quick
                .into_iter()
                .filter_map(|(_, shape, at)| self.cost(d, shape, at, false).map(|c| (c, shape, at)))
                .collect();
            scored.sort_by(order);
            scored.truncate(40);
            let best = scored
                .iter()
                .filter_map(|&(_, shape, at)| self.cost(d, shape, at, true).map(|c| (c, shape, at)))
                .min_by(|a, b| {
                    a.0.total_cmp(&b.0)
                        .then((a.2.x, a.2.y).cmp(&(b.2.x, b.2.y)))
                });
            if best.is_some() {
                return best;
            }
        }
        None
    }

    /// A reward for drawing a matched transistor as the mirror image of its
    /// partner at the same height.
    fn symmetry_bonus(&self, d: usize, shape: usize, at: Point) -> f64 {
        let dev = &self.c.devices[d];
        if !dev.kind.transistor() {
            return 0.0;
        }
        let s = &self.shapes[d][shape];
        let own = translate(s.body, at);
        let own_cx = own.min.x + own.max.x;
        let mut bonus = 0.0;
        for (i, other) in self.c.devices.iter().enumerate() {
            let (Some(pl), Some(body)) = (self.pos[i], self.bodies[i]) else {
                continue;
            };
            if i == d || other.kind != dev.kind || !matched(dev, other) {
                continue;
            }
            // In a differential pair the two inputs face away from each
            // other; in a mirror the shared gate or base faces the partner.
            let toward = if body.min.x + body.max.x > own_cx {
                Dir::Right
            } else {
                Dir::Left
            };
            if let Some(k) = dev.pins.iter().position(|p| p.role == PinRole::Input) {
                let f = s.pins[k].1;
                let shared = dev.pins[k].net == other.pins[k].net;
                if f.horizontal() && shared != (f == toward) {
                    bonus += 8.0;
                }
            }
            if pl.at.y == at.y && s.orient == mirror_of(pl.orient) && s.flip_text == pl.flip_text {
                bonus -= 6.0;
            }
        }
        bonus
    }

    /// Bounds of everything placed so far.
    fn bounds(&self) -> Option<Rect> {
        self.zones
            .iter()
            .flatten()
            .copied()
            .reduce(|a, b| a.union(b))
    }

    /// Place a part with nothing placed on its signal nets: to the right of
    /// the drawing, top-aligned, in the orientation it likes best.
    fn place_apart(&mut self, d: usize) {
        let b = self.bounds();
        let mut best: Option<(f64, usize, Point)> = None;
        for shape in 0..self.shapes[d].len() {
            let s = &self.shapes[d][shape];
            let base = match b {
                None => Point::new(0, 0) - Point::new(0, s.zone.min.y),
                Some(b) => Point::new(b.max.x + 96 - s.zone.min.x, b.min.y - s.zone.min.y),
            };
            for row in 0..24 {
                for col in 0..6 {
                    let at =
                        Point::new(snap(base.x + col * 4 * GRID), snap(base.y + row * 3 * GRID));
                    if let Some(cost) = self.cost(d, shape, at, true) {
                        let cost = cost + row as f64 * 0.5 + col as f64 * 2.0;
                        if best.is_none_or(|(c, _, _)| cost < c) {
                            best = Some((cost, shape, at));
                        }
                    }
                }
            }
        }
        let (_, shape, at) = best.unwrap_or_else(|| {
            let b = self
                .bounds()
                .unwrap_or(Rect::from_points(Point::new(0, 0), Point::new(0, 0)));
            (0.0, 0, Point::new(snap(b.max.x + 160), snap(b.max.y + 160)))
        });
        self.commit(d, shape, at);
    }

    /// Which part to place next.
    fn next(&self) -> Option<usize> {
        let c = self.c;
        let unplaced: Vec<usize> = (0..c.devices.len())
            .filter(|&d| self.pos[d].is_none())
            .collect();
        if unplaced.is_empty() {
            return None;
        }
        let placed_net = |n: usize| !self.net_pins[n].is_empty();
        let all_placed = |d: usize| {
            c.devices[d]
                .nets()
                .filter(|&n| c.is_signal(n))
                .all(placed_net)
        };
        let class = |d: usize| -> u32 {
            let dev = &c.devices[d];
            let signal = dev.nets().filter(|&n| c.is_signal(n)).count();
            let base = if c.feedback[d] {
                return if self.st.feedback_first { 0 } else { 4 };
            } else if dev.kind.two_terminal() && signal == 1 {
                u32::from(self.st.actives_first)
            } else if !dev.kind.two_terminal() {
                u32::from(!self.st.actives_first)
            } else {
                2
            };
            base + 1
        };
        let frontier: Vec<usize> = unplaced
            .iter()
            .copied()
            .filter(|&d| {
                let touches = c.devices[d].nets().any(|n| c.is_signal(n) && placed_net(n));
                touches && (!c.feedback[d] || all_placed(d))
            })
            .collect();
        if !frontier.is_empty() {
            return frontier.into_iter().min_by_key(|&d| {
                let depth = c.devices[d]
                    .nets()
                    .filter(|&n| c.is_signal(n) && placed_net(n))
                    .filter_map(|n| c.nets[n].depth)
                    .min()
                    .unwrap_or(u32::MAX);
                (class(d), depth, d)
            });
        }
        // Feedback parts whose nets will never all be placed: place anyway.
        if let Some(&d) = unplaced
            .iter()
            .find(|&&d| c.devices[d].nets().any(|n| c.is_signal(n) && placed_net(n)))
        {
            return Some(d);
        }
        // A new start: the seed or the input first, then whatever is closest
        // to it, with supply sources last.
        if let Some(seed) = self.st.seed
            && self.pos[seed].is_none()
        {
            return Some(seed);
        }
        unplaced.iter().copied().min_by_key(|&d| {
            let dev = &c.devices[d];
            let is_input = Some(d) == c.input;
            let signal = dev.nets().any(|n| c.is_signal(n));
            let depth = dev
                .nets()
                .filter_map(|n| c.nets[n].depth)
                .min()
                .unwrap_or(u32::MAX);
            (!is_input, !signal, depth, d)
        })
    }
}

/// The left-right mirror of a transistor orientation.
fn mirror_of(o: Orient) -> Orient {
    match o {
        Orient::R0 => Orient::M0,
        Orient::M0 => Orient::R0,
        Orient::M180 => Orient::R180,
        Orient::R180 => Orient::M180,
        o => o,
    }
}

/// Two transistors that belong together: a differential pair (shared
/// emitter or source) or a mirror (shared base or gate).
fn matched(a: &super::circuit::Device, b: &super::circuit::Device) -> bool {
    a.pins
        .iter()
        .zip(&b.pins)
        .any(|(x, y)| x.net == y.net && matches!(x.role, PinRole::Common | PinRole::Input))
}

/// A device standing in a current path between the rails, and the nets at
/// its top and bottom.
#[derive(Debug, Clone, Copy)]
struct Stack {
    dev: usize,
    upper: usize,
    lower: usize,
    row: usize,
}

impl Placer<'_> {
    /// The ends of a device's conduction path, upper first where the device
    /// decides it: collector or drain over emitter or source for N types,
    /// the other way round for P types. Two-terminal parts that carry
    /// current (resistors, inductors, diodes, current sources) go either way.
    fn channel(&self, d: usize) -> Option<(usize, usize, bool)> {
        let dev = &self.c.devices[d];
        let net = |role: PinRole| dev.pins.iter().find(|p| p.role == role).map(|p| p.net);
        match dev.kind {
            Kind::Bjt { p } | Kind::Fet { p } => {
                let (out, common) = (net(PinRole::Output)?, net(PinRole::Common)?);
                Some(if p {
                    (common, out, true)
                } else {
                    (out, common, true)
                })
            }
            Kind::Passive if !is_capacitor(&dev.symbol) => {
                Some((dev.pins[0].net, dev.pins[1].net, false))
            }
            Kind::Diode => Some((dev.pins[0].net, dev.pins[1].net, false)),
            Kind::Source if dev.symbol.eq_ignore_ascii_case("current") => {
                Some((dev.pins[0].net, dev.pins[1].net, false))
            }
            _ => None,
        }
    }

    /// The devices in current paths from a positive rail down to ground or
    /// a negative rail, with the row each stands in counted from the top.
    fn stacks(&self) -> Vec<Stack> {
        let c = self.c;
        let n = c.nets.len();
        let top = |x: usize| matches!(c.nets[x].kind, NetKind::Rail { positive: true });
        let bottom = |x: usize| {
            matches!(
                c.nets[x].kind,
                NetKind::Ground | NetKind::Rail { positive: false }
            )
        };
        let edges: Vec<(usize, usize, usize, bool)> = (0..c.devices.len())
            .filter_map(|d| self.channel(d).map(|(a, b, fixed)| (d, a, b, fixed)))
            .filter(|&(_, a, b, _)| a != b && !(top(a) && bottom(b)) && !(top(b) && bottom(a)))
            .collect();
        // Distance down from the positive rails and up from the bottom ones.
        let walk = |from_top: bool| {
            let mut dist = vec![usize::MAX; n];
            for (x, d) in dist.iter_mut().enumerate() {
                if (from_top && top(x)) || (!from_top && bottom(x)) {
                    *d = 0;
                }
            }
            for _ in 0..n {
                let mut changed = false;
                for &(_, a, b, fixed) in &edges {
                    let pairs: &[(usize, usize)] = if fixed {
                        if from_top { &[(a, b)] } else { &[(b, a)] }
                    } else {
                        &[(a, b), (b, a)]
                    };
                    for &(from, to) in pairs {
                        if dist[from] != usize::MAX && c.is_signal(to) && dist[from] + 1 < dist[to]
                        {
                            dist[to] = dist[from] + 1;
                            changed = true;
                        }
                    }
                }
                if !changed {
                    break;
                }
            }
            dist
        };
        let (dt, db) = (walk(true), walk(false));
        let mut out = Vec::new();
        for &(d, a, b, fixed) in &edges {
            let (upper, lower) = if fixed || dt[a] < dt[b] || (dt[a] == dt[b] && db[a] > db[b]) {
                (a, b)
            } else {
                (b, a)
            };
            if dt[upper] == usize::MAX || db[lower] == usize::MAX {
                continue;
            }
            out.push(Stack {
                dev: d,
                upper,
                lower,
                row: 0,
            });
        }
        let rows = out
            .iter()
            .map(|s| dt[s.upper] + db[s.lower] + 1)
            .max()
            .unwrap_or(1);
        for s in out.iter_mut() {
            s.row = if dt[s.upper] == 0 {
                0
            } else if db[s.lower] == 0 {
                rows - 1
            } else {
                dt[s.upper].min(rows - 1)
            };
        }
        out
    }

    /// Stand the current paths up as columns: devices stacked between the
    /// rails share a vertical axis, columns sharing nets sit side by side, a
    /// differential pair's columns are adjacent and mirrored, a mirror's
    /// shared gate faces its partner, and a tail source sits centred under
    /// its pair. Everything else is placed around the columns afterwards.
    fn place_columns(&mut self) {
        let stacks = self.stacks();
        let transistors = stacks
            .iter()
            .filter(|s| self.c.devices[s.dev].kind.transistor())
            .count();
        if transistors < 3 {
            return;
        }
        let k = stacks.len();
        // Join each device to the one directly below it when the net between
        // them is a plain link; a net with several devices above and one
        // below makes that one a tail, centred under them.
        let mut parent: Vec<usize> = (0..k).collect();
        fn find(p: &mut [usize], x: usize) -> usize {
            let mut x = x;
            while p[x] != x {
                p[x] = p[p[x]];
                x = p[x];
            }
            x
        }
        let mut tail: Vec<Option<Vec<usize>>> = vec![None; k];
        for net in 0..self.c.nets.len() {
            if !self.c.is_signal(net) {
                continue;
            }
            let above: Vec<usize> = (0..k).filter(|&i| stacks[i].lower == net).collect();
            let below: Vec<usize> = (0..k).filter(|&i| stacks[i].upper == net).collect();
            match (above.len(), below.len()) {
                (1, b) if b >= 1 => {
                    let primary = below
                        .iter()
                        .copied()
                        .find(|&i| self.c.devices[stacks[i].dev].kind.transistor())
                        .unwrap_or(below[0]);
                    let (x, y) = (find(&mut parent, above[0]), find(&mut parent, primary));
                    parent[x] = y;
                }
                (a, 1) if a >= 2 => tail[below[0]] = Some(above.clone()),
                _ => {}
            }
        }
        let mut columns: Vec<Vec<usize>> = Vec::new();
        let mut col_of = vec![usize::MAX; k];
        for i in 0..k {
            if tail[i].is_some() {
                continue;
            }
            let r = find(&mut parent, i);
            let at = match (0..columns.len()).find(|&c| find(&mut parent, columns[c][0]) == r) {
                Some(c) => c,
                None => {
                    columns.push(Vec::new());
                    columns.len() - 1
                }
            };
            columns[at].push(i);
            col_of[i] = at;
        }
        let m = columns.len();
        if !(2..=7).contains(&m) {
            return;
        }
        // Order the columns: those sharing nets close together, matched
        // pairs adjacent, the input side on the left. Few enough to try every
        // order.
        let nets_of = |col: &Vec<usize>| -> Vec<usize> {
            col.iter()
                .flat_map(|&i| self.c.devices[stacks[i].dev].nets())
                .filter(|&n| self.c.is_signal(n))
                .collect()
        };
        let col_nets: Vec<Vec<usize>> = columns.iter().map(nets_of).collect();
        let affinity = |a: usize, b: usize| -> usize {
            let mut shared: Vec<usize> = col_nets[a]
                .iter()
                .copied()
                .filter(|n| col_nets[b].contains(n))
                .collect();
            shared.sort();
            shared.dedup();
            shared.len()
        };
        let key: Vec<u32> = columns
            .iter()
            .map(|col| {
                col.iter()
                    .flat_map(|&i| self.c.devices[stacks[i].dev].pins.iter())
                    .filter(|p| p.role == PinRole::Input)
                    .filter_map(|p| self.c.nets[p.net].depth)
                    .min()
                    .unwrap_or(u32::MAX / 4)
            })
            .collect();
        let pair = |a: usize, b: usize| -> bool {
            columns[a].iter().any(|&i| {
                columns[b].iter().any(|&j| {
                    let (x, y) = (
                        &self.c.devices[stacks[i].dev],
                        &self.c.devices[stacks[j].dev],
                    );
                    x.kind == y.kind
                        && x.kind.transistor()
                        && stacks[i].row == stacks[j].row
                        && x.pins
                            .iter()
                            .zip(&y.pins)
                            .any(|(p, q)| p.net == q.net && p.role == PinRole::Common)
                })
            })
        };
        let mut best: Option<(f64, Vec<usize>)> = None;
        let mut perm: Vec<usize> = (0..m).collect();
        permutations(&mut perm, 0, &mut |order: &[usize]| {
            let pos = |c: usize| order.iter().position(|&o| o == c).expect("in order") as f64;
            let mut cost = 0.0;
            for a in 0..m {
                for b in a + 1..m {
                    let d = (pos(a) - pos(b)).abs();
                    cost += affinity(a, b) as f64 * d;
                    if pair(a, b) && d > 1.0 {
                        cost += 100.0;
                    }
                    let (ka, kb) = (key[a], key[b]);
                    if (ka < kb && pos(a) > pos(b)) || (kb < ka && pos(b) > pos(a)) {
                        cost += 0.01;
                    }
                }
            }
            if best.as_ref().is_none_or(|(c, _)| cost < *c) {
                best = Some((cost, order.to_vec()));
            }
        });
        let Some((_, order)) = best else { return };
        let rank_of = |c: usize| order.iter().position(|&o| o == c).expect("in order");

        // Which way each device's control pin faces.
        let col_x = |i: usize| -> f64 {
            match &tail[i] {
                Some(parents) => {
                    parents
                        .iter()
                        .map(|&p| rank_of(col_of[p]) as f64)
                        .sum::<f64>()
                        / parents.len() as f64
                }
                None => rank_of(col_of[i]) as f64,
            }
        };
        let mut facing_left = vec![true; k];
        for i in 0..k {
            let dev = &self.c.devices[stacks[i].dev];
            let Some(g) = dev.pins.iter().position(|p| p.role == PinRole::Input) else {
                continue;
            };
            let me = col_x(i);
            let mut decided = None;
            for j in 0..k {
                let other = &self.c.devices[stacks[j].dev];
                if i == j || other.kind != dev.kind || stacks[i].row != stacks[j].row {
                    continue;
                }
                let them = col_x(j);
                if (them - me).abs() < 0.1 {
                    continue;
                }
                let shared_gate = other.pins.get(g).is_some_and(|q| q.net == dev.pins[g].net);
                let shared_common = dev.pins.iter().zip(&other.pins).any(|(p, q)| {
                    p.net == q.net && p.role == PinRole::Common && self.c.is_signal(p.net)
                });
                if shared_gate {
                    decided = Some(them < me);
                } else if shared_common {
                    decided = Some(them > me);
                }
            }
            facing_left[i] = decided.unwrap_or_else(|| {
                let gate = dev.pins[g].net;
                let xs: Vec<f64> = (0..k)
                    .filter(|&j| j != i && self.c.devices[stacks[j].dev].nets().any(|n| n == gate))
                    .map(col_x)
                    .collect();
                xs.is_empty() || xs.iter().sum::<f64>() / (xs.len() as f64) <= me
            });
        }

        // Shapes, axes and extents.
        let mut shape_of = vec![0usize; k];
        let mut axis = vec![0i32; k];
        let mut span = vec![(0i32, 0i32); k];
        let mut top_pin = vec![0i32; k];
        let mut height = vec![0i32; k];
        for i in 0..k {
            let st = stacks[i];
            let dev = &self.c.devices[st.dev];
            let want = |o: Orient| -> bool {
                let s = self.shapes[st.dev].iter().find(|s| s.orient == o);
                let Some(s) = s else { return false };
                let up = dev.pins.iter().position(|p| p.net == st.upper);
                let low = dev.pins.iter().position(|p| p.net == st.lower);
                match (up, low) {
                    (Some(u), Some(l)) => {
                        s.pins[u].0.y < s.pins[l].0.y && s.pins[u].0.x == s.pins[l].0.x
                    }
                    _ => false,
                }
            };
            let orients: Vec<Orient> = if dev.kind.transistor() {
                let p = matches!(dev.kind, Kind::Bjt { p: true } | Kind::Fet { p: true });
                match (p, facing_left[i]) {
                    (false, true) => vec![Orient::R0],
                    (false, false) => vec![Orient::M0],
                    (true, true) => vec![Orient::M180],
                    (true, false) => vec![Orient::R180],
                }
            } else {
                vec![Orient::R0, Orient::M180]
            };
            let Some(o) = orients.into_iter().find(|&o| want(o)) else {
                return;
            };
            let Some(si) = self.shapes[st.dev]
                .iter()
                .position(|s| s.orient == o && !s.flip_text)
            else {
                return;
            };
            let s = &self.shapes[st.dev][si];
            let u = dev
                .pins
                .iter()
                .position(|p| p.net == st.upper)
                .expect("upper pin");
            let l = dev
                .pins
                .iter()
                .position(|p| p.net == st.lower)
                .expect("lower pin");
            shape_of[i] = si;
            axis[i] = s.pins[u].0.x;
            span[i] = (axis[i] - s.zone.min.x, s.zone.max.x - axis[i]);
            top_pin[i] = s.pins[u].0.y;
            height[i] = s.pins[l].0.y - s.pins[u].0.y;
        }
        // Column positions from left to right, rows from the top.
        // Room for text drawn wider than estimated: LTspice keeps a minimum
        // font size when zoomed out.
        let gap = 6 * GRID + self.st.spread * GRID;
        let mut col_axis = vec![0i32; m];
        let mut x = 0;
        for (r, &c) in order.iter().enumerate() {
            let left = columns[c].iter().map(|&i| span[i].0).max().unwrap_or(0);
            let right = columns[c].iter().map(|&i| span[i].1).max().unwrap_or(0);
            if r > 0 {
                x += left;
            }
            col_axis[c] = snap(x);
            x = col_axis[c] + right + gap;
        }
        let rows = stacks.iter().map(|s| s.row).max().unwrap_or(0) + 1;
        let mut row_y = vec![0i32; rows];
        for r in 1..rows {
            let tallest = (0..k)
                .filter(|&i| stacks[i].row == r - 1)
                .map(|i| height[i])
                .max()
                .unwrap_or(96);
            row_y[r] = row_y[r - 1] + tallest + 4 * GRID;
        }
        // Commit columns first, then the tails between them.
        let mut todo: Vec<usize> = (0..k).filter(|&i| tail[i].is_none()).collect();
        todo.extend((0..k).filter(|&i| tail[i].is_some()));
        for i in todo {
            let st = stacks[i];
            let ax = match &tail[i] {
                Some(parents) => snap(
                    parents.iter().map(|&p| col_axis[col_of[p]]).sum::<i32>()
                        / parents.len() as i32,
                ),
                None => col_axis[col_of[i]],
            };
            let base = Point::new(ax - axis[i], row_y[st.row] - top_pin[i]);
            let spot = (0..12)
                .flat_map(|k| [(0, k), (k, 0), (-k, 0)])
                .map(|(dx, dy)| base.offset(dx * GRID, dy * GRID))
                .find(|&at| self.fits(translate(self.shapes[st.dev][shape_of[i]].zone, at)));
            if let Some(at) = spot {
                self.commit(st.dev, shape_of[i], at);
                self.fixed[st.dev] = true;
            }
        }
    }
}

/// Call `f` with every ordering of `items[from..]`.
fn permutations(items: &mut [usize], from: usize, f: &mut dyn FnMut(&[usize])) {
    if from == items.len() {
        f(items);
        return;
    }
    for i in from..items.len() {
        items.swap(from, i);
        permutations(items, from + 1, f);
        items.swap(from, i);
    }
}

/// Place every device. Returns one placement per device and the total
/// drawing-convention cost of the result, for comparing strategies.
pub(crate) fn place(c: &Circuit, st: &Strategy) -> (Vec<Placement>, f64) {
    let mut p = Placer::new(c, st);
    if st.columns {
        p.place_columns();
    }
    while let Some(d) = p.next() {
        let anchored = c.devices[d]
            .nets()
            .any(|n| c.is_signal(n) && !p.net_pins[n].is_empty());
        if !(anchored && p.place_near(d)) {
            p.place_apart(d);
        }
    }
    p.refine(2);
    let conventions = (0..c.devices.len())
        .map(|d| p.conventions(d, p.chosen[d], p.pos[d].expect("placed").at))
        .sum();
    (
        p.pos.into_iter().map(|x| x.expect("all placed")).collect(),
        conventions,
    )
}

/// Capacitors carry no DC current, so they do not stand in current paths.
fn is_capacitor(symbol: &str) -> bool {
    let base = symbol.rsplit(['\\', '/']).next().unwrap_or(symbol);
    base.eq_ignore_ascii_case("cap") || base.to_ascii_lowercase().starts_with("cap_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn straight_facing_pins_need_no_corners() {
        let e = estimate(
            Point::new(0, 0),
            Dir::Right,
            Point::new(64, 0),
            &[Dir::Right],
        );
        assert_eq!(e.0, 4.0);
        assert_eq!(e.1, vec![Point::new(0, 0), Point::new(64, 0)]);
        // Offset vertically: a Z, two corners.
        let z = estimate(
            Point::new(0, 0),
            Dir::Right,
            Point::new(64, 64),
            &[Dir::Right],
        );
        assert_eq!(z.0, 8.0 + 5.0);
        assert_eq!(z.1.len(), 4);
        // Nearly lined up: the small jog costs extra, so pins line up.
        let jog = estimate(
            Point::new(0, 0),
            Dir::Right,
            Point::new(64, 32),
            &[Dir::Right],
        );
        assert!(jog.0 > 6.0 + 5.0, "{jog:?}");
        // Facing away from each other: a detour.
        let u = estimate(Point::new(0, 0), Dir::Left, Point::new(64, 0), &[Dir::Left]);
        assert!(u.0 > 14.0);
    }

    #[test]
    fn crossings_need_interiors() {
        let h = (Point::new(0, 0), Point::new(64, 0));
        assert!(crosses(h, (Point::new(32, -16), Point::new(32, 16))));
        assert!(!crosses(h, (Point::new(64, -16), Point::new(64, 16))));
    }
}
