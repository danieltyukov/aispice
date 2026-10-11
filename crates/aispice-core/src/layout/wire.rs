//! Connecting placed parts: ground symbols under grounded pins, labels on
//! supply rails, wires for local signal connections and labels for the long
//! ones.
//!
//! Every wire comes from the maze router, which follows LTspice's connection
//! rules, so no step here can join two nets by accident; the round trip in
//! [`super::verify`] checks the whole drawing again at the end.

use super::circuit::{Circuit, NetKind, auto_named};
use super::geom::{Dir, core_body, orient_dir, overlaps, place_rect, segment_hits};
use super::maze::{CLEARANCE, Grid, Start};
use super::place::Placement;
use super::symbols::symbol;
use super::text::{flag_box, symbol_texts};
use crate::geometry::{GRID, Point, Rect};
use crate::schematic::{Flag, Symbol, Wire};

/// Pins farther apart than this (in sheet units) are joined by labels rather
/// than a long wire.
const LABEL_DIST: i32 = 40 * GRID;

#[derive(Debug, Clone, Default)]
pub(crate) struct Wiring {
    pub wires: Vec<Wire>,
    pub flags: Vec<Flag>,
    /// Automatically named nets that needed labels, with the name given.
    pub renames: Vec<(usize, String)>,
}

#[derive(Debug, Clone, Copy)]
struct PinPos {
    p: Point,
    f: Dir,
    net: usize,
}

struct Ctx {
    grid: Grid,
    /// Areas labels and ground symbols must not cover: bodies, part text,
    /// flags already placed.
    blocked: Vec<Rect>,
    wires: Vec<Wire>,
    flags: Vec<Flag>,
    /// Pins with the centre of their part, for working out label sides.
    pin_owners: Vec<(Point, Point)>,
    /// Every pin and the way its wire leaves.
    pin_dirs: Vec<(Point, Dir)>,
}

impl Ctx {
    fn add_path(&mut self, path: &[Point], net: usize) {
        for w in path.windows(2) {
            if w[0] != w[1] {
                self.grid.add_wire(w[0], w[1], net as u32);
                self.wires.push(Wire::new(w[0], w[1]));
            }
        }
    }

    /// Whether a flag stub can be drawn: clear of everything, its own net's
    /// wires included. A one-point path is a flag right on an existing point.
    fn path_free(&self, path: &[Point], net: usize) -> bool {
        path.windows(2)
            .all(|w| self.grid.stub_free(w[0], w[1], net as u32))
    }

    /// The box a flag would cover with `stub` as its wire.
    fn flag_rect(&self, f: &Flag, stub: &[Point]) -> Rect {
        let mut wires: Vec<Wire> = stub
            .windows(2)
            .filter(|w| w[0] != w[1])
            .map(|w| Wire::new(w[0], w[1]))
            .collect();
        wires.extend(self.wires.iter().copied());
        flag_box(f, &wires, &self.pin_owners)
    }

    /// How badly a flag's box collides with what is drawn.
    fn clash(&self, r: &Rect, own_path: &[Point]) -> f64 {
        let r = r.inflate(-1);
        let mut cost = 0.0;
        for b in &self.blocked {
            if overlaps(&r, b) {
                cost += 10.0;
            }
        }
        for w in &self.wires {
            let own = own_path
                .iter()
                .any(|p| super::geom::on_segment(*p, w.a, w.b));
            if !own && segment_hits(w.a, w.b, &r) {
                cost += 6.0;
            }
        }
        // A flag crowding another pin, or sitting where that pin's wire has
        // to leave, blocks it.
        let near = r.inflate(14);
        for &(pin, f) in &self.pin_dirs {
            if own_path.first() == Some(&pin) {
                continue;
            }
            let out = f.step(pin, 2 * GRID);
            if near.contains(pin) || segment_hits(pin, out, &r.inflate(8)) {
                cost += 20.0;
            }
        }
        cost
    }

    fn commit_flag(&mut self, f: Flag, path: &[Point], net: usize) {
        let rect = self.flag_rect(&f, path);
        self.add_path(path, net);
        self.grid.add_flag(f.at, net as u32);
        if f.is_ground() {
            // Nothing may run through the ground symbol.
            self.grid.add_keepout(rect.inflate(6));
        } else {
            self.grid.add_cost(rect.inflate(2), 12);
        }
        self.blocked.push(rect);
        self.flags.push(f);
    }

    /// Put a flag for `net` on pin `pin`, trying stubs of a few shapes.
    /// Returns false if nothing fits (the caller then uses the pin itself).
    fn flag_on_pin(&mut self, pin: PinPos, label: &str, prefer: Dir) -> bool {
        let mut options: Vec<Vec<Point>> = Vec::new();
        let ground = label == "0";
        let p = pin.p;
        if ground {
            match pin.f {
                Dir::Down => {
                    for k in [0, 1, 2, 3] {
                        options.push(vec![p, Dir::Down.step(p, k * GRID)]);
                    }
                }
                Dir::Left | Dir::Right => {
                    for out in [1, 2, 3] {
                        for down in [2, 3, 1, 4] {
                            let a = pin.f.step(p, out * GRID);
                            options.push(vec![p, a, Dir::Down.step(a, down * GRID)]);
                        }
                    }
                }
                Dir::Up => {
                    for up in [1, 2] {
                        for side in [Dir::Left, Dir::Right] {
                            for k in [3, 4, 5] {
                                let a = Dir::Up.step(p, up * GRID);
                                options.push(vec![p, a, side.step(a, k * GRID)]);
                            }
                        }
                    }
                }
            }
        } else {
            for k in [0, 1, 2, 3] {
                if k == 0 && pin.f != prefer {
                    continue;
                }
                options.push(vec![p, pin.f.step(p, k * GRID)]);
            }
            for k in [1, 2] {
                let a = pin.f.step(p, k * GRID);
                for side in [prefer, Dir::Left, Dir::Right, Dir::Up, Dir::Down] {
                    if side == pin.f || side == pin.f.opposite() {
                        continue;
                    }
                    options.push(vec![p, a, side.step(a, 2 * GRID)]);
                }
            }
        }
        let mut best: Option<(f64, Vec<Point>)> = None;
        for (rank, path) in options.into_iter().enumerate() {
            let mut path = path;
            path.dedup();
            let end = *path.last().expect("non-empty");
            if !self.path_free(&path, pin.net) || !self.grid.free_for(end, pin.net as u32) {
                continue;
            }
            let f = Flag {
                at: end,
                label: label.to_string(),
            };
            let rect = self.flag_rect(&f, &path);
            let mut cost = self.clash(&rect, &path) + rank as f64 * 0.3;
            if !ground {
                // Labels read best as horizontal text beside a stub.
                let last = path.len() >= 2;
                let horizontal_end =
                    last && path[path.len() - 2].y == end.y && path[path.len() - 2] != end;
                if !horizontal_end && pin.f.horizontal() {
                    cost += 1.0;
                }
            }
            if best.as_ref().is_none_or(|(b, _)| cost < *b) {
                best = Some((cost, path));
            }
        }
        match best {
            Some((_, path)) => {
                let at = *path.last().expect("non-empty");
                self.commit_flag(
                    Flag {
                        at,
                        label: label.to_string(),
                    },
                    &path,
                    pin.net,
                );
                true
            }
            None => false,
        }
    }

    /// Route the pins of one cluster of a net into a tree. Returns the pins
    /// that could not be joined.
    fn route_cluster(&mut self, net: usize, pins: &[PinPos]) -> (Vec<PinPos>, Vec<Wire>) {
        let mut made = Vec::new();
        if pins.len() < 2 {
            return (Vec::new(), made);
        }
        // Start from the pin with the least total distance to the others.
        let root = (0..pins.len())
            .min_by_key(|&i| {
                pins.iter()
                    .map(|q| (q.p.x - pins[i].p.x).abs() + (q.p.y - pins[i].p.y).abs())
                    .sum::<i32>()
            })
            .expect("non-empty");
        let mut tree: Vec<Point> = vec![pins[root].p];
        let mut todo: Vec<usize> = (0..pins.len()).filter(|&i| i != root).collect();
        let mut failed = Vec::new();
        while !todo.is_empty() {
            let (k, _) = todo
                .iter()
                .enumerate()
                .map(|(k, &i)| {
                    let d = tree
                        .iter()
                        .map(|t| (t.x - pins[i].p.x).abs() + (t.y - pins[i].p.y).abs())
                        .min()
                        .unwrap_or(i32::MAX);
                    (k, d)
                })
                .min_by_key(|&(_, d)| d)
                .expect("non-empty");
            let i = todo.remove(k);
            let pin = pins[i];
            let manhattan = tree
                .iter()
                .map(|t| (t.x - pin.p.x).abs() + (t.y - pin.p.y).abs())
                .min()
                .unwrap_or(0)
                / GRID;
            let limit = (manhattan as u32 * 10) * 3 + 600;
            let start = Start {
                at: pin.p,
                dirs: vec![pin.f],
            };
            match self.grid.route(&[start], &tree, &[net as u32], limit) {
                Some(path) => {
                    let mut pts = Vec::new();
                    for w in path.windows(2) {
                        let d = Dir::from_vec(w[1].x - w[0].x, w[1].y - w[0].y).expect("axis");
                        let mut p = w[0];
                        pts.push(p);
                        while p != w[1] {
                            p = d.step(p, GRID);
                            pts.push(p);
                        }
                    }
                    self.add_path(&path, net);
                    made.extend(
                        path.windows(2)
                            .filter(|w| w[0] != w[1])
                            .map(|w| Wire::new(w[0], w[1])),
                    );
                    tree.extend(pts);
                }
                None => failed.push(pin),
            }
        }
        (failed, made)
    }

    /// The best place for a label naming a wired cluster: a stub off one of
    /// its pins, a point on one of its horizontal wires, or for the output a
    /// stub to the right of its rightmost point. Only the cluster's own wires
    /// count: a label on another cluster of the same net would leave this
    /// one unnamed.
    fn label_cluster(
        &mut self,
        net: usize,
        pins: &[PinPos],
        own: &[Wire],
        label: &str,
        rightmost: bool,
    ) {
        let tag = net as u32;
        let max_x = pins
            .iter()
            .map(|p| p.p.x)
            .chain(own.iter().flat_map(|w| [w.a.x, w.b.x]))
            .max()
            .unwrap_or(0);
        let mut best: Option<(f64, Vec<Point>)> = None;
        let consider =
            |ctx: &Ctx, path: Vec<Point>, extra: f64, best: &mut Option<(f64, Vec<Point>)>| {
                let end = *path.last().expect("non-empty");
                if !ctx.path_free(&path, net) || !ctx.grid.free_for(end, tag) {
                    return;
                }
                let f = Flag {
                    at: end,
                    label: label.to_string(),
                };
                let rect = ctx.flag_rect(&f, &path);
                let mut cost = ctx.clash(&rect, &path) + extra;
                if rightmost {
                    cost += (max_x - end.x).max(0) as f64 / 48.0;
                }
                if best.as_ref().is_none_or(|(b, _)| cost < *b) {
                    *best = Some((cost, path));
                }
            };
        for pin in pins {
            for k in [1, 2, 3] {
                let end = pin.f.step(pin.p, k * GRID);
                let vertical = !pin.f.horizontal();
                consider(
                    self,
                    vec![pin.p, end],
                    k as f64 * 0.4 + if vertical { 1.5 } else { 0.0 },
                    &mut best,
                );
            }
        }
        if rightmost {
            let ends: Vec<Point> = pins
                .iter()
                .map(|p| p.p)
                .chain(own.iter().flat_map(|w| [w.a, w.b]))
                .filter(|p| p.x == max_x)
                .collect();
            for e in ends {
                for k in [2, 3, 4] {
                    consider(
                        self,
                        vec![e, Dir::Right.step(e, k * GRID)],
                        k as f64 * 0.3 - 1.0,
                        &mut best,
                    );
                }
            }
        }
        for w in own {
            if w.a.y != w.b.y {
                continue;
            }
            let (x0, x1) = (w.a.x.min(w.b.x), w.a.x.max(w.b.x));
            let mut x = x0 + GRID;
            while x < x1 {
                consider(self, vec![Point::new(x, w.a.y)], 0.2, &mut best);
                x += GRID;
            }
        }
        if let Some((_, path)) = best {
            let at = *path.last().expect("non-empty");
            self.commit_flag(
                Flag {
                    at,
                    label: label.to_string(),
                },
                &path,
                net,
            );
        } else if let Some(pin) = pins.first() {
            // Last resort: the label right on the pin always connects only it.
            self.commit_flag(
                Flag {
                    at: pin.p,
                    label: label.to_string(),
                },
                &[pin.p],
                net,
            );
        }
    }
}

/// Group pins whose minimum spanning tree edges are shorter than `limit`.
fn clusters(pins: &[PinPos], limit: i32) -> Vec<Vec<PinPos>> {
    let n = pins.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], x: usize) -> usize {
        let mut x = x;
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    for (i, a) in pins.iter().enumerate() {
        for (j, b) in pins.iter().enumerate().skip(i + 1) {
            let d = (a.p.x - b.p.x).abs() + (a.p.y - b.p.y).abs();
            if d <= limit {
                let (ra, rb) = (find(&mut parent, i), find(&mut parent, j));
                parent[ra] = rb;
            }
        }
    }
    let mut groups: Vec<(usize, Vec<PinPos>)> = Vec::new();
    for (i, pin) in pins.iter().enumerate() {
        let r = find(&mut parent, i);
        match groups.iter_mut().find(|(k, _)| *k == r) {
            Some((_, g)) => g.push(*pin),
            None => groups.push((r, vec![*pin])),
        }
    }
    groups.into_iter().map(|(_, g)| g).collect()
}

/// A label name for an automatically named net, unused by any other net.
fn fresh_name(c: &Circuit, taken: &[String]) -> String {
    (1..)
        .map(|i| format!("n{i}"))
        .find(|n| {
            !c.nets.iter().any(|x| x.name.eq_ignore_ascii_case(n))
                && !taken.iter().any(|t| t.eq_ignore_ascii_case(n))
        })
        .expect("unbounded")
}

/// The fallback when no routing grid can be built: a flag on every pin,
/// ground, rail or net name, and a fresh name for automatically numbered
/// nets with more than one pin.
fn labels_only(c: &Circuit, pins: &[PinPos]) -> Wiring {
    let mut out = Wiring::default();
    let mut taken: Vec<String> = Vec::new();
    for (n, info) in c.nets.iter().enumerate() {
        let on: Vec<&PinPos> = pins.iter().filter(|p| p.net == n).collect();
        let label = match info.kind {
            NetKind::Ground => "0".to_string(),
            _ if info.named || !auto_named(&info.name) => info.name.clone(),
            _ if on.len() > 1 => {
                let name = fresh_name(c, &taken);
                taken.push(name.clone());
                out.renames.push((n, name.clone()));
                name
            }
            _ => continue,
        };
        for p in on {
            out.flags.push(Flag {
                at: p.p,
                label: label.clone(),
            });
        }
    }
    out
}

/// Wire up placed devices.
pub(crate) fn wire(c: &Circuit, placed: &[Placement]) -> Wiring {
    let mut pins: Vec<PinPos> = Vec::new();
    let mut syms: Vec<Symbol> = Vec::new();
    let mut bodies: Vec<Rect> = Vec::new();
    let mut texts: Vec<Rect> = Vec::new();
    let mut pin_owners = Vec::new();
    for (d, pl) in c.devices.iter().zip(placed) {
        let s = symbol(d, pl.at, pl.orient, pl.flip_text);
        let core = core_body(&d.def)
            .map(|b| place_rect(b, pl.at, pl.orient))
            .unwrap_or(Rect::from_points(pl.at, pl.at));
        let centre = Point::new((core.min.x + core.max.x) / 2, (core.min.y + core.max.y) / 2);
        for p in &d.pins {
            let at = pl.at + pl.orient.apply(p.at);
            pins.push(PinPos {
                p: at,
                f: orient_dir(p.facing, pl.orient),
                net: p.net,
            });
            pin_owners.push((at, centre));
        }
        texts.extend(symbol_texts(&s, &d.def));
        bodies.push(core);
        syms.push(s);
    }
    let mut area = bodies
        .iter()
        .chain(texts.iter())
        .copied()
        .reduce(|a, b| a.union(b))
        .unwrap_or(Rect::from_points(Point::new(0, 0), Point::new(0, 0)));
    for p in &pins {
        area.include(p.p);
    }
    let area = area.inflate(14 * GRID);
    let Some(mut grid) = Grid::new(area) else {
        // Too large to route (a value of thousands of characters makes its
        // text that wide): every pin gets a label, which still connects.
        return labels_only(c, &pins);
    };
    for b in &bodies {
        grid.add_keepout(b.inflate(CLEARANCE));
        grid.add_cost(b.inflate(CLEARANCE + 16), 3);
    }
    for t in &texts {
        grid.add_cost(t.inflate(2), 30);
    }
    for p in &pins {
        grid.add_pin(p.p, p.f, p.net as u32);
    }
    let mut blocked: Vec<Rect> = bodies.iter().map(|b| b.inflate(4)).collect();
    blocked.extend(texts.iter().copied());
    let mut ctx = Ctx {
        grid,
        blocked,
        wires: Vec::new(),
        flags: Vec::new(),
        pin_owners,
        pin_dirs: pins.iter().map(|p| (p.p, p.f)).collect(),
    };

    let mut renames: Vec<(usize, String)> = Vec::new();
    let mut taken: Vec<String> = Vec::new();
    // Ground symbols and rail labels first: they sit right at their pins.
    for pin in pins.clone() {
        let net = &c.nets[pin.net];
        match net.kind {
            NetKind::Ground => {
                if !ctx.flag_on_pin(pin, "0", Dir::Down) {
                    ctx.commit_flag(
                        Flag {
                            at: pin.p,
                            label: "0".into(),
                        },
                        &[pin.p],
                        pin.net,
                    );
                }
            }
            NetKind::Rail { positive } => {
                let toward = if positive { Dir::Up } else { Dir::Down };
                // A rail LTspice would have numbered gets a name of its own,
                // or its label could collide with LTspice's numbering of
                // the other nets.
                let name = if net.named || !auto_named(&net.name) {
                    net.name.clone()
                } else if let Some((_, given)) = renames.iter().find(|(n, _)| *n == pin.net) {
                    given.clone()
                } else {
                    let given = fresh_name(c, &taken);
                    taken.push(given.clone());
                    renames.push((pin.net, given.clone()));
                    given
                };
                if !ctx.flag_on_pin(pin, &name, toward) {
                    ctx.commit_flag(
                        Flag {
                            at: pin.p,
                            label: name,
                        },
                        &[pin.p],
                        pin.net,
                    );
                }
            }
            NetKind::Signal => {}
        }
    }

    // Signal nets, small and local ones first so they get the straight runs.
    let mut order: Vec<usize> = (0..c.nets.len()).filter(|&n| c.is_signal(n)).collect();
    let span = |n: usize| {
        let ps: Vec<Point> = pins.iter().filter(|p| p.net == n).map(|p| p.p).collect();
        let (x0, x1) = (ps.iter().map(|p| p.x).min(), ps.iter().map(|p| p.x).max());
        let (y0, y1) = (ps.iter().map(|p| p.y).min(), ps.iter().map(|p| p.y).max());
        match (x0, x1, y0, y1) {
            (Some(a), Some(b), Some(c0), Some(d)) => (b - a) + (d - c0),
            _ => 0,
        }
    };
    order.sort_by_key(|&n| (span(n), n));
    for n in order {
        let net_pins: Vec<PinPos> = pins.iter().copied().filter(|p| p.net == n).collect();
        if net_pins.is_empty() {
            continue;
        }
        let mut groups = Vec::new();
        for g in clusters(&net_pins, LABEL_DIST) {
            let (failed, made) = ctx.route_cluster(n, &g);
            let joined: Vec<PinPos> = g
                .iter()
                .copied()
                .filter(|p| !failed.iter().any(|f| f.p == p.p))
                .collect();
            if !joined.is_empty() {
                groups.push((joined, made));
            }
            for f in failed {
                groups.push((vec![f], Vec::new()));
            }
        }
        let info = &c.nets[n];
        let needs_label = info.named || groups.len() > 1;
        if !needs_label {
            continue;
        }
        let label = if info.named || !auto_named(&info.name) {
            info.name.clone()
        } else {
            // A new name for a net LTspice would have numbered itself; the
            // round trip is checked with the same renaming.
            let name = fresh_name(c, &taken);
            taken.push(name.clone());
            renames.push((n, name.clone()));
            name
        };
        let rightmost = Some(n) == c.output;
        for (g, made) in &groups {
            ctx.label_cluster(n, g, made, &label, rightmost);
        }
    }

    Wiring {
        wires: ctx.wires,
        flags: ctx.flags,
        renames,
    }
}
