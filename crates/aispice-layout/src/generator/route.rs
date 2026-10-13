//! Stacking rows and joining their terminals: a small deterministic router.
//!
//! Rows are stacked bottom to top and centred on the widest. Below each row
//! is a channel of Metal3 tracks for its source, drain and ring nets, above
//! it a channel for its gate nets; every terminal reaches its net's track
//! on a straight Metal2 stub with a Via2. A net with tracks in more than one
//! channel gets a Metal4 column to the right of the rows, with a Via3 on
//! each of its tracks. Track order follows net order, so the same input
//! always gives the same layout.
//!
//! A symmetric block (a comparator) names the net pairs that mirror each
//! other about its vertical axis. Its routing is then mirrored too: in each
//! channel a pair's two nets share one track height, each running from its
//! own side's terminals to its own side's column; the left net's column
//! stands left of the rows and its partner's at the same distance right of
//! them, and a net that is its own mirror image (a supply, the clock) gets a
//! column on each side. A pair whose nets reach the other side of the axis
//! (a latch's cross-coupled gates) gets columns on both sides for each net,
//! and the two nets change sides once, in the first channel where they
//! cross: there they take two adjacent full-width tracks, everywhere else
//! each reaches only its own side's column. Where a pair's two nets
//! interleave in a row, they also take two adjacent tracks, the inner one
//! balancing the stubs.

use super::Params;
use super::plan::{Label, Layer, Plan, Rect};
use super::row::{RowLayout, Terminal};
use super::rules::*;
use std::collections::{BTreeMap, BTreeSet};

/// Width of a track or spine column.
pub const TRACK_W: i64 = 300;
/// Space from a ring's outer edge to the nearest track.
const TRACK_GAP: i64 = 500;
/// Space from the widest ring to the first spine column.
const SPINE_GAP: i64 = 600;
/// A track is at least this long (`Mn.d`, 0.144 um2 at 0.3 um wide).
const MIN_TRACK: i64 = 600;

struct Channel {
    /// Track centre per net, in order.
    tracks: Vec<(String, i64)>,
    /// Crossing pairs that change sides in this channel.
    hosts: BTreeSet<usize>,
    terminals: Vec<Terminal>,
    /// Stubs run downward from the terminals (true) or upward.
    down: bool,
}

/// Nets in the order they first appear from left to right, `last` moved to
/// the end (the ring's rail sits outermost).
fn net_order(terms: &[Terminal], last: Option<&str>) -> Vec<String> {
    let mut sorted: Vec<&Terminal> = terms.iter().collect();
    sorted.sort_by_key(|t| t.x);
    let mut out: Vec<String> = Vec::new();
    for t in sorted {
        if !out.contains(&t.net) {
            out.push(t.net.clone());
        }
    }
    if let Some(l) = last
        && let Some(i) = out.iter().position(|n| n == l)
    {
        let n = out.remove(i);
        out.push(n);
    }
    out
}

/// The net pairs of a symmetric block.
struct Mirror<'a> {
    pairs: &'a [(String, String)],
    /// Pairs whose left net has a terminal right of the axis, or whose right
    /// net one left of it.
    crossing: BTreeSet<usize>,
}

/// Where a net's terminals in a channel sit against the axis.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
    Both,
}

fn side(xs: impl Iterator<Item = i64> + Clone, axis: i64) -> Side {
    let (lo, hi) = (xs.clone().min().unwrap_or(axis), xs.max().unwrap_or(axis));
    if hi < axis {
        Side::Left
    } else if lo > axis {
        Side::Right
    } else {
        Side::Both
    }
}

impl Mirror<'_> {
    /// The pair a net belongs to: its index and the left and right nets.
    fn pair(&self, net: &str) -> Option<(usize, &str, &str)> {
        self.pairs
            .iter()
            .enumerate()
            .find(|(_, (l, r))| l == net || r == net)
            .map(|(k, (l, r))| (k, l.as_str(), r.as_str()))
    }

    fn is_right(&self, net: &str) -> bool {
        self.pairs.iter().any(|(_, r)| r == net)
    }

    /// A channel's nets in track order, inner first: one net per track, but
    /// the two nets of a pair on one track when they stay apart, and on two
    /// adjacent tracks when they overlap. Then the net on the outer track
    /// has the longer stubs; `stacked` keeps, per pair, how many more outer
    /// stubs the left net has had so far, and the inner track goes to the
    /// net that keeps that nearer zero.
    ///
    /// A crossing pair changes sides in the first channel where its nets
    /// cross (recorded in `hosted`, and returned): there it takes two
    /// tracks; elsewhere its crossed nets share one, each running to its own
    /// side's column.
    fn slots(
        &self,
        nets: &[String],
        terms: &[Terminal],
        axis: i64,
        shield: &str,
        stacked: &mut BTreeMap<usize, i64>,
        hosted: &mut BTreeSet<usize>,
    ) -> (Vec<Vec<String>>, BTreeSet<usize>) {
        let mut hosts = BTreeSet::new();
        if self.pairs.is_empty() {
            return (nets.iter().map(|n| vec![n.clone()]).collect(), hosts);
        }
        let span = |n: &str| {
            let xs = terms.iter().filter(|t| t.net == n).map(|t| t.x);
            (xs.clone().min().unwrap_or(0), xs.max().unwrap_or(0))
        };
        let count = |n: &str| terms.iter().filter(|t| t.net == n).count() as i64;
        // A track's extent as drawn below, grown to MIN_TRACK when short. A
        // pair's columns stand outside it (the left net's on the left), so
        // they never bring the two tracks closer.
        let extent = |n: &str| {
            let (a, b) = span(n);
            let (x1, x2) = (a - TRACK_W / 2, b + TRACK_W / 2);
            let grow = snap_up((MIN_TRACK - (x2 - x1)).max(0) / 2);
            (x1 - grow, x2 + grow)
        };
        let mut done: BTreeSet<&str> = BTreeSet::new();
        let mut out = Vec::new();
        for n in nets {
            if done.contains(n.as_str()) {
                continue;
            }
            match self.pair(n) {
                Some((k, l, r)) if nets.iter().any(|x| x == l) && nets.iter().any(|x| x == r) => {
                    done.insert(l);
                    done.insert(r);
                    let at = |n: &str| side(terms.iter().filter(|t| t.net == n).map(|t| t.x), axis);
                    // The net whose terminals sit left, and the other: a
                    // crossing pair's nets each run to their own side's
                    // column, so they may sit either way round.
                    let (a, b) = match (at(l), at(r)) {
                        (Side::Right, Side::Left) if self.crossing.contains(&k) => {
                            if hosted.insert(k) {
                                hosts.insert(k);
                            }
                            (r, l)
                        }
                        _ => (l, r),
                    };
                    if !hosts.contains(&k) && extent(a).1 + MN_SPACE <= extent(b).0 {
                        out.push(vec![l.to_string(), r.to_string()]);
                    } else {
                        let e = stacked.entry(k).or_insert(0);
                        let (left_inner, right_inner) = (*e - count(r), *e + count(l));
                        let (a, b) = if left_inner.abs() <= right_inner.abs() {
                            *e = left_inner;
                            (l, r)
                        } else {
                            *e = right_inner;
                            (r, l)
                        };
                        // Where the pair changes sides, a track of the row's
                        // ring net on each side of the two (when the ring
                        // net has no terminal here), so both see the same
                        // neighbours.
                        let shielded = hosts.contains(&k) && !terms.iter().any(|t| t.net == shield);
                        if shielded {
                            out.push(vec![shield.to_string()]);
                        }
                        out.push(vec![a.to_string()]);
                        out.push(vec![b.to_string()]);
                        if shielded {
                            out.push(vec![shield.to_string()]);
                        }
                    }
                }
                _ => {
                    done.insert(n);
                    out.push(vec![n.clone()]);
                }
            }
        }
        (out, hosts)
    }
}

fn moved(t: &Terminal, dx: i64, dy: i64) -> Terminal {
    Terminal {
        x: t.x + dx,
        via: t.via.translate(dx, dy),
        net: t.net.clone(),
    }
}

/// Stack the rows, route them and label the ports. Returns the plan and the
/// nets that needed a spine column. `mirror` names the net pairs of a
/// symmetric block (left net first), whose routing is then mirrored about
/// the rows' axis; empty for any other block.
pub fn assemble(
    cell: &str,
    rows: &[RowLayout],
    ports: &[String],
    params: &Params,
    mirror: &[(String, String)],
) -> (Plan, Vec<String>) {
    let mut sym = Mirror {
        pairs: mirror,
        crossing: BTreeSet::new(),
    };
    for row in rows {
        let axis = (row.ring.x1 + row.ring.x2) / 2;
        for t in row.down.iter().chain(&row.up) {
            if let Some((k, l, r)) = sym.pair(&t.net)
                && ((t.net == l && t.x > axis) || (t.net == r && t.x < axis))
            {
                sym.crossing.insert(k);
            }
        }
    }
    let mut stacked: BTreeMap<usize, i64> = BTreeMap::new();
    let mut hosted: BTreeSet<usize> = BTreeSet::new();
    let pitch = params.track_pitch_nm();
    let row_gap = params.row_gap_nm();
    let mut plan = Plan {
        cell: cell.to_string(),
        ..Default::default()
    };
    let widest = rows.iter().map(|r| r.ring.width()).max().unwrap_or(0);
    let mut channels: Vec<Channel> = Vec::new();
    let mut ring_right = i64::MIN;
    let mut ring_left = i64::MAX;
    let mut y = 0i64;
    for row in rows {
        let axis = (row.ring.x1 + row.ring.x2) / 2;
        let (down_nets, down_hosts) = sym.slots(
            &net_order(&row.down, Some(&row.bulk)),
            &row.down,
            axis,
            &row.bulk,
            &mut stacked,
            &mut hosted,
        );
        let (up_nets, up_hosts) = sym.slots(
            &net_order(&row.up, None),
            &row.up,
            axis,
            &row.bulk,
            &mut stacked,
            &mut hosted,
        );
        let nd = down_nets.len() as i64;
        let ring_bottom = y + TRACK_W + (nd - 1).max(0) * pitch + TRACK_GAP;
        let dy = ring_bottom - row.ring.y1;
        let dx = snap_down((widest - row.ring.width()) / 2) - row.ring.x1;
        let mut p = row.plan.clone();
        p.translate(dx, dy);
        plan.extend(p);
        ring_right = ring_right.max(row.ring.x2 + dx);
        ring_left = ring_left.min(row.ring.x1 + dx);
        channels.push(Channel {
            tracks: down_nets
                .iter()
                .enumerate()
                .flat_map(|(i, slot)| {
                    slot.iter().map(move |n| {
                        (
                            n.clone(),
                            ring_bottom - TRACK_GAP - TRACK_W / 2 - i as i64 * pitch,
                        )
                    })
                })
                .collect(),
            terminals: row.down.iter().map(|t| moved(t, dx, dy)).collect(),
            down: true,
            hosts: down_hosts,
        });
        let ring_top = row.ring.y2 + dy;
        let nu = up_nets.len() as i64;
        channels.push(Channel {
            tracks: up_nets
                .iter()
                .enumerate()
                .flat_map(|(i, slot)| {
                    slot.iter().map(move |n| {
                        (
                            n.clone(),
                            ring_top + TRACK_GAP + TRACK_W / 2 + i as i64 * pitch,
                        )
                    })
                })
                .collect(),
            terminals: row.up.iter().map(|t| moved(t, dx, dy)).collect(),
            down: false,
            hosts: up_hosts,
        });
        y = ring_top + TRACK_GAP + TRACK_W + (nu - 1).max(0) * pitch + row_gap;
    }

    // Nets with tracks in more than one channel get a spine column, in the
    // order they first appear from the bottom.
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    let mut order: Vec<&str> = Vec::new();
    for c in &channels {
        for (n, _) in &c.tracks {
            let e = seen.entry(n).or_insert(0);
            if *e == 0 {
                order.push(n);
            }
            *e += 1;
        }
    }
    let spine: Vec<String> = order
        .iter()
        .filter(|n| seen[*n] > 1)
        .map(|n| n.to_string())
        .collect();
    let right_x = |j: usize| ring_right + SPINE_GAP + TRACK_W / 2 + j as i64 * pitch;
    let left_x = |j: usize| ring_left - SPINE_GAP - TRACK_W / 2 - j as i64 * pitch;
    let mut spine_x: BTreeMap<&str, Vec<i64>> = BTreeMap::new();
    if mirror.is_empty() {
        for (j, n) in spine.iter().enumerate() {
            spine_x.insert(n.as_str(), vec![right_x(j)]);
        }
    } else {
        // Column j on each side: a pair's two nets, or a net that is its
        // own mirror image on both. A crossing pair takes two columns a
        // side: the left net's at j on the left and j + 1 on the right, its
        // partner's the mirror image.
        let mut j = 0;
        for n in &spine {
            if spine_x.contains_key(n.as_str()) {
                continue;
            }
            match sym.pair(n) {
                Some((k, l, r)) if sym.crossing.contains(&k) => {
                    spine_x.insert(l, vec![left_x(j), right_x(j + 1)]);
                    spine_x.insert(r, vec![right_x(j), left_x(j + 1)]);
                    j += 2;
                    continue;
                }
                Some((_, l, r)) => {
                    for m in [l, r] {
                        if spine.iter().any(|s| s == m) {
                            let x = if sym.is_right(m) {
                                right_x(j)
                            } else {
                                left_x(j)
                            };
                            spine_x.insert(m, vec![x]);
                        }
                    }
                }
                None => {
                    spine_x.insert(n.as_str(), vec![left_x(j), right_x(j)]);
                }
            }
            j += 1;
        }
    }

    let axis = (ring_left + ring_right) / 2;
    let crossing = |net: &str| {
        sym.pair(net)
            .filter(|(k, _, _)| sym.crossing.contains(k))
            .map(|(k, _, _)| k)
    };
    let mut spine_span: BTreeMap<(&str, i64), (i64, i64)> = BTreeMap::new();
    let mut first_track: BTreeMap<String, (i64, i64, i64)> = BTreeMap::new();
    for c in &channels {
        for (net, ty) in &c.tracks {
            let xs: Vec<i64> = c
                .terminals
                .iter()
                .filter(|t| &t.net == net)
                .map(|t| t.x)
                .collect();
            let mut x1 = xs.iter().copied().min().unwrap_or(0) - TRACK_W / 2;
            let mut x2 = xs.iter().copied().max().unwrap_or(0) + TRACK_W / 2;
            // A crossing pair's net reaches only the columns on the side its
            // terminals here are on, except where the pair changes sides.
            let at = side(xs.iter().copied(), axis);
            let reach = |sx: &i64| match crossing(net) {
                None => true,
                Some(k) if c.hosts.contains(&k) => true,
                Some(_) => at == Side::Both || (at == Side::Left) == (*sx < axis),
            };
            for &sx in spine_x
                .get(net.as_str())
                .into_iter()
                .flatten()
                .filter(|sx| reach(sx))
            {
                if sx > x1 {
                    x2 = x2.max(sx + VIA / 2 + VIA_ENDCAP);
                } else {
                    x1 = x1.min(sx - VIA / 2 - VIA_ENDCAP);
                }
                plan.add(Layer::Via3, Rect::centred(sx, *ty, VIA, VIA), Some(net));
                let span = spine_span.entry((net.as_str(), sx)).or_insert((*ty, *ty));
                span.0 = span.0.min(*ty);
                span.1 = span.1.max(*ty);
            }
            if x2 - x1 < MIN_TRACK {
                let grow = MIN_TRACK - (x2 - x1);
                x1 -= snap_up(grow / 2);
                x2 += snap_up(grow / 2);
            }
            plan.add(
                Layer::Metal3,
                Rect::new(x1, ty - TRACK_W / 2, x2, ty + TRACK_W / 2),
                Some(net),
            );
            first_track.entry(net.clone()).or_insert((x1, x2, *ty));
        }
        for t in &c.terminals {
            let ty = c
                .tracks
                .iter()
                .find(|(n, _)| *n == t.net)
                .map(|(_, y)| *y)
                .expect("every terminal's net has a track in its channel");
            let stub = if c.down {
                Rect::new(
                    t.x - MN_WIDTH / 2,
                    ty - VIA / 2 - VIA_ENDCAP,
                    t.x + MN_WIDTH / 2,
                    t.via.y2 + VIA_ENDCAP,
                )
            } else {
                Rect::new(
                    t.x - MN_WIDTH / 2,
                    t.via.y1 - VIA_ENDCAP,
                    t.x + MN_WIDTH / 2,
                    ty + VIA / 2 + VIA_ENDCAP,
                )
            };
            plan.add(Layer::Metal2, stub, Some(&t.net));
            plan.add(Layer::Via2, Rect::centred(t.x, ty, VIA, VIA), Some(&t.net));
        }
    }
    for ((net, sx), (lo, hi)) in &spine_span {
        let sx = *sx;
        plan.add(
            Layer::Metal4,
            Rect::new(
                sx - TRACK_W / 2,
                lo - VIA / 2 - VIA_ENDCAP,
                sx + TRACK_W / 2,
                hi + VIA / 2 + VIA_ENDCAP,
            ),
            Some(net),
        );
    }
    if params.pin_edge {
        edge_pins(&mut plan, ports, &first_track, &sym);
    } else {
        for port in ports {
            if let Some(&(x1, _, ty)) = first_track.get(port) {
                plan.labels.push(Label {
                    layer: Layer::Metal3,
                    text: port.clone(),
                    x: x1 + TRACK_W / 2,
                    y: ty,
                    pin: true,
                });
            }
        }
    }
    (plan, spine)
}

/// Pin columns of an edge-pinned block are this far apart, on a grid fixed
/// to the block's origin: twice the chip router's 0.6 um grid, so a router
/// reaches each from below with room on both sides.
pub const PIN_PITCH: i64 = 1200;

/// Bring every port to the bottom edge of the block: a Metal4 column per
/// port to the left of everything, on the [`PIN_PITCH`] grid, joined to the
/// port's first Metal3 track by extending that track left to a Via3. The
/// pin is the column's foot. Columns are vertical on Metal4 and extensions
/// horizontal on Metal3 at the tracks' own heights, which no other net uses
/// in their channel (or only a mirror partner on the other side of the
/// axis), so no two nets meet on one layer. In a symmetric block the right
/// net of a pair has its column right of everything, as far out as its
/// partner's on the left (to the nearest grid line), and a net that is its
/// own mirror image gets a second column there too, the same metal without
/// the pin, so neither side carries more.
fn edge_pins(
    plan: &mut Plan,
    ports: &[String],
    first_track: &BTreeMap<String, (i64, i64, i64)>,
    sym: &Mirror,
) {
    let Some(bbox) = plan.bbox() else {
        return;
    };
    let first = (bbox.x1 - SPINE_GAP - TRACK_W / 2).div_euclid(PIN_PITCH) * PIN_PITCH;
    // A left column's mirror image on the right, on the nearest grid line
    // that clears the block.
    let clear = bbox.x2 + SPINE_GAP + TRACK_W / 2;
    let mirrored = |x: i64| {
        let m = (bbox.x1 + bbox.x2 - x + PIN_PITCH / 2).div_euclid(PIN_PITCH) * PIN_PITCH;
        if m < clear { m + PIN_PITCH } else { m }
    };
    // The feet stand below everything else, so even the column of a track
    // at the very bottom is long enough for `Mn.d` (0.144 um2).
    let bottom = bbox.y1 - PIN_FOOT;
    let pinned: Vec<(&String, (i64, i64, i64))> = ports
        .iter()
        .filter_map(|p| first_track.get(p).map(|t| (p, *t)))
        .collect();
    let left: Vec<&String> = pinned
        .iter()
        .map(|(p, _)| *p)
        .filter(|p| !sym.is_right(p))
        .collect();
    // A column from the track at `ty` down to the bottom edge, joined to the
    // track's end at `reach` by extending the track.
    let column = |plan: &mut Plan, port: &str, cx: i64, reach: i64, ty: i64| {
        let (a, b) = if cx < reach {
            (cx - VIA / 2 - VIA_ENDCAP, reach)
        } else {
            (reach, cx + VIA / 2 + VIA_ENDCAP)
        };
        plan.add(
            Layer::Metal3,
            Rect::new(a, ty - TRACK_W / 2, b, ty + TRACK_W / 2),
            Some(port),
        );
        plan.add(Layer::Via3, Rect::centred(cx, ty, VIA, VIA), Some(port));
        plan.add(
            Layer::Metal4,
            Rect::new(
                cx - TRACK_W / 2,
                bottom,
                cx + TRACK_W / 2,
                ty + VIA / 2 + VIA_ENDCAP,
            ),
            Some(port),
        );
    };
    let left_x = |i: usize| first - i as i64 * PIN_PITCH;
    for (port, (x1, x2, ty)) in pinned {
        let cx = if sym.is_right(port) {
            let partner = sym.pair(port).map(|(_, l, _)| l).unwrap_or("");
            let i = left
                .iter()
                .position(|p| *p == partner)
                .unwrap_or(left.len());
            let cx = mirrored(left_x(i));
            column(plan, port, cx, x2 - TRACK_W, ty);
            cx
        } else {
            let i = left.iter().position(|p| *p == port).unwrap_or(0);
            let cx = left_x(i);
            column(plan, port, cx, x1 + TRACK_W, ty);
            if !sym.pairs.is_empty() && sym.pair(port).is_none() {
                column(plan, port, mirrored(cx), x2 - TRACK_W, ty);
            }
            cx
        };
        plan.labels.push(Label {
            layer: Layer::Metal4,
            text: port.clone(),
            x: cx,
            y: bottom + PIN_LABEL_UP,
            pin: true,
        });
    }
}

/// How far the pin columns reach below the rest of the block.
const PIN_FOOT: i64 = 600;

/// The pin label sits this far above the column's foot, so the pin shape
/// drawn around it (0.2 um square) ends on the block's edge.
const PIN_LABEL_UP: i64 = 100;
