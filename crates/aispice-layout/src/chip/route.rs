//! The chip's top-level router: the few nets between pads and blocks, on a
//! grid, in two layers.
//!
//! Nodes sit every 0.6 um on Metal3 (preferred horizontal) and Metal4
//! (preferred vertical). Wires are 0.3 um wide and every via gets a 0.3 um
//! pad on both layers, so two nets on neighbouring nodes are 0.3 um apart
//! (the rule is 0.21) and two vias 0.41 um (rule 0.22): any routing on the
//! grid where each node belongs to at most one net per layer is clean by
//! construction. Blocks and a halo around them are off limits.
//!
//! Each pin reaches the grid on a stub: straight out of its block or pad on
//! its own layer, a short jog along the grid line when it is off the grid,
//! and a Via2 up to Metal3 for pins on Metal2. Grid nodes near a stub are
//! reserved for its net. Each net then grows as a tree, terminal by
//! terminal, with A* from every node of the tree to the next terminal.
//! Nets go shortest first; a net that finds no path moves to the front and
//! everything is routed again, up to 8 times. The same problem always gives
//! the same routes.

use super::geom::{Facing, LayerRect, METAL1, METAL2, METAL3, METAL4, Rect, VIA1, VIA2, VIA3};
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

/// Grid pitch, wire width and the clearance kept from anything that is not
/// on the grid, nm.
pub const GRID: i64 = super::floorplan::GRID;
pub const WIRE: i64 = 300;
const HALF: i64 = WIRE / 2;
const CLEAR: i64 = 250;
const VIA: i64 = 190;
/// Routing keeps this far from a block's outline.
pub const HALO: i64 = 1_200;
/// At most this many grid nodes per layer.
pub const MAX_NODES: usize = 4_000_000;
const ATTEMPTS: usize = 8;
const STEP: u32 = 2;
const ACROSS: u32 = 6;
const VIA_COST: u32 = 8;

/// A pin the router connects: its shape, the way out of its block or pad,
/// and that object's outline.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Terminal {
    pub net: String,
    /// `ota.inp`, `pad ota_inp`: for messages.
    pub name: String,
    pub pin: LayerRect,
    pub facing: Facing,
    pub owner: Rect,
}

#[derive(Debug, Clone)]
pub struct Problem {
    /// Where routes may go.
    pub area: Rect,
    /// Outlines routes keep a halo from (the blocks).
    pub blocks: Vec<Rect>,
    pub terminals: Vec<Terminal>,
}

/// One net's routing, for the report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NetRoute {
    pub net: String,
    pub terminals: Vec<String>,
    /// Wire length on the grid, um, without stubs.
    pub length_um: f64,
    pub vias: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Routed {
    pub nets: Vec<NetRoute>,
    /// Every shape drawn, with its net.
    pub shapes: Vec<(String, LayerRect)>,
    /// Where each terminal's stub meets its pin, for labels: net, layer,
    /// point.
    pub pin_points: Vec<(String, String, i16, (i64, i64))>,
    /// Nets with a single terminal, which were not routed.
    pub single: Vec<String>,
    pub attempts: usize,
}

struct Grid {
    x0: i64,
    y0: i64,
    nx: usize,
    ny: usize,
}

impl Grid {
    fn idx(&self, l: usize, i: usize, j: usize) -> usize {
        (l * self.ny + j) * self.nx + i
    }

    fn pos(&self, n: usize) -> (usize, usize, usize) {
        let per = self.nx * self.ny;
        let l = n / per;
        let r = n % per;
        (l, r % self.nx, r / self.nx)
    }

    fn xy(&self, i: usize, j: usize) -> (i64, i64) {
        (self.x0 + i as i64 * GRID, self.y0 + j as i64 * GRID)
    }

    fn node_at(&self, x: i64, y: i64) -> Option<(usize, usize)> {
        let (dx, dy) = (x - self.x0, y - self.y0);
        if dx < 0 || dy < 0 || dx % GRID != 0 || dy % GRID != 0 {
            return None;
        }
        let (i, j) = ((dx / GRID) as usize, (dy / GRID) as usize);
        (i < self.nx && j < self.ny).then_some((i, j))
    }

    /// Nodes whose footprint comes closer than `CLEAR` to a rectangle
    /// (counted along each axis, which is the stricter measure).
    fn near(&self, r: &Rect) -> Vec<(usize, usize)> {
        let reach = r.expand(HALF + CLEAR - 1);
        let lo = |v: i64, o: i64| (v - o + GRID - 1).div_euclid(GRID).max(0);
        let hi = |v: i64, o: i64, n: usize| (v - o).div_euclid(GRID).min(n as i64 - 1);
        let (i0, i1) = (lo(reach.x1, self.x0), hi(reach.x2, self.x0, self.nx));
        let (j0, j1) = (lo(reach.y1, self.y0), hi(reach.y2, self.y0, self.ny));
        let mut out = Vec::new();
        for j in j0..=j1 {
            for i in i0..=i1 {
                out.push((i as usize, j as usize));
            }
        }
        out
    }
}

/// What owns a node: nobody, a block, or a net.
const FREE: u32 = 0;
const BLOCKED: u32 = u32::MAX;

fn layer_of(gds: i16) -> Option<usize> {
    match gds {
        METAL3 => Some(0),
        METAL4 => Some(1),
        _ => None,
    }
}

const LAYERS: [i16; 2] = [METAL3, METAL4];

/// A terminal's way onto the grid.
struct Access {
    node: usize,
    shapes: Vec<LayerRect>,
    /// Where the stub meets the pin.
    at: (i64, i64),
}

fn rect_around(x: i64, y: i64, half: i64) -> Rect {
    Rect::new(x - half, y - half, x + half, y + half)
}

/// The stub from a pin to the grid.
fn access(g: &Grid, t: &Terminal) -> Result<Access, String> {
    let r = &t.pin.rect;
    // The pin's middle on the side it leaves by, and the grid line beyond
    // the owner's halo that the stub runs to.
    let (ex, ey) = match t.facing {
        Facing::North => ((r.x1 + r.x2) / 2, r.y2),
        Facing::South => ((r.x1 + r.x2) / 2, r.y1),
        Facing::East => (r.x2, (r.y1 + r.y2) / 2),
        Facing::West => (r.x1, (r.y1 + r.y2) / 2),
    };
    let out = HALO + HALF + CLEAR;
    let snap = |v: i64, up: bool, origin: i64| {
        let d = v - origin;
        origin
            + if up {
                (d + GRID - 1).div_euclid(GRID) * GRID
            } else {
                d.div_euclid(GRID) * GRID
            }
    };
    let nearest =
        |v: i64, origin: i64| origin + ((v - origin) as f64 / GRID as f64).round() as i64 * GRID;
    let (nx, ny) = match t.facing {
        Facing::North => (
            nearest(ex, g.x0),
            snap(t.owner.y2.max(ey) + out, true, g.y0),
        ),
        Facing::South => (
            nearest(ex, g.x0),
            snap(t.owner.y1.min(ey) - out, false, g.y0),
        ),
        Facing::East => (
            snap(t.owner.x2.max(ex) + out, true, g.x0),
            nearest(ey, g.y0),
        ),
        Facing::West => (
            snap(t.owner.x1.min(ex) - out, false, g.x0),
            nearest(ey, g.y0),
        ),
    };
    let (i, j) = g.node_at(nx, ny).ok_or_else(|| {
        format!(
            "{} has no room to reach the routing grid ({:.2}, {:.2}) um is outside the core",
            t.name,
            nx as f64 / 1000.0,
            ny as f64 / 1000.0
        )
    })?;
    let layer = t.pin.layer;
    if !matches!(layer, METAL1 | METAL2 | METAL3 | METAL4) {
        return Err(format!(
            "{} is on a layer the chip router does not use; it routes on Metal3 and Metal4 and reaches pins on Metal1 to Metal4",
            t.name
        ));
    }
    // Inside the pin the stub is no wider than the pin and goes no deeper
    // than a wire's width, so it never reaches past the pin into the block;
    // outside, it runs to the grid line, then along it to the node.
    let depth = match t.facing {
        Facing::North | Facing::South => r.height(),
        Facing::East | Facing::West => r.width(),
    }
    .min(WIRE);
    let (inner, outer) = match t.facing {
        Facing::South => (
            Rect::new((ex - HALF).max(r.x1), ey, (ex + HALF).min(r.x2), ey + depth),
            Rect::new(ex - HALF, ny - HALF, ex + HALF, ey),
        ),
        Facing::North => (
            Rect::new((ex - HALF).max(r.x1), ey - depth, (ex + HALF).min(r.x2), ey),
            Rect::new(ex - HALF, ey, ex + HALF, ny + HALF),
        ),
        Facing::West => (
            Rect::new(ex, (ey - HALF).max(r.y1), ex + depth, (ey + HALF).min(r.y2)),
            Rect::new(nx - HALF, ey - HALF, ex, ey + HALF),
        ),
        Facing::East => (
            Rect::new(ex - depth, (ey - HALF).max(r.y1), ex, (ey + HALF).min(r.y2)),
            Rect::new(ex, ey - HALF, nx + HALF, ey + HALF),
        ),
    };
    let mut shapes = vec![
        LayerRect { layer, rect: inner },
        LayerRect { layer, rect: outer },
    ];
    let (cx, cy) = match t.facing {
        Facing::North | Facing::South => (ex, ny),
        Facing::East | Facing::West => (nx, ey),
    };
    if (cx, cy) != (nx, ny) {
        shapes.push(LayerRect {
            layer,
            rect: Rect::new(cx, cy, nx, ny).expand(HALF),
        });
    }
    // A pin below Metal3 reaches it through a stack of vias at the node,
    // each with a pad on the metals it joins.
    let node_layer = if matches!(layer, METAL1 | METAL2) {
        let stack: &[(i16, i16)] = if layer == METAL1 {
            &[(VIA1, METAL2), (VIA2, METAL3)]
        } else {
            &[(VIA2, METAL3)]
        };
        shapes.push(LayerRect {
            layer,
            rect: rect_around(nx, ny, HALF),
        });
        for (via, metal) in stack {
            shapes.push(LayerRect {
                layer: *via,
                rect: rect_around(nx, ny, VIA / 2),
            });
            shapes.push(LayerRect {
                layer: *metal,
                rect: rect_around(nx, ny, HALF),
            });
        }
        0
    } else {
        layer_of(layer).expect("Metal3 or Metal4")
    };
    Ok(Access {
        node: g.idx(node_layer, i, j),
        shapes,
        at: (ex, ey),
    })
}

/// Shortest path from any of `from` to `to` over nodes free or of `net`.
fn astar(g: &Grid, owner: &[u32], net: u32, from: &[usize], to: usize) -> Option<Vec<usize>> {
    let n = owner.len();
    let mut cost = vec![u32::MAX; n];
    let mut prev = vec![u32::MAX; n];
    let (_, ti, tj) = g.pos(to);
    let h = |k: usize| {
        let (_, i, j) = g.pos(k);
        STEP * ((i as i64 - ti as i64).unsigned_abs() + (j as i64 - tj as i64).unsigned_abs())
            as u32
    };
    let mut heap = BinaryHeap::new();
    for &s in from {
        cost[s] = 0;
        heap.push(Reverse((h(s), s)));
    }
    let usable = |k: usize| owner[k] == FREE || owner[k] == net;
    while let Some(Reverse((f, k))) = heap.pop() {
        if f > cost[k].saturating_add(h(k)) {
            continue;
        }
        if k == to {
            let mut path = vec![k];
            let mut c = k;
            while prev[c] != u32::MAX {
                c = prev[c] as usize;
                path.push(c);
            }
            path.reverse();
            return Some(path);
        }
        let (l, i, j) = g.pos(k);
        let base = cost[k];
        let mut next: [(Option<usize>, u32); 5] = [(None, 0); 5];
        let horizontal = if l == 0 { STEP } else { ACROSS };
        let vertical = if l == 1 { STEP } else { ACROSS };
        if i > 0 {
            next[0] = (Some(g.idx(l, i - 1, j)), horizontal);
        }
        if i + 1 < g.nx {
            next[1] = (Some(g.idx(l, i + 1, j)), horizontal);
        }
        if j > 0 {
            next[2] = (Some(g.idx(l, i, j - 1)), vertical);
        }
        if j + 1 < g.ny {
            next[3] = (Some(g.idx(l, i, j + 1)), vertical);
        }
        next[4] = (Some(g.idx(1 - l, i, j)), VIA_COST);
        for (m, c) in next {
            let Some(m) = m else { continue };
            if !usable(m) {
                continue;
            }
            let nc = base + c;
            if nc < cost[m] {
                cost[m] = nc;
                prev[m] = k as u32;
                heap.push(Reverse((nc + h(m), m)));
            }
        }
    }
    None
}

struct NetPlan<'a> {
    net: &'a str,
    terms: Vec<&'a Terminal>,
}

/// Route every net with two or more terminals.
pub fn route(p: &Problem) -> Result<Routed, String> {
    let x0 = (p.area.x1 + GRID - 1).div_euclid(GRID) * GRID;
    let y0 = (p.area.y1 + GRID - 1).div_euclid(GRID) * GRID;
    if p.area.x2 <= x0 || p.area.y2 <= y0 {
        return Err("the routing area is empty".into());
    }
    let nx = ((p.area.x2 - x0) / GRID + 1) as usize;
    let ny = ((p.area.y2 - y0) / GRID + 1) as usize;
    if nx.saturating_mul(ny) > MAX_NODES {
        return Err(format!(
            "the core is too large for the router's grid ({nx} x {ny} nodes; at most {MAX_NODES})"
        ));
    }
    let g = Grid { x0, y0, nx, ny };
    let mut by_net: BTreeMap<&str, Vec<&Terminal>> = BTreeMap::new();
    for t in &p.terminals {
        by_net.entry(t.net.as_str()).or_default().push(t);
    }
    let single: Vec<String> = by_net
        .iter()
        .filter(|(_, t)| t.len() < 2)
        .map(|(n, _)| n.to_string())
        .collect();
    let mut order: Vec<NetPlan> = by_net
        .into_iter()
        .filter(|(_, t)| t.len() >= 2)
        .map(|(net, terms)| NetPlan { net, terms })
        .collect();
    // Shortest first: half the perimeter of the terminals' bounding box.
    let span = |n: &NetPlan| {
        let xs = n.terms.iter().map(|t| t.pin.rect.x1);
        let ys = n.terms.iter().map(|t| t.pin.rect.y1);
        (xs.clone().max().unwrap_or(0) - xs.min().unwrap_or(0))
            + (ys.clone().max().unwrap_or(0) - ys.min().unwrap_or(0))
    };
    order.sort_by_key(|n| (span(n), n.net.to_string()));

    // The grid without routes: blocks and their halos, then every stub's
    // neighbourhood reserved for its net.
    let mut base = vec![FREE; 2 * nx * ny];
    for b in &p.blocks {
        let zone = b.expand(HALO);
        for l in 0..2 {
            for (i, j) in g.near(&zone) {
                base[g.idx(l, i, j)] = BLOCKED;
            }
        }
    }
    let net_id: BTreeMap<&str, u32> = order
        .iter()
        .enumerate()
        .map(|(k, n)| (n.net, k as u32 + 1))
        .collect();
    let mut accesses: BTreeMap<(String, String), Access> = BTreeMap::new();
    let mut stub_shapes: Vec<(&str, &str, LayerRect)> = Vec::new();
    for n in &order {
        for t in &n.terms {
            let a = access(&g, t)?;
            if base[a.node] == BLOCKED {
                return Err(format!(
                    "{} reaches the routing grid inside another block's halo; move the blocks apart",
                    t.name
                ));
            }
            for s in &a.shapes {
                stub_shapes.push((n.net, &t.name, s.clone()));
            }
            accesses.insert((n.net.to_string(), t.name.clone()), a);
        }
    }
    // Stubs of different nets must keep the spacing rule between them.
    for (k, (na, ta, a)) in stub_shapes.iter().enumerate() {
        for (nb, tb, b) in &stub_shapes[k + 1..] {
            let rule = if a.layer == METAL1 { 180.0 } else { 210.0 };
            if na != nb && a.layer == b.layer && a.rect.distance(&b.rect) < rule {
                return Err(format!(
                    "the pins {ta} and {tb} are too close for the chip router to reach both; move one of the blocks"
                ));
            }
        }
    }
    // Nodes near a stub belong to its net: another net there would come
    // closer than the spacing rule. Halo nodes stay blocked.
    let claim = |base: &mut Vec<u32>, k: usize, id: u32, what: &str| -> Result<(), String> {
        match base[k] {
            FREE => {
                base[k] = id;
                Ok(())
            }
            BLOCKED => Ok(()),
            v if v == id => Ok(()),
            _ => Err(format!(
                "{what} comes too close to another net's pin; move the blocks apart"
            )),
        }
    };
    for (net, name, s) in &stub_shapes {
        let Some(l) = layer_of(s.layer) else { continue };
        for (i, j) in g.near(&s.rect) {
            claim(&mut base, g.idx(l, i, j), net_id[net], name)?;
        }
    }
    for ((net, name), a) in &accesses {
        let id = net_id[net.as_str()];
        if base[a.node] != id {
            claim(&mut base, a.node, id, name)?;
            base[a.node] = id;
        }
    }

    let mut attempts = 0;
    loop {
        attempts += 1;
        let mut owner = base.clone();
        let mut paths: Vec<(usize, Vec<Vec<usize>>)> = Vec::new();
        let mut failed: Option<usize> = None;
        for (k, n) in order.iter().enumerate() {
            let id = net_id[n.net];
            let nodes: Vec<usize> = n
                .terms
                .iter()
                .map(|t| accesses[&(n.net.to_string(), t.name.clone())].node)
                .collect();
            let mut tree: Vec<usize> = vec![nodes[0]];
            let mut left: Vec<usize> = nodes[1..].to_vec();
            let mut net_paths = Vec::new();
            while !left.is_empty() {
                // The terminal nearest the tree goes next.
                let (pick, _) = left
                    .iter()
                    .enumerate()
                    .map(|(q, &t)| {
                        let (_, ti, tj) = g.pos(t);
                        let d = tree
                            .iter()
                            .map(|&s| {
                                let (_, si, sj) = g.pos(s);
                                (si as i64 - ti as i64).abs() + (sj as i64 - tj as i64).abs()
                            })
                            .min()
                            .unwrap_or(0);
                        (q, d)
                    })
                    .min_by_key(|&(q, d)| (d, q))
                    .expect("terminals left");
                let target = left.remove(pick);
                if tree.contains(&target) {
                    continue;
                }
                match astar(&g, &owner, id, &tree, target) {
                    Some(path) => {
                        for &v in &path {
                            owner[v] = id;
                            if !tree.contains(&v) {
                                tree.push(v);
                            }
                        }
                        net_paths.push(path);
                    }
                    None => {
                        failed = Some(k);
                        break;
                    }
                }
            }
            if failed.is_some() {
                break;
            }
            paths.push((k, net_paths));
        }
        match failed {
            None => return Ok(emit(&g, &order, &paths, &accesses, single, attempts)),
            Some(k) if attempts < ATTEMPTS && k > 0 => {
                let n = order.remove(k);
                order.insert(0, n);
            }
            Some(k) => {
                let n = &order[k];
                return Err(format!(
                    "net {} could not be routed between {}; give the blocks more room or move the pads",
                    n.net,
                    n.terms
                        .iter()
                        .map(|t| t.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }
}

fn emit(
    g: &Grid,
    order: &[NetPlan],
    paths: &[(usize, Vec<Vec<usize>>)],
    accesses: &BTreeMap<(String, String), Access>,
    single: Vec<String>,
    attempts: usize,
) -> Routed {
    let mut shapes = Vec::new();
    let mut nets = Vec::new();
    let mut pin_points = Vec::new();
    for (k, net_paths) in paths {
        let n = &order[*k];
        let net = n.net.to_string();
        let mut length = 0i64;
        let mut vias = 0usize;
        for t in &n.terms {
            let a = &accesses[&(net.clone(), t.name.clone())];
            for s in &a.shapes {
                shapes.push((net.clone(), s.clone()));
            }
            pin_points.push((net.clone(), t.name.clone(), t.pin.layer, a.at));
        }
        for path in net_paths {
            // Runs of one layer in one direction become one rectangle; a
            // change of layer is a via with a pad on both layers.
            let mut s = 0;
            while s + 1 < path.len() {
                let (la, ia, ja) = g.pos(path[s]);
                let (lb, ib, jb) = g.pos(path[s + 1]);
                if la != lb {
                    let (x, y) = g.xy(ia, ja);
                    vias += 1;
                    shapes.push((
                        net.clone(),
                        LayerRect {
                            layer: VIA3,
                            rect: rect_around(x, y, VIA / 2),
                        },
                    ));
                    for l in LAYERS {
                        shapes.push((
                            net.clone(),
                            LayerRect {
                                layer: l,
                                rect: rect_around(x, y, HALF),
                            },
                        ));
                    }
                    s += 1;
                    continue;
                }
                let horizontal = ja == jb;
                let mut e = s + 1;
                while e + 1 < path.len() {
                    let (lc, ic, jc) = g.pos(path[e + 1]);
                    if lc != la || (horizontal && jc != ja) || (!horizontal && ic != ia) {
                        break;
                    }
                    e += 1;
                }
                let (_, ie, je) = g.pos(path[e]);
                let (x0, y0) = g.xy(ia, ja);
                let (x1, y1) = g.xy(ie, je);
                let _ = ib;
                length += (x1 - x0).abs() + (y1 - y0).abs();
                shapes.push((
                    net.clone(),
                    LayerRect {
                        layer: LAYERS[la],
                        rect: Rect::new(x0, y0, x1, y1).expand(HALF),
                    },
                ));
                s = e;
            }
        }
        nets.push(NetRoute {
            net,
            terminals: n.terms.iter().map(|t| t.name.clone()).collect(),
            length_um: length as f64 / 1000.0,
            vias,
        });
    }
    nets.sort_by(|a, b| a.net.cmp(&b.net));
    Routed {
        nets,
        shapes,
        pin_points,
        single,
        attempts,
    }
}

/// Shapes of different nets on one layer closer than the metal spacing
/// rule (0.21 um), and nets whose shapes do not form one connected piece
/// together with their pins. Empty for a good routing; tests use it.
pub fn check(routed: &Routed, terminals: &[Terminal]) -> Vec<String> {
    let mut out = Vec::new();
    let s = &routed.shapes;
    for (k, (na, a)) in s.iter().enumerate() {
        for (nb, b) in &s[k + 1..] {
            // Metal1 0.18 um, Metal2 to Metal4 0.21 um, vias 0.22 um.
            let rule = match a.layer {
                VIA1 | VIA2 | VIA3 => 220.0,
                METAL1 => 180.0,
                _ => 210.0,
            };
            if na != nb && a.layer == b.layer && a.rect.distance(&b.rect) < rule {
                out.push(format!(
                    "{na} and {nb} on layer {} are too close: {:?} {:?}",
                    a.layer, a.rect, b.rect
                ));
            }
        }
    }
    // Connectivity per net: shapes touching on a layer, or a via joining a
    // point on two layers.
    let mut nets: BTreeMap<&str, Vec<LayerRect>> = BTreeMap::new();
    for (n, r) in s {
        nets.entry(n.as_str()).or_default().push(r.clone());
    }
    for t in terminals {
        if let Some(v) = nets.get_mut(t.net.as_str()) {
            v.push(t.pin.clone());
        }
    }
    let joins = |a: &LayerRect, b: &LayerRect| {
        let cut = |l: i16| matches!(l, VIA1 | VIA2 | VIA3);
        let touch = a.rect.touches(&b.rect);
        touch
            && (a.layer == b.layer
                || (cut(a.layer) && via_joins(a.layer, b.layer))
                || (cut(b.layer) && via_joins(b.layer, a.layer)))
    };
    for (net, rects) in &nets {
        if !routed.nets.iter().any(|r| r.net == *net) {
            continue;
        }
        let mut seen = vec![false; rects.len()];
        let mut stack = vec![0usize];
        seen[0] = true;
        while let Some(k) = stack.pop() {
            for q in 0..rects.len() {
                if !seen[q] && joins(&rects[k], &rects[q]) {
                    seen[q] = true;
                    stack.push(q);
                }
            }
        }
        if seen.iter().any(|s| !s) {
            let apart: Vec<String> = rects
                .iter()
                .zip(&seen)
                .filter(|(_, s)| !**s)
                .take(4)
                .map(|(r, _)| format!("{} {:?}", r.layer, r.rect.um()))
                .collect();
            out.push(format!(
                "net {net} is not connected (apart: {})",
                apart.join(", ")
            ));
        }
    }
    out
}

fn via_joins(via: i16, metal: i16) -> bool {
    matches!(
        (via, metal),
        (VIA1, METAL1)
            | (VIA1, METAL2)
            | (VIA2, METAL2)
            | (VIA2, METAL3)
            | (VIA3, METAL3)
            | (VIA3, METAL4)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(
        net: &str,
        name: &str,
        layer: i16,
        rect: Rect,
        facing: Facing,
        owner: Rect,
    ) -> Terminal {
        Terminal {
            net: net.into(),
            name: name.into(),
            pin: LayerRect { layer, rect },
            facing,
            owner,
        }
    }

    /// Two blocks with edge pins facing down, and pads on the left.
    fn problem() -> Problem {
        let area = Rect::new(0, 0, 120_000, 90_000);
        let a = Rect::new(30_000, 40_000, 50_000, 60_000);
        let b = Rect::new(70_000, 40_000, 95_000, 60_000);
        let pin = |x: i64, owner: Rect| Rect::new(x - 150, owner.y1, x + 150, owner.y1 + 5_000);
        let left = Rect::new(-10_000, 0, 0, 90_000);
        let pad = |y: i64| Rect::new(-300, y - 250, 0, y + 250);
        Problem {
            area,
            blocks: vec![a, b],
            terminals: vec![
                term("x", "a.out", METAL4, pin(31_200, a), Facing::South, a),
                term("x", "b.in", METAL4, pin(72_000, b), Facing::South, b),
                term("y", "a.inp", METAL4, pin(32_400, a), Facing::South, a),
                term("y", "pad y", METAL3, pad(20_150), Facing::East, left),
                term("z", "b.vdd", METAL4, pin(73_200, b), Facing::South, b),
                term("z", "a.vdd", METAL4, pin(33_600, a), Facing::South, a),
                term("z", "pad z", METAL3, pad(70_000), Facing::East, left),
                term("w", "b.lonely", METAL4, pin(74_400, b), Facing::South, b),
            ],
        }
    }

    #[test]
    fn nets_are_routed_connected_and_apart() {
        let p = problem();
        let r = route(&p).unwrap();
        assert_eq!(r.nets.len(), 3);
        assert_eq!(r.single, ["w"]);
        let problems = check(&r, &p.terminals);
        assert!(problems.is_empty(), "{problems:#?}");
        // Nothing enters a block.
        for (_, s) in &r.shapes {
            for b in &p.blocks {
                let inside =
                    s.rect.x1 > b.x1 && s.rect.x2 < b.x2 && s.rect.y1 > b.y1 && s.rect.y2 < b.y2;
                assert!(!inside, "{s:?} inside {b:?}");
            }
        }
        // The same problem routes the same way.
        assert_eq!(route(&p).unwrap(), r);
    }

    #[test]
    fn pins_too_close_together_are_named() {
        let mut p = problem();
        p.terminals[2].pin.rect = p.terminals[0].pin.rect.translate(350, 0);
        let e = route(&p).unwrap_err();
        assert!(e.contains("too close"), "{e}");
    }

    #[test]
    fn a_net_walled_in_is_named() {
        let mut p = problem();
        // A wall that shuts the pad in against the edge of the area.
        p.blocks.push(Rect::new(4_000, 0, 6_000, 90_000));
        let e = route(&p).unwrap_err();
        assert!(e.contains("could not be routed"), "{e}");
    }

    #[test]
    fn random_problems_route_clean_or_fail_cleanly() {
        // A small deterministic generator, so the test is the same every run.
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut next = |m: i64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % m as u64) as i64
        };
        for case in 0..20 {
            let area = Rect::new(0, 0, 150_000, 150_000);
            let mut blocks = Vec::new();
            let mut terminals = Vec::new();
            for b in 0..3 {
                let x = 10_000 + b * 45_000 + next(5) * 1_200;
                let y = 30_000 + next(60) * 1_200;
                let r = Rect::new(x, y, x + 25_000, y + 20_000);
                for k in 0..4 {
                    let px = x + 1_200 + k * 2_400;
                    terminals.push(term(
                        &format!("n{}", next(5)),
                        &format!("b{b}.p{k}"),
                        METAL4,
                        Rect::new(px - 150, y, px + 150, y + 3_000),
                        Facing::South,
                        r,
                    ));
                }
                blocks.push(r);
            }
            let p = Problem {
                area,
                blocks,
                terminals,
            };
            match route(&p) {
                Ok(r) => {
                    let problems = check(&r, &p.terminals);
                    assert!(problems.is_empty(), "case {case}: {problems:#?}");
                }
                Err(e) => assert!(e.contains("could not be routed"), "case {case}: {e}"),
            }
        }
    }
}
