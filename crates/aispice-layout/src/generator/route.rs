//! Stacking rows and joining their terminals: a small deterministic router.
//!
//! Rows are stacked bottom to top and centred on the widest. Below each row
//! is a channel of Metal3 tracks for its source, drain and ring nets, above
//! it a channel for its gate nets; every terminal reaches its net's track
//! on a straight Metal2 stub with a Via2. A net with tracks in more than one
//! channel gets a Metal4 column to the right of the rows, with a Via3 on
//! each of its tracks. Track order follows net order, so the same input
//! always gives the same layout.

use super::Params;
use super::plan::{Label, Layer, Plan, Rect};
use super::row::{RowLayout, Terminal};
use super::rules::*;
use std::collections::BTreeMap;

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

fn moved(t: &Terminal, dx: i64, dy: i64) -> Terminal {
    Terminal {
        x: t.x + dx,
        via: t.via.translate(dx, dy),
        net: t.net.clone(),
    }
}

/// Stack the rows, route them and label the ports. Returns the plan and the
/// nets that needed a spine column.
pub fn assemble(
    cell: &str,
    rows: &[RowLayout],
    ports: &[String],
    params: &Params,
) -> (Plan, Vec<String>) {
    let pitch = params.track_pitch_nm();
    let row_gap = params.row_gap_nm();
    let mut plan = Plan {
        cell: cell.to_string(),
        ..Default::default()
    };
    let widest = rows.iter().map(|r| r.ring.width()).max().unwrap_or(0);
    let mut channels: Vec<Channel> = Vec::new();
    let mut ring_right = i64::MIN;
    let mut y = 0i64;
    for row in rows {
        let down_nets = net_order(&row.down, Some(&row.bulk));
        let up_nets = net_order(&row.up, None);
        let nd = down_nets.len() as i64;
        let ring_bottom = y + TRACK_W + (nd - 1).max(0) * pitch + TRACK_GAP;
        let dy = ring_bottom - row.ring.y1;
        let dx = snap_down((widest - row.ring.width()) / 2) - row.ring.x1;
        let mut p = row.plan.clone();
        p.translate(dx, dy);
        plan.extend(p);
        ring_right = ring_right.max(row.ring.x2 + dx);
        channels.push(Channel {
            tracks: down_nets
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    (
                        n.clone(),
                        ring_bottom - TRACK_GAP - TRACK_W / 2 - i as i64 * pitch,
                    )
                })
                .collect(),
            terminals: row.down.iter().map(|t| moved(t, dx, dy)).collect(),
            down: true,
        });
        let ring_top = row.ring.y2 + dy;
        let nu = up_nets.len() as i64;
        channels.push(Channel {
            tracks: up_nets
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    (
                        n.clone(),
                        ring_top + TRACK_GAP + TRACK_W / 2 + i as i64 * pitch,
                    )
                })
                .collect(),
            terminals: row.up.iter().map(|t| moved(t, dx, dy)).collect(),
            down: false,
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
    let spine_x: BTreeMap<&str, i64> = spine
        .iter()
        .enumerate()
        .map(|(j, n)| {
            (
                n.as_str(),
                ring_right + SPINE_GAP + TRACK_W / 2 + j as i64 * pitch,
            )
        })
        .collect();

    let mut spine_span: BTreeMap<&str, (i64, i64)> = BTreeMap::new();
    let mut first_track: BTreeMap<String, (i64, i64)> = BTreeMap::new();
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
            if let Some(&sx) = spine_x.get(net.as_str()) {
                x2 = x2.max(sx + VIA / 2 + VIA_ENDCAP);
                plan.add(Layer::Via3, Rect::centred(sx, *ty, VIA, VIA), Some(net));
                let span = spine_span.entry(net.as_str()).or_insert((*ty, *ty));
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
            first_track.entry(net.clone()).or_insert((x1, *ty));
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
    for (net, (lo, hi)) in &spine_span {
        let sx = spine_x[net];
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
    for port in ports {
        if let Some(&(x1, ty)) = first_track.get(port) {
            plan.labels.push(Label {
                layer: Layer::Metal3,
                text: port.clone(),
                x: x1 + TRACK_W / 2,
                y: ty,
                pin: true,
            });
        }
    }
    (plan, spine)
}
